//! Long-lived per-engine digest index and snapshot/tail session source.
//!
//! The index lifetime is independent of load-balancer sessions. A session is
//! registered under the same short lock used to clone its snapshot boundary,
//! so every later live batch is either represented by the clone or queued on
//! that session's bounded authenticated tail. Snapshot traversal and encoding
//! operate on the clone and never hold the live ingestion lock.

use std::{collections::HashMap, sync::Arc};

use parking_lot::Mutex;
use thiserror::Error;

use crate::{
    digest_index::{DigestIndexError, DigestIndexLimits, DigestKvIndex, SnapshotGroupKey},
    kv_snapshot::{
        AttentionKind, EngineIncarnation, GroupDisposition, GroupMetadata, SnapshotLimits,
        encode_snapshot_with_cancel,
    },
    kv_transport::SequencedBatch,
    snapshot_digest_delta::{DigestDeltaSummary, SnapshotDigestDeltaAdapter},
    snapshot_producer::{
        ProducerIdentity, ProducerSnapshot, ProducerTailEvent, SnapshotBuildFuture,
        SnapshotProducerCancellation, SnapshotProducerSource, SnapshotProducerSourceError,
        SnapshotProducerSourcePhase, SnapshotProducerSourceStatus, SnapshotTailPublisher,
    },
};

const MAX_ACTIVE_SESSIONS: usize = 2;

#[derive(Clone, Debug)]
pub struct CompanionIndexSourceConfig {
    pub group: GroupMetadata,
    pub index_limits: DigestIndexLimits,
    pub snapshot_limits: SnapshotLimits,
    pub max_active_sessions: usize,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CompanionIndexStatus {
    pub phase: SnapshotProducerSourcePhase,
    pub ready: bool,
    pub watermark: Option<u64>,
    pub companion_generation: u64,
    pub active_sessions: usize,
    pub indexed_blocks: usize,
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum CompanionIndexSourceError {
    #[error("companion index source configuration is invalid")]
    InvalidConfig,
    #[error("companion index source is not authoritative")]
    NotReady,
    #[error("companion replay state is invalid")]
    InvalidReplay,
    #[error("companion generation was exhausted")]
    GenerationExhausted,
    #[error("companion digest index update failed")]
    Index,
}

impl From<DigestIndexError> for CompanionIndexSourceError {
    fn from(_: DigestIndexError) -> Self {
        Self::Index
    }
}

struct ReplayState {
    index: DigestKvIndex,
    last_sequence: Option<u64>,
}

struct SourceState {
    identity: ProducerIdentity,
    index: DigestKvIndex,
    replay: Option<ReplayState>,
    watermark: Option<u64>,
    ready: bool,
    fenced: bool,
    builds_in_progress: usize,
    next_session_id: u64,
    subscribers: HashMap<u64, SnapshotTailPublisher>,
}

/// Concrete, engine-neutral owner of one long-lived per-engine digest index.
///
/// A runtime adapter feeds already-qualified [`SequencedBatch`] values from
/// [`crate::kv_transport::ZmqKvEventSource`]. The source retains only compact
/// commitments and the original bounded payload needed by active tail
/// sessions; it never stores or logs raw token vectors.
pub struct CompanionIndexSource {
    config: CompanionIndexSourceConfig,
    delta: SnapshotDigestDeltaAdapter,
    state: Arc<Mutex<SourceState>>,
}

impl CompanionIndexSource {
    /// Create an unready source with an empty initial replay stage.
    ///
    /// Live subscription should already be established before constructing
    /// and feeding this source. Apply the full replay, then call
    /// [`Self::finish_replay`] before accepting snapshot sessions.
    ///
    /// # Errors
    ///
    /// Rejects unsafe session cardinality, unsupported group metadata, a zero
    /// generation, or an invalid digest secret.
    pub fn new(
        config: CompanionIndexSourceConfig,
        engine_incarnation: EngineIncarnation,
        companion_generation: u64,
        digest_secret: &[u8],
    ) -> Result<Self, CompanionIndexSourceError> {
        if companion_generation == 0
            || config.max_active_sessions == 0
            || config.max_active_sessions > MAX_ACTIVE_SESSIONS
            || config.group.block_size == 0
            || config.group.disposition != GroupDisposition::Indexed
            || !matches!(
                config.group.attention_kind,
                AttentionKind::FullAttention
                    | AttentionKind::MlaAttention
                    | AttentionKind::SinkFullAttention
            )
        {
            return Err(CompanionIndexSourceError::InvalidConfig);
        }
        let index = DigestKvIndex::from_secret(config.index_limits, digest_secret)?;
        let replay_index = index.clone();
        let identity = ProducerIdentity {
            engine_incarnation,
            digest_key_id: index.digest_key_id(),
            companion_generation,
        };
        let delta = SnapshotDigestDeltaAdapter::new(
            SnapshotGroupKey {
                data_parallel_rank: config.group.data_parallel_rank,
                group_idx: config.group.group_idx,
            },
            crate::kv_wire::KvWireLimits::default(),
        );
        Ok(Self {
            config,
            delta,
            state: Arc::new(Mutex::new(SourceState {
                identity,
                index,
                replay: Some(ReplayState {
                    index: replay_index,
                    last_sequence: None,
                }),
                watermark: None,
                ready: false,
                fenced: false,
                builds_in_progress: 0,
                next_session_id: 1,
                subscribers: HashMap::new(),
            })),
        })
    }

    #[must_use]
    pub fn status(&self) -> CompanionIndexStatus {
        let state = self.state.lock();
        let indexed_blocks = state.replay.as_ref().map_or_else(
            || state.index.stats().nodes,
            |replay| replay.index.stats().nodes,
        );
        CompanionIndexStatus {
            phase: source_phase(&state),
            ready: state.ready,
            watermark: state.watermark,
            companion_generation: state.identity.companion_generation,
            active_sessions: state.subscribers.len(),
            indexed_blocks,
        }
    }

    /// Apply one sparse, monotonically increasing replay batch to private
    /// staging state. Replay never publishes tail events.
    ///
    /// # Errors
    ///
    /// Fails closed on a missing rebuild stage, non-increasing replay input,
    /// or digest-index application failure.
    pub fn apply_replay(
        &self,
        batch: &SequencedBatch,
    ) -> Result<DigestDeltaSummary, CompanionIndexSourceError> {
        let mut state = self.state.lock();
        apply_replay_locked(&self.delta, &mut state, batch)
    }

    /// Apply replay only while the caller still owns the expected companion
    /// generation. This closes the cancellation race where an abandoned
    /// blocking replay worker observes shutdown after a fresh rebuild started.
    ///
    /// # Errors
    ///
    /// Returns `InvalidReplay` without touching the current stage when the
    /// generation changed, and otherwise has the same behavior as
    /// [`Self::apply_replay`].
    pub fn apply_replay_for_generation(
        &self,
        expected_generation: u64,
        batch: &SequencedBatch,
    ) -> Result<DigestDeltaSummary, CompanionIndexSourceError> {
        let mut state = self.state.lock();
        if state.identity.companion_generation != expected_generation {
            return Err(CompanionIndexSourceError::InvalidReplay);
        }
        apply_replay_locked(&self.delta, &mut state, batch)
    }

    /// Atomically publish a completed replay through the exact `through`
    /// watermark. Sparse replay sequences are valid because scheduler steps
    /// with no KV mutation emit no event batch.
    ///
    /// # Errors
    ///
    /// Rejects and discards a stage unless its final observed event exactly
    /// matches the claimed authoritative watermark.
    pub fn finish_replay(&self, through: u64) -> Result<(), CompanionIndexSourceError> {
        let mut state = self.state.lock();
        let replay = state
            .replay
            .take()
            .ok_or(CompanionIndexSourceError::InvalidReplay)?;
        if replay.last_sequence != Some(through) {
            state.fenced = true;
            return Err(CompanionIndexSourceError::InvalidReplay);
        }
        state.index = replay.index;
        state.watermark = Some(through);
        state.ready = true;
        state.fenced = false;
        Ok(())
    }

    /// Apply and fan out one live batch after the authoritative watermark.
    /// Duplicate deliveries are ignored. Real vLLM watermarks are sparse, so
    /// every strictly newer batch is accepted; the process-level transport
    /// fence must recover actual loss before feeding this source. An index
    /// failure fences the generation and starts a fresh empty replay stage.
    ///
    /// # Errors
    ///
    /// Returns `NotReady` while rebuilding and `Index` after fencing an
    /// invalid live transition.
    pub fn apply_live(
        &self,
        batch: &SequencedBatch,
    ) -> Result<DigestDeltaSummary, CompanionIndexSourceError> {
        let mut state = self.state.lock();
        if !state.ready {
            return Err(CompanionIndexSourceError::NotReady);
        }
        let watermark = state.watermark.ok_or(CompanionIndexSourceError::NotReady)?;
        if batch.sequence <= watermark {
            return Ok(DigestDeltaSummary::default());
        }
        let Ok(summary) = self.delta.apply_batch(&mut state.index, &batch.batch) else {
            fence_locked(&mut state, None)?;
            return Err(CompanionIndexSourceError::Index);
        };
        state.watermark = Some(batch.sequence);
        let identity = state.identity.clone();
        state.subscribers.retain(|_, publisher| {
            let delivered = publisher
                .try_send(ProducerTailEvent::Batch {
                    identity: identity.clone(),
                    event_watermark: batch.sequence,
                    payload: batch.payload.clone(),
                })
                .is_ok();
            if !delivered {
                publisher.revoke();
            }
            delivered
        });
        Ok(summary)
    }

    /// Fence all sessions and start a fresh replay generation after transport
    /// disconnect, reconnect, or an attested engine incarnation change.
    ///
    /// # Errors
    ///
    /// Returns `GenerationExhausted` rather than wrapping an identity fence.
    pub fn begin_rebuild(
        &self,
        engine_incarnation: Option<EngineIncarnation>,
    ) -> Result<(), CompanionIndexSourceError> {
        fence_locked(&mut self.state.lock(), engine_incarnation)
    }
}

impl SnapshotProducerSource for CompanionIndexSource {
    fn status(&self) -> SnapshotProducerSourceStatus {
        let status = self.status();
        SnapshotProducerSourceStatus {
            phase: status.phase,
            ready: status.ready,
            watermark_present: status.watermark.is_some(),
            active_sessions: status.active_sessions,
            indexed_blocks: status.indexed_blocks,
        }
    }

    fn start(
        &self,
        publisher: SnapshotTailPublisher,
        cancellation: SnapshotProducerCancellation,
    ) -> Result<SnapshotBuildFuture, SnapshotProducerSourceError> {
        let (session_id, index, identity, watermark) = {
            let mut state = self.state.lock();
            if !state.ready || cancellation.is_cancelled() {
                return Err(if cancellation.is_cancelled() {
                    SnapshotProducerSourceError::Cancelled
                } else {
                    SnapshotProducerSourceError::Failed
                });
            }
            if state.subscribers.len() >= self.config.max_active_sessions {
                return Err(SnapshotProducerSourceError::Failed);
            }
            let session_id = state.next_session_id;
            state.next_session_id = state
                .next_session_id
                .checked_add(1)
                .ok_or(SnapshotProducerSourceError::Failed)?;
            state.subscribers.insert(session_id, publisher);
            state.builds_in_progress = state.builds_in_progress.saturating_add(1);
            (
                session_id,
                state.index.clone(),
                state.identity.clone(),
                state.watermark.ok_or(SnapshotProducerSourceError::Failed)?,
            )
        };

        let cleanup_state = Arc::clone(&self.state);
        let mut cleanup_cancellation = cancellation.clone();
        tokio::spawn(async move {
            cleanup_cancellation.cancelled().await;
            let mut state = cleanup_state.lock();
            state.subscribers.remove(&session_id);
        });

        let state = Arc::clone(&self.state);
        let build_guard = SourceBuildGuard {
            state: Arc::clone(&state),
        };
        let group = self.config.group.clone();
        let snapshot_limits = self.config.snapshot_limits;
        let build_identity = identity.clone();
        let build_cancellation = cancellation.clone();
        Ok(Box::pin(async move {
            let frame = tokio::task::spawn_blocking(move || {
                let body = index
                    .export_snapshot_with_cancel(
                        build_identity.engine_incarnation.clone(),
                        watermark,
                        group,
                        || build_cancellation.is_cancelled(),
                    )
                    .map_err(|error| map_build_error(&error))?;
                encode_snapshot_with_cancel(&body, snapshot_limits, || {
                    build_cancellation.is_cancelled()
                })
                .map_err(|error| {
                    if matches!(error, crate::kv_snapshot::SnapshotError::Cancelled) {
                        SnapshotProducerSourceError::Cancelled
                    } else {
                        SnapshotProducerSourceError::Failed
                    }
                })
            })
            .await
            .map_err(|_| SnapshotProducerSourceError::Failed)??;

            drop(build_guard);
            let mut current = state.lock();
            if cancellation.is_cancelled() || !current.ready || current.identity != identity {
                current.subscribers.remove(&session_id);
                return Err(SnapshotProducerSourceError::Cancelled);
            }
            let caught_up = current
                .watermark
                .ok_or(SnapshotProducerSourceError::Failed)?;
            let publisher = current
                .subscribers
                .get(&session_id)
                .ok_or(SnapshotProducerSourceError::Cancelled)?;
            if let Err(error) = publisher.try_send(ProducerTailEvent::CaughtUp {
                identity: identity.clone(),
                event_watermark: caught_up,
            }) {
                publisher.revoke();
                current.subscribers.remove(&session_id);
                return Err(error);
            }
            Ok(ProducerSnapshot {
                identity,
                watermark,
                frame,
            })
        }))
    }
}

struct SourceBuildGuard {
    state: Arc<Mutex<SourceState>>,
}

impl Drop for SourceBuildGuard {
    fn drop(&mut self) {
        let mut state = self.state.lock();
        state.builds_in_progress = state.builds_in_progress.saturating_sub(1);
    }
}

fn apply_replay_locked(
    delta: &SnapshotDigestDeltaAdapter,
    state: &mut SourceState,
    batch: &SequencedBatch,
) -> Result<DigestDeltaSummary, CompanionIndexSourceError> {
    let replay = state
        .replay
        .as_ref()
        .ok_or(CompanionIndexSourceError::InvalidReplay)?;
    if replay
        .last_sequence
        .is_some_and(|previous| batch.sequence <= previous)
    {
        state.replay = None;
        state.fenced = true;
        return Err(CompanionIndexSourceError::InvalidReplay);
    }
    let result = delta.apply_batch(
        &mut state
            .replay
            .as_mut()
            .ok_or(CompanionIndexSourceError::InvalidReplay)?
            .index,
        &batch.batch,
    );
    let Ok(summary) = result else {
        state.replay = None;
        state.fenced = true;
        return Err(CompanionIndexSourceError::Index);
    };
    let replay = state
        .replay
        .as_mut()
        .ok_or(CompanionIndexSourceError::InvalidReplay)?;
    replay.last_sequence = Some(batch.sequence);
    state.fenced = false;
    Ok(summary)
}

fn map_build_error(error: &DigestIndexError) -> SnapshotProducerSourceError {
    if *error == DigestIndexError::Cancelled {
        SnapshotProducerSourceError::Cancelled
    } else {
        SnapshotProducerSourceError::Failed
    }
}

fn fence_locked(
    state: &mut SourceState,
    engine_incarnation: Option<EngineIncarnation>,
) -> Result<(), CompanionIndexSourceError> {
    let Some(next_generation) = state.identity.companion_generation.checked_add(1) else {
        state.ready = false;
        state.fenced = true;
        state.builds_in_progress = 0;
        state.watermark = None;
        state.index.clear();
        state.replay = None;
        let identity = state.identity.clone();
        for publisher in state.subscribers.values() {
            let _ = publisher.try_send(ProducerTailEvent::Disconnect(identity.clone()));
            publisher.revoke();
        }
        state.subscribers.clear();
        return Err(CompanionIndexSourceError::GenerationExhausted);
    };
    state.ready = false;
    state.fenced = true;
    state.builds_in_progress = 0;
    state.watermark = None;
    state.index.clear();
    state.identity.companion_generation = next_generation;
    if let Some(engine_incarnation) = engine_incarnation {
        state.identity.engine_incarnation = engine_incarnation;
    }
    let identity = state.identity.clone();
    for publisher in state.subscribers.values() {
        let _ = publisher.try_send(ProducerTailEvent::IdentityChanged(identity.clone()));
        publisher.revoke();
    }
    state.subscribers.clear();
    state.replay = Some(ReplayState {
        index: state.index.clone(),
        last_sequence: None,
    });
    Ok(())
}

fn source_phase(state: &SourceState) -> SnapshotProducerSourcePhase {
    if state.fenced {
        SnapshotProducerSourcePhase::Fenced
    } else if state.builds_in_progress != 0 {
        SnapshotProducerSourcePhase::Building
    } else if state.ready {
        SnapshotProducerSourcePhase::Ready
    } else {
        SnapshotProducerSourcePhase::Replay
    }
}

#[cfg(test)]
mod tests {
    use std::{sync::atomic::Ordering, time::Duration};

    use bytes::Bytes;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::UnixStream,
        task::JoinHandle,
        time::timeout,
    };

    use super::*;
    use crate::{
        block_digest::BlockDigester,
        kv_snapshot::{
            DigestAlgorithm, DigestSpec, ResetScope, SnapshotExpectations, decode_snapshot,
        },
        kv_wire::{BlockStored, ExternalBlockHash, KvEvent, KvEventBatch},
        snapshot_producer::{
            ProducerTailEvent, SnapshotProducer, SnapshotProducerConfig, SnapshotProducerError,
            SnapshotProducerSourceError, test_tail_channel,
        },
        snapshot_session::{
            SNAPSHOT_RESPONSE_LENGTH_PREFIX_BYTES, SnapshotSessionChallenge, SnapshotSessionLimits,
            SnapshotSessionSecret, authenticated_snapshot_frame_length, encode_client_hello,
        },
        snapshot_tail_wire::{TAIL_FRAME_LENGTH_PREFIX_BYTES, TailWireLimits, tail_frame_length},
    };

    const SECRET: [u8; 32] = *b"0123456789abcdef0123456789abcdef";
    const SESSION_SECRET: [u8; 32] = *b"snapshot-session-secret-32-byte!";
    const CHALLENGE: SnapshotSessionChallenge = SnapshotSessionChallenge::new([0x51; 32]);

    fn incarnation(name: &str) -> EngineIncarnation {
        EngineIncarnation {
            engine_id: name.to_owned(),
            model_revision: "revision".to_owned(),
            image_digest: "sha256:image".to_owned(),
            process_started_unix_ns: 42,
            attestation_sha256: vec![7; 32],
        }
    }

    fn config(max_active_sessions: usize) -> CompanionIndexSourceConfig {
        CompanionIndexSourceConfig {
            group: GroupMetadata {
                data_parallel_rank: 0,
                group_idx: 0,
                attention_kind: AttentionKind::MlaAttention,
                disposition: GroupDisposition::Indexed,
                block_size: 4,
            },
            index_limits: DigestIndexLimits::default(),
            snapshot_limits: SnapshotLimits::default(),
            max_active_sessions,
        }
    }

    fn source(max_active_sessions: usize) -> Arc<CompanionIndexSource> {
        Arc::new(
            CompanionIndexSource::new(
                config(max_active_sessions),
                incarnation("engine-a"),
                1,
                &SECRET,
            )
            .unwrap(),
        )
    }

    fn store_batch(sequence: u64, hash: u64, tokens: &[u32]) -> SequencedBatch {
        SequencedBatch {
            sequence,
            payload: Bytes::from(vec![u8::try_from(sequence).unwrap()]),
            batch: KvEventBatch {
                timestamp: 1.0,
                data_parallel_rank: Some(0),
                events: vec![KvEvent::BlockStored(BlockStored {
                    block_hashes: vec![ExternalBlockHash::Unsigned(hash)],
                    parent_block_hash: None,
                    token_ids: tokens.to_vec(),
                    block_size: tokens.len(),
                    group_idx: Some(0),
                    kv_cache_spec_kind: Some("mla_attention".to_owned()),
                    kv_cache_spec_sliding_window: None,
                    medium: Some("GPU".to_owned()),
                    locality: Some("LOCAL".to_owned()),
                    lora_name: None,
                    cache_namespace: None,
                    has_extra_keys: false,
                })],
            },
        }
    }

    fn clear_batch(sequence: u64) -> SequencedBatch {
        SequencedBatch {
            sequence,
            payload: Bytes::from(vec![u8::try_from(sequence).unwrap()]),
            batch: KvEventBatch {
                timestamp: 1.0,
                data_parallel_rank: Some(0),
                events: vec![KvEvent::AllBlocksCleared],
            },
        }
    }

    fn empty_batch(sequence: u64) -> SequencedBatch {
        SequencedBatch {
            sequence,
            payload: Bytes::from(vec![u8::try_from(sequence).unwrap()]),
            batch: KvEventBatch {
                timestamp: 1.0,
                data_parallel_rank: Some(0),
                events: Vec::new(),
            },
        }
    }

    fn large_store_batch(sequence: u64, blocks: u32) -> SequencedBatch {
        SequencedBatch {
            sequence,
            payload: Bytes::from(vec![u8::try_from(sequence).unwrap()]),
            batch: KvEventBatch {
                timestamp: 1.0,
                data_parallel_rank: Some(0),
                events: vec![KvEvent::BlockStored(BlockStored {
                    block_hashes: (1..=blocks)
                        .map(|hash| ExternalBlockHash::Unsigned(u64::from(hash)))
                        .collect(),
                    parent_block_hash: None,
                    token_ids: (1..=blocks).collect(),
                    block_size: 1,
                    group_idx: Some(0),
                    kv_cache_spec_kind: Some("mla_attention".to_owned()),
                    kv_cache_spec_sliding_window: None,
                    medium: Some("GPU".to_owned()),
                    locality: Some("LOCAL".to_owned()),
                    lora_name: None,
                    cache_namespace: None,
                    has_extra_keys: false,
                })],
            },
        }
    }

    async fn read_frame(
        stream: &mut UnixStream,
        prefix_bytes: usize,
        frame_length: impl FnOnce(&[u8]) -> usize,
    ) {
        let mut frame = vec![0_u8; prefix_bytes];
        stream.read_exact(&mut frame).await.unwrap();
        let total = frame_length(&frame);
        frame.resize(total, 0);
        stream.read_exact(&mut frame[prefix_bytes..]).await.unwrap();
    }

    async fn start_concrete_handler(
        source: Arc<CompanionIndexSource>,
    ) -> (UnixStream, JoinHandle<Result<(), SnapshotProducerError>>) {
        let (server, mut client) = UnixStream::pair().unwrap();
        let uid = server.peer_cred().unwrap().uid();
        let producer = SnapshotProducer::new(
            SnapshotProducerConfig {
                expected_peer_uid: uid,
                snapshot_timeout: Duration::from_secs(2),
                tail_idle_timeout: Duration::from_secs(2),
                session_limits: SnapshotSessionLimits::default(),
                tail_limits: TailWireLimits::default(),
                tail_queue_capacity: 4,
                tail_queue_max_bytes: 8 * 1024 * 1024,
            },
            Arc::new(SnapshotSessionSecret::new(SESSION_SECRET)),
            source,
        )
        .unwrap();
        let task = tokio::spawn(async move { producer.handle(server).await });
        let hello = encode_client_hello(
            CHALLENGE,
            &SnapshotSessionSecret::new(SESSION_SECRET),
            SnapshotSessionLimits::default(),
        )
        .unwrap();
        client.write_all(&hello).await.unwrap();
        read_frame(
            &mut client,
            SNAPSHOT_RESPONSE_LENGTH_PREFIX_BYTES,
            |prefix| {
                authenticated_snapshot_frame_length(prefix, SnapshotSessionLimits::default())
                    .unwrap()
            },
        )
        .await;
        read_frame(&mut client, TAIL_FRAME_LENGTH_PREFIX_BYTES, |prefix| {
            tail_frame_length(prefix, TailWireLimits::default()).unwrap()
        })
        .await;
        (client, task)
    }

    async fn assert_prompt_revocation(
        mut client: UnixStream,
        task: JoinHandle<Result<(), SnapshotProducerError>>,
    ) {
        let result = timeout(Duration::from_millis(250), task)
            .await
            .expect("source revocation must stop the handler promptly")
            .unwrap();
        assert!(matches!(
            result,
            Err(SnapshotProducerError::Source(
                SnapshotProducerSourceError::Cancelled
            ))
        ));
        let mut byte = [0_u8; 1];
        assert_eq!(
            timeout(Duration::from_millis(250), client.read(&mut byte))
                .await
                .expect("revoked peer must observe prompt connection close")
                .unwrap(),
            0,
            "queued stale tail data was drained before revocation"
        );
    }

    #[tokio::test]
    async fn session_is_registered_before_snapshot_build_and_tail_is_ordered() {
        let source = source(2);
        source.apply_replay(&store_batch(7, 70, &[1, 2])).unwrap();
        source.apply_replay(&empty_batch(10)).unwrap();
        source.finish_replay(10).unwrap();
        let (publisher, cancellation, mut receiver, _signal, _cancelled) =
            test_tail_channel(4, 1024);
        let build = source.start(publisher, cancellation).unwrap();

        source.apply_live(&store_batch(11, 71, &[3, 4])).unwrap();
        let snapshot = build.await.unwrap();
        assert_eq!(snapshot.watermark, 10);
        assert_eq!(snapshot.identity.companion_generation, 1);

        let digest = DigestSpec {
            algorithm: DigestAlgorithm::HmacSha256V1,
            key_id: BlockDigester::new(SECRET).key_id().to_vec(),
            digest_bytes: 32,
        };
        let body = decode_snapshot(
            &snapshot.frame,
            SnapshotLimits::default(),
            SnapshotExpectations {
                engine_incarnation: &incarnation("engine-a"),
                reset_scope: ResetScope::full_engine(),
                digest: &digest,
            },
        )
        .unwrap();
        assert_eq!(body.watermark, 10);
        assert_eq!(body.records.len(), 1);

        assert!(matches!(
            receiver.recv().await.unwrap().event,
            ProducerTailEvent::Batch {
                event_watermark: 11,
                payload,
                ..
            } if payload.as_ref() == [11]
        ));
        assert!(matches!(
            receiver.recv().await.unwrap().event,
            ProducerTailEvent::CaughtUp {
                event_watermark: 11,
                ..
            }
        ));
        assert_eq!(source.status().indexed_blocks, 2);
    }

    #[tokio::test]
    async fn session_cancellation_does_not_stop_long_lived_index() {
        let source = source(1);
        source.apply_replay(&empty_batch(10)).unwrap();
        source.finish_replay(10).unwrap();
        let (publisher, cancellation, _receiver, signal, cancelled) = test_tail_channel(2, 1024);
        let build = source.start(publisher, cancellation).unwrap();
        let _snapshot = build.await.unwrap();
        cancelled.store(true, Ordering::Release);
        signal.send(true).unwrap();
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        assert_eq!(source.status().active_sessions, 0);

        source.apply_live(&store_batch(11, 71, &[3, 4])).unwrap();
        let status = source.status();
        assert!(status.ready);
        assert_eq!(status.watermark, Some(11));
        assert_eq!(status.indexed_blocks, 1);
    }

    #[tokio::test]
    async fn queue_backpressure_drops_only_slow_session() {
        let source = source(1);
        source.apply_replay(&empty_batch(10)).unwrap();
        source.finish_replay(10).unwrap();
        let (publisher, cancellation, _receiver, _signal, _cancelled) = test_tail_channel(1, 1024);
        let _build = source.start(publisher, cancellation).unwrap();
        source.apply_live(&store_batch(11, 71, &[1, 2])).unwrap();
        source.apply_live(&store_batch(12, 72, &[3, 4])).unwrap();

        let status = source.status();
        assert!(status.ready);
        assert_eq!(status.active_sessions, 0);
        assert_eq!(status.watermark, Some(12));
        assert_eq!(status.indexed_blocks, 2);
    }

    #[tokio::test]
    async fn queue_byte_overflow_revokes_before_stale_tail_is_drained() {
        let source = source(1);
        source.apply_replay(&empty_batch(10)).unwrap();
        source.finish_replay(10).unwrap();
        let (client, task) = start_concrete_handler(Arc::clone(&source)).await;

        let mut first = store_batch(11, 71, &[1, 2]);
        first.payload = Bytes::from(vec![0x11; 7 * 1024 * 1024]);
        source.apply_live(&first).unwrap();
        tokio::task::yield_now().await;
        let mut second = store_batch(12, 72, &[3, 4]);
        second.payload = Bytes::from(vec![0x12; 7 * 1024 * 1024]);
        source.apply_live(&second).unwrap();
        let mut third = store_batch(13, 73, &[5, 6]);
        third.payload = Bytes::from(vec![0x13; 7 * 1024 * 1024]);
        source.apply_live(&third).unwrap();

        let result = timeout(Duration::from_millis(250), task)
            .await
            .expect("byte overflow must stop a blocked writer promptly")
            .unwrap();
        assert!(matches!(
            result,
            Err(SnapshotProducerError::Source(
                SnapshotProducerSourceError::Cancelled
            ))
        ));
        let mut client = client;
        let mut received = Vec::new();
        timeout(
            Duration::from_millis(250),
            client.read_to_end(&mut received),
        )
        .await
        .expect("revoked peer must close promptly")
        .unwrap();
        assert!(
            !received.windows(64).any(|window| window == [0x12; 64]),
            "queued stale tail payload was drained after byte-budget revocation"
        );
        let status = source.status();
        assert!(status.ready);
        assert_eq!(status.watermark, Some(13));
        assert_eq!(status.active_sessions, 0);
    }

    #[tokio::test]
    async fn rebuild_revokes_handler_before_queued_tail_is_drained() {
        let source = source(1);
        source.apply_replay(&empty_batch(10)).unwrap();
        source.finish_replay(10).unwrap();
        let (client, task) = start_concrete_handler(Arc::clone(&source)).await;

        source.apply_live(&store_batch(11, 71, &[1, 2])).unwrap();
        source.begin_rebuild(None).unwrap();

        assert_prompt_revocation(client, task).await;
        let status = source.status();
        assert!(!status.ready);
        assert_eq!(status.watermark, None);
        assert_eq!(status.active_sessions, 0);
    }

    #[tokio::test]
    async fn two_sessions_receive_independent_tail_and_third_is_rejected() {
        let source = source(2);
        source.apply_replay(&empty_batch(10)).unwrap();
        source.finish_replay(10).unwrap();
        let (publisher_a, cancellation_a, mut receiver_a, _signal_a, _cancelled_a) =
            test_tail_channel(3, 1024);
        let (publisher_b, cancellation_b, mut receiver_b, _signal_b, _cancelled_b) =
            test_tail_channel(3, 1024);
        let _build_a = source.start(publisher_a, cancellation_a).unwrap();
        let _build_b = source.start(publisher_b, cancellation_b).unwrap();
        let (publisher_c, cancellation_c, _receiver_c, _signal_c, _cancelled_c) =
            test_tail_channel(3, 1024);
        assert!(matches!(
            source.start(publisher_c, cancellation_c),
            Err(SnapshotProducerSourceError::Failed)
        ));

        source.apply_live(&store_batch(11, 71, &[1, 2])).unwrap();
        for receiver in [&mut receiver_a, &mut receiver_b] {
            assert!(matches!(
                receiver.recv().await.unwrap().event,
                ProducerTailEvent::Batch {
                    event_watermark: 11,
                    ref payload,
                    ..
                } if payload.as_ref() == [11]
            ));
        }
        assert_eq!(source.status().active_sessions, 2);
    }

    #[tokio::test]
    async fn sparse_live_progress_is_valid_and_explicit_rebuild_fences_sessions() {
        let source = source(1);
        source.apply_replay(&empty_batch(10)).unwrap();
        source.finish_replay(10).unwrap();
        let (publisher, cancellation, mut receiver, _signal, _cancelled) =
            test_tail_channel(2, 1024);
        let _build = source.start(publisher, cancellation).unwrap();
        source.apply_live(&store_batch(12, 72, &[3, 4])).unwrap();
        assert!(matches!(
            receiver.recv().await.unwrap().event,
            ProducerTailEvent::Batch {
                event_watermark: 12,
                ..
            }
        ));
        assert!(source.status().ready);

        source.begin_rebuild(None).unwrap();
        assert!(matches!(
            receiver.recv().await.unwrap().event,
            ProducerTailEvent::IdentityChanged(ProducerIdentity {
                companion_generation: 2,
                ..
            })
        ));
        let fenced = source.status();
        assert!(!fenced.ready);
        assert_eq!(fenced.indexed_blocks, 0);
        assert_eq!(fenced.active_sessions, 0);

        source.apply_replay(&store_batch(20, 80, &[8, 9])).unwrap();
        source.apply_replay(&empty_batch(25)).unwrap();
        source.finish_replay(25).unwrap();
        let recovered = source.status();
        assert!(recovered.ready);
        assert_eq!(recovered.watermark, Some(25));
        assert_eq!(recovered.companion_generation, 2);
    }

    #[tokio::test]
    async fn attested_incarnation_change_fences_old_session() {
        let source = source(1);
        source.apply_replay(&empty_batch(10)).unwrap();
        source.finish_replay(10).unwrap();
        let (publisher, cancellation, mut receiver, _signal, _cancelled) =
            test_tail_channel(2, 1024);
        let _build = source.start(publisher, cancellation).unwrap();
        source.begin_rebuild(Some(incarnation("engine-b"))).unwrap();
        assert!(matches!(
            receiver.recv().await.unwrap().event,
            ProducerTailEvent::IdentityChanged(ProducerIdentity {
                engine_incarnation: EngineIncarnation { ref engine_id, .. },
                companion_generation: 2,
                ..
            }) if engine_id == "engine-b"
        ));
        let status = source.status();
        assert!(!status.ready);
        assert_eq!(status.active_sessions, 0);
    }

    #[tokio::test]
    async fn generation_exhaustion_permanently_fences_state_and_sessions() {
        let source = Arc::new(
            CompanionIndexSource::new(config(1), incarnation("engine-a"), u64::MAX, &SECRET)
                .unwrap(),
        );
        source.apply_replay(&empty_batch(10)).unwrap();
        source.finish_replay(10).unwrap();
        let (publisher, cancellation, mut receiver, _signal, _cancelled) =
            test_tail_channel(2, 1024);
        let _build = source.start(publisher, cancellation).unwrap();

        assert_eq!(
            source.begin_rebuild(None),
            Err(CompanionIndexSourceError::GenerationExhausted)
        );
        assert!(matches!(
            receiver.recv().await.unwrap().event,
            ProducerTailEvent::Disconnect(ProducerIdentity {
                companion_generation: u64::MAX,
                ..
            })
        ));
        let status = source.status();
        assert!(!status.ready);
        assert_eq!(status.watermark, None);
        assert_eq!(status.indexed_blocks, 0);
        assert_eq!(status.active_sessions, 0);
        assert_eq!(
            source.apply_replay(&empty_batch(11)),
            Err(CompanionIndexSourceError::InvalidReplay)
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn large_snapshot_build_yields_reactor_and_observes_cancellation() {
        let source = source(1);
        source.apply_replay(&large_store_batch(10, 50_000)).unwrap();
        source.finish_replay(10).unwrap();
        let (publisher, cancellation, _receiver, signal, cancelled) = test_tail_channel(2, 1024);
        let mut build = source.start(publisher, cancellation).unwrap();
        let (heartbeat_sender, heartbeat) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            let _ = heartbeat_sender.send(());
        });

        tokio::select! {
            result = &mut build => panic!("blocking snapshot build completed on reactor: {result:?}"),
            result = heartbeat => result.unwrap(),
        }
        cancelled.store(true, Ordering::Release);
        signal.send(true).unwrap();
        assert!(matches!(
            tokio::time::timeout(std::time::Duration::from_secs(2), &mut build)
                .await
                .unwrap(),
            Err(SnapshotProducerSourceError::Cancelled)
        ));
        tokio::task::yield_now().await;
        assert!(source.status().ready);
        assert_eq!(source.status().active_sessions, 0);
        assert_eq!(source.status().indexed_blocks, 50_000);
    }

    #[test]
    fn clear_event_resets_index_without_losing_authority() {
        let source = source(1);
        source.apply_replay(&store_batch(7, 70, &[1, 2])).unwrap();
        source.apply_replay(&empty_batch(10)).unwrap();
        source.finish_replay(10).unwrap();
        let summary = source.apply_live(&clear_batch(11)).unwrap();
        assert_eq!(summary.clear_events, 1);
        let status = source.status();
        assert!(status.ready);
        assert_eq!(status.watermark, Some(11));
        assert_eq!(status.indexed_blocks, 0);
    }

    #[test]
    fn incomplete_or_regressing_replay_cannot_be_published() {
        let source = source(1);
        source.apply_replay(&empty_batch(7)).unwrap();
        assert_eq!(
            source.finish_replay(10),
            Err(CompanionIndexSourceError::InvalidReplay)
        );
        assert_eq!(
            source.apply_replay(&empty_batch(10)),
            Err(CompanionIndexSourceError::InvalidReplay)
        );

        source.begin_rebuild(None).unwrap();
        source.apply_replay(&empty_batch(10)).unwrap();
        assert_eq!(
            source.apply_replay(&empty_batch(10)),
            Err(CompanionIndexSourceError::InvalidReplay)
        );
        assert_eq!(
            source.finish_replay(10),
            Err(CompanionIndexSourceError::InvalidReplay)
        );
        assert!(!source.status().ready);
    }

    #[tokio::test]
    async fn status_tracks_replay_build_ready_and_fenced_without_identity() {
        let source = source(1);
        assert_eq!(source.status().phase, SnapshotProducerSourcePhase::Replay);
        assert!(!source.status().ready);
        assert!(source.status().watermark.is_none());

        source.apply_replay(&empty_batch(10)).unwrap();
        source.finish_replay(10).unwrap();
        assert_eq!(source.status().phase, SnapshotProducerSourcePhase::Ready);
        assert!(source.status().ready);
        assert_eq!(source.status().watermark, Some(10));

        let (publisher, cancellation, _receiver, signal, _cancelled) = test_tail_channel(2, 1_024);
        let build = source.start(publisher, cancellation).unwrap();
        assert_eq!(source.status().phase, SnapshotProducerSourcePhase::Building);
        assert!(source.status().ready);
        assert_eq!(source.status().active_sessions, 1);
        drop(build);
        assert_eq!(source.status().phase, SnapshotProducerSourcePhase::Ready);

        signal.send(true).unwrap();
        timeout(Duration::from_secs(1), async {
            while source.status().active_sessions != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();

        source.begin_rebuild(None).unwrap();
        assert_eq!(source.status().phase, SnapshotProducerSourcePhase::Fenced);
        assert!(!source.status().ready);
        assert!(source.status().watermark.is_none());
        source.apply_replay(&empty_batch(20)).unwrap();
        assert_eq!(source.status().phase, SnapshotProducerSourcePhase::Replay);
        source.finish_replay(20).unwrap();
        assert_eq!(source.status().phase, SnapshotProducerSourcePhase::Ready);
        assert!(source.status().ready);
    }

    #[test]
    fn replay_index_error_discards_private_stage() {
        let source = source(1);
        let mut invalid = store_batch(7, 70, &[1, 2]);
        let KvEvent::BlockStored(stored) = &mut invalid.batch.events[0] else {
            panic!("test batch must contain a store");
        };
        stored.parent_block_hash = Some(ExternalBlockHash::Unsigned(999));
        assert_eq!(
            source.apply_replay(&invalid),
            Err(CompanionIndexSourceError::Index)
        );
        assert_eq!(
            source.finish_replay(7),
            Err(CompanionIndexSourceError::InvalidReplay)
        );
        assert!(!source.status().ready);
    }
}
