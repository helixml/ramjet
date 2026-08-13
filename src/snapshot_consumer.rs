//! Router-side authenticated snapshot and live-tail consumption.
//!
//! This is deliberately not the accepted-stream server from
//! [`crate::snapshot_supervisor`]. The companion owns that admission/runtime
//! side. A future outbound reconnect supervisor gives this consumer an already
//! connected Unix stream, a fresh challenge, and one absolute deadline.
//!
//! One session authenticates its peer before writing protocol bytes, requests
//! one snapshot, builds it privately, then consumes authenticated tail frames
//! for the lifetime of the stream. Publication occurs only on an exact caught-
//! up transition. Timeout, task cancellation, EOF, malformed input, a tail
//! gap, or an apply failure synchronously fences the session in `Drop`.

use std::{
    io,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use parking_lot::Mutex;
use thiserror::Error;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::UnixStream,
    time::{Instant, timeout_at},
};

use crate::{
    block_digest::BlockDigester,
    digest_index::{DigestIndexLimits, DigestKvIndex, SnapshotGroupKey},
    kv_snapshot::{EngineIncarnation, SnapshotLimits},
    kv_wire::KvWireLimits,
    snapshot_actor::{
        ActorAction, SessionEpoch, SnapshotActorError, SnapshotActorIdentity, SnapshotActorLimits,
        SnapshotBootstrapActor,
    },
    snapshot_bootstrap::{
        PreparedSnapshotGeneration, SnapshotBootstrapError,
        prepare_authenticated_snapshot_with_cancel,
    },
    snapshot_digest_delta::SnapshotDigestDeltaAdapter,
    snapshot_session::{
        AuthenticatedSnapshot, SNAPSHOT_DIGEST_KEY_ID_BYTES, SNAPSHOT_RESPONSE_LENGTH_PREFIX_BYTES,
        SnapshotSessionChallenge, SnapshotSessionError, SnapshotSessionExpectations,
        SnapshotSessionLimits, SnapshotSessionSecret, authenticated_snapshot_frame_length,
        decode_authenticated_snapshot, encode_client_hello,
    },
    snapshot_tail::SnapshotTailFenceReason,
    snapshot_tail_wire::{
        TAIL_FRAME_LENGTH_PREFIX_BYTES, TailDirection, TailFrameDecoder, TailSessionExpectations,
        TailSessionKey, TailWireError, TailWireLimits, tail_frame_length,
    },
};

/// The publication state shared by reconnecting consumer sessions and routing.
pub type SharedSnapshotPublication = Arc<Mutex<SnapshotBootstrapActor<DigestKvIndex>>>;

#[derive(Clone, Debug)]
pub struct SnapshotConsumerConfig {
    pub expected_peer_uid: u32,
    pub expected_engine_incarnation: EngineIncarnation,
    pub minimum_snapshot_watermark: u64,
    pub minimum_companion_generation: u64,
    pub group: SnapshotGroupKey,
    pub session_limits: SnapshotSessionLimits,
    pub snapshot_limits: SnapshotLimits,
    pub index_limits: DigestIndexLimits,
    pub tail_limits: TailWireLimits,
    pub event_limits: KvWireLimits,
}

/// One engine's router-side snapshot publication owner.
pub struct SnapshotConsumer {
    config: SnapshotConsumerConfig,
    session_secret: SnapshotSessionSecret,
    digest_secret: [u8; 32],
    digest_key_id: [u8; SNAPSHOT_DIGEST_KEY_ID_BYTES],
    publication: SharedSnapshotPublication,
}

impl SnapshotConsumer {
    /// Construct a consumer and its bounded publication actor.
    ///
    /// # Errors
    ///
    /// Returns [`SnapshotActorError`] when actor limits are invalid.
    pub fn new(
        config: SnapshotConsumerConfig,
        session_secret: SnapshotSessionSecret,
        digest_secret: [u8; 32],
        actor_limits: SnapshotActorLimits,
    ) -> Result<Self, SnapshotActorError> {
        let digest_key_id = *BlockDigester::new(digest_secret).key_id().as_bytes();
        let publication = Arc::new(Mutex::new(SnapshotBootstrapActor::new(actor_limits)?));
        Ok(Self {
            config,
            session_secret,
            digest_secret,
            digest_key_id,
            publication,
        })
    }

    /// Access the atomically published index owner.
    #[must_use]
    pub fn publication(&self) -> &SharedSnapshotPublication {
        &self.publication
    }

    /// Consume one already-connected companion session until it is fenced.
    ///
    /// The caller owns challenge randomness and reuse prevention. `deadline` is
    /// the sole end-to-end budget; no phase creates a fresh relative timeout.
    /// Dropping this future synchronously revokes any publication it owns and
    /// signals cancellation to in-progress blocking snapshot construction.
    ///
    /// # Errors
    ///
    /// Returns a content-free protocol, transport, build, lifecycle, or
    /// deadline error. A returned error never leaves this session published.
    pub async fn consume(
        &self,
        stream: UnixStream,
        challenge: SnapshotSessionChallenge,
        deadline: Instant,
    ) -> Result<(), SnapshotConsumerError> {
        timeout_at(deadline, self.consume_until(stream, challenge))
            .await
            .map_err(|_| SnapshotConsumerError::Timeout)?
    }

    async fn consume_until(
        &self,
        mut stream: UnixStream,
        challenge: SnapshotSessionChallenge,
    ) -> Result<(), SnapshotConsumerError> {
        let authenticated = self
            .request_authenticated_snapshot(&mut stream, challenge)
            .await?;
        let identity = SnapshotActorIdentity {
            engine_incarnation: authenticated.engine_incarnation().clone(),
            digest_key_id: *authenticated.digest_key_id(),
            companion_generation: authenticated.companion_generation(),
        };
        let started = self
            .publication
            .lock()
            .start_session(identity.clone(), self.config.minimum_snapshot_watermark)
            .map_err(SnapshotConsumerError::Actor)?;
        let cancellation = Arc::new(AtomicBool::new(false));
        let _session_guard = SessionFenceGuard {
            publication: Arc::clone(&self.publication),
            epoch: started.epoch,
            cancellation: Arc::clone(&cancellation),
        };
        require_action(
            self.publication.lock().begin_snapshot_build(started.epoch),
            &[ActorActionKind::SnapshotBuildStarted],
        )?;

        let tail_key = TailSessionKey::derive(
            &self.session_secret,
            challenge,
            identity.companion_generation,
            TailDirection::CompanionToRouter,
        );
        let mut tail_decoder = TailFrameDecoder::new(
            &tail_key,
            TailSessionExpectations {
                direction: TailDirection::CompanionToRouter,
                session_id: challenge,
                first_message_sequence: 1,
                engine_incarnation: &identity.engine_incarnation,
                digest_key_id: &identity.digest_key_id,
                companion_generation: identity.companion_generation,
            },
            self.config.tail_limits,
        )
        .map_err(SnapshotConsumerError::Tail)?;

        let adapter = SnapshotDigestDeltaAdapter::new(self.config.group, self.config.event_limits);
        let mut tail_reader = TailStreamReader::default();
        let mut build =
            Box::pin(self.build_private_generation(authenticated, Arc::clone(&cancellation)));
        let prepared = loop {
            tokio::select! {
                biased;
                build_result = &mut build => match build_result {
                    Ok(prepared) => break prepared,
                    Err(error) => {
                        self.publication.lock().snapshot_build_failed(started.epoch);
                        return Err(error);
                    }
                },
                frame = tail_reader.read(&mut stream, self.config.tail_limits) => {
                    let verified = tail_decoder
                        .decode(&frame?)
                        .map_err(SnapshotConsumerError::Tail)?;
                    let action = self.publication.lock().accept_tail_frame(
                        started.epoch,
                        verified,
                        |index, payload| adapter.apply(index, payload).map(|_| ()),
                    );
                    require_action(action, &[ActorActionKind::TailQueued])?;
                }
            }
        };
        self.install_private_generation(started.epoch, prepared, adapter)?;
        self.consume_tail(
            &mut stream,
            &mut tail_reader,
            &mut tail_decoder,
            started.epoch,
            adapter,
        )
        .await
    }

    async fn request_authenticated_snapshot(
        &self,
        stream: &mut UnixStream,
        challenge: SnapshotSessionChallenge,
    ) -> Result<AuthenticatedSnapshot, SnapshotConsumerError> {
        verify_peer(stream, self.config.expected_peer_uid)?;
        let hello =
            encode_client_hello(challenge, &self.session_secret, self.config.session_limits)
                .map_err(SnapshotConsumerError::Session)?;
        stream
            .write_all(&hello)
            .await
            .map_err(|_| SnapshotConsumerError::Io)?;
        let response = read_snapshot_frame(stream, self.config.session_limits).await?;
        decode_authenticated_snapshot(
            &response,
            SnapshotSessionExpectations {
                challenge,
                engine_incarnation: &self.config.expected_engine_incarnation,
                digest_key_id: &self.digest_key_id,
                minimum_snapshot_watermark: self.config.minimum_snapshot_watermark,
                minimum_companion_generation: self.config.minimum_companion_generation,
            },
            &self.session_secret,
            self.config.session_limits,
        )
        .map_err(SnapshotConsumerError::Session)
    }

    async fn build_private_generation(
        &self,
        authenticated: AuthenticatedSnapshot,
        cancellation: Arc<AtomicBool>,
    ) -> Result<PreparedSnapshotGeneration, SnapshotConsumerError> {
        let digest_secret = self.digest_secret;
        let snapshot_limits = self.config.snapshot_limits;
        let index_limits = self.config.index_limits;
        let group = self.config.group;
        let minimum_snapshot_watermark = self.config.minimum_snapshot_watermark;
        tokio::task::spawn_blocking(move || {
            prepare_authenticated_snapshot_with_cancel(
                authenticated,
                &digest_secret,
                snapshot_limits,
                index_limits,
                group,
                minimum_snapshot_watermark,
                || cancellation.load(Ordering::Acquire),
            )
        })
        .await
        .map_err(|_| SnapshotConsumerError::SnapshotTask)?
        .map_err(SnapshotConsumerError::Bootstrap)
    }

    fn install_private_generation(
        &self,
        epoch: SessionEpoch,
        prepared: PreparedSnapshotGeneration,
        adapter: SnapshotDigestDeltaAdapter,
    ) -> Result<(), SnapshotConsumerError> {
        let installed = self
            .publication
            .lock()
            .install_prepared_snapshot(epoch, prepared, |index, payload| {
                adapter.apply(index, payload).map(|_| ())
            })
            .map_err(SnapshotConsumerError::Actor)?;
        require_action(
            installed,
            &[
                ActorActionKind::SnapshotInstalled,
                ActorActionKind::TailApplied,
                ActorActionKind::Published,
                ActorActionKind::AlreadyPublished,
            ],
        )
    }

    async fn consume_tail(
        &self,
        stream: &mut UnixStream,
        tail_reader: &mut TailStreamReader,
        tail_decoder: &mut TailFrameDecoder<'_>,
        epoch: SessionEpoch,
        adapter: SnapshotDigestDeltaAdapter,
    ) -> Result<(), SnapshotConsumerError> {
        loop {
            let frame = tail_reader.read(stream, self.config.tail_limits).await?;
            let verified = tail_decoder
                .decode(&frame)
                .map_err(SnapshotConsumerError::Tail)?;
            let action =
                self.publication
                    .lock()
                    .accept_tail_frame(epoch, verified, |index, payload| {
                        adapter.apply(index, payload).map(|_| ())
                    });
            require_action(
                action,
                &[
                    ActorActionKind::TailApplied,
                    ActorActionKind::DuplicateIgnored,
                    ActorActionKind::IdentityCurrent,
                    ActorActionKind::Published,
                    ActorActionKind::AlreadyPublished,
                ],
            )?;
        }
    }
}

impl Drop for SnapshotConsumer {
    fn drop(&mut self) {
        self.digest_secret.fill(0);
    }
}

#[derive(Debug, Error)]
pub enum SnapshotConsumerError {
    #[error("snapshot consumer timed out")]
    Timeout,
    #[error("snapshot consumer I/O failed")]
    Io,
    #[error("snapshot consumer peer credential lookup failed")]
    PeerCredentialFailed,
    #[error("snapshot consumer peer user does not match")]
    PeerUidMismatch,
    #[error("snapshot consumer frame was truncated")]
    Truncated,
    #[error("snapshot companion disconnected")]
    Disconnected,
    #[error("snapshot session rejected")]
    Session(#[source] SnapshotSessionError),
    #[error("snapshot private generation build failed")]
    Bootstrap(#[source] SnapshotBootstrapError),
    #[error("snapshot private generation task failed")]
    SnapshotTask,
    #[error("snapshot publication actor rejected the session")]
    Actor(#[source] SnapshotActorError),
    #[error("snapshot tail frame rejected")]
    Tail(#[source] TailWireError),
    #[error("snapshot generation was fenced")]
    Fenced(SnapshotTailFenceReason),
    #[error("snapshot session was superseded")]
    Superseded,
    #[error("snapshot publication actor returned an unexpected transition")]
    UnexpectedActorAction,
}

impl SnapshotConsumerError {
    #[must_use]
    pub const fn reason(&self) -> &'static str {
        match self {
            Self::Timeout => "timeout",
            Self::Io => "io_failed",
            Self::PeerCredentialFailed => "peer_credential_failed",
            Self::PeerUidMismatch => "peer_uid_mismatch",
            Self::Truncated => "truncated",
            Self::Disconnected => "disconnected",
            Self::Session(error) => error.reason(),
            Self::Bootstrap(error) => error.reason(),
            Self::SnapshotTask => "snapshot_task_failed",
            Self::Actor(error) => error.reason(),
            Self::Tail(error) => error.reason(),
            Self::Fenced(reason) => reason.as_str(),
            Self::Superseded => "superseded",
            Self::UnexpectedActorAction => "unexpected_actor_action",
        }
    }
}

struct SessionFenceGuard {
    publication: SharedSnapshotPublication,
    epoch: SessionEpoch,
    cancellation: Arc<AtomicBool>,
}

impl Drop for SessionFenceGuard {
    fn drop(&mut self) {
        self.cancellation.store(true, Ordering::Release);
        self.publication.lock().disconnected(self.epoch);
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum ActorActionKind {
    SnapshotBuildStarted,
    SnapshotInstalled,
    TailQueued,
    TailApplied,
    DuplicateIgnored,
    IdentityCurrent,
    Published,
    AlreadyPublished,
}

fn require_action(
    action: ActorAction,
    accepted: &[ActorActionKind],
) -> Result<(), SnapshotConsumerError> {
    let kind = match action {
        ActorAction::SnapshotBuildStarted => ActorActionKind::SnapshotBuildStarted,
        ActorAction::SnapshotInstalled => ActorActionKind::SnapshotInstalled,
        ActorAction::TailQueued => ActorActionKind::TailQueued,
        ActorAction::TailApplied => ActorActionKind::TailApplied,
        ActorAction::DuplicateIgnored => ActorActionKind::DuplicateIgnored,
        ActorAction::IdentityCurrent => ActorActionKind::IdentityCurrent,
        ActorAction::Published { .. } => ActorActionKind::Published,
        ActorAction::AlreadyPublished => ActorActionKind::AlreadyPublished,
        ActorAction::SessionFenced(reason) => return Err(SnapshotConsumerError::Fenced(reason)),
        ActorAction::StaleEpochIgnored | ActorAction::IdentityChanged => {
            return Err(SnapshotConsumerError::Superseded);
        }
    };
    if accepted.contains(&kind) {
        Ok(())
    } else {
        Err(SnapshotConsumerError::UnexpectedActorAction)
    }
}

fn verify_peer(stream: &UnixStream, expected_uid: u32) -> Result<(), SnapshotConsumerError> {
    let credential = stream
        .peer_cred()
        .map_err(|_| SnapshotConsumerError::PeerCredentialFailed)?;
    if credential.uid() != expected_uid {
        return Err(SnapshotConsumerError::PeerUidMismatch);
    }
    Ok(())
}

async fn read_snapshot_frame(
    stream: &mut UnixStream,
    limits: SnapshotSessionLimits,
) -> Result<Vec<u8>, SnapshotConsumerError> {
    let mut frame = vec![0_u8; SNAPSHOT_RESPONSE_LENGTH_PREFIX_BYTES];
    read_exact(stream, &mut frame).await?;
    let total = authenticated_snapshot_frame_length(&frame, limits)
        .map_err(SnapshotConsumerError::Session)?;
    frame.resize(total, 0);
    read_exact(stream, &mut frame[SNAPSHOT_RESPONSE_LENGTH_PREFIX_BYTES..]).await?;
    Ok(frame)
}

#[derive(Default)]
struct TailStreamReader {
    frame: Vec<u8>,
    filled: usize,
    total: Option<usize>,
}

impl TailStreamReader {
    /// Cancellation-safe because partially read bytes and the cursor live in
    /// `self`, not in the dropped read future.
    async fn read(
        &mut self,
        stream: &mut UnixStream,
        limits: TailWireLimits,
    ) -> Result<Vec<u8>, SnapshotConsumerError> {
        if self.frame.is_empty() {
            self.frame.resize(TAIL_FRAME_LENGTH_PREFIX_BYTES, 0);
        }
        self.fill(stream, TAIL_FRAME_LENGTH_PREFIX_BYTES).await?;
        let total = if let Some(total) = self.total {
            total
        } else {
            let total =
                tail_frame_length(&self.frame, limits).map_err(SnapshotConsumerError::Tail)?;
            self.frame.resize(total, 0);
            self.total = Some(total);
            total
        };
        self.fill(stream, total).await?;
        self.filled = 0;
        self.total = None;
        Ok(std::mem::take(&mut self.frame))
    }

    async fn fill(
        &mut self,
        stream: &mut UnixStream,
        target: usize,
    ) -> Result<(), SnapshotConsumerError> {
        while self.filled < target {
            match stream.read(&mut self.frame[self.filled..target]).await {
                Ok(0) if self.filled == 0 => return Err(SnapshotConsumerError::Disconnected),
                Ok(0) => return Err(SnapshotConsumerError::Truncated),
                Ok(read) => self.filled += read,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => return Err(SnapshotConsumerError::Io),
            }
        }
        Ok(())
    }
}

async fn read_exact(
    stream: &mut UnixStream,
    destination: &mut [u8],
) -> Result<(), SnapshotConsumerError> {
    let mut filled = 0;
    while filled < destination.len() {
        match stream.read(&mut destination[filled..]).await {
            Ok(0) if filled == 0 => return Err(SnapshotConsumerError::Disconnected),
            Ok(0) => return Err(SnapshotConsumerError::Truncated),
            Ok(read) => filled += read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => return Err(SnapshotConsumerError::Io),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{future::pending, time::Duration};

    use tokio::sync::oneshot;

    use super::*;
    use crate::{
        kv_snapshot::{
            AttentionKind, DigestAlgorithm, DigestSpec, GroupDisposition, GroupMetadata,
            ResetScope, SnapshotBody, SnapshotCapacity, encode_snapshot,
        },
        snapshot_session::{
            SnapshotSessionBinding, decode_client_hello, encode_authenticated_snapshot,
        },
        snapshot_tail_wire::{TailFrameBinding, TailFrameType, encode_tail_frame},
    };

    const SESSION_SECRET: [u8; 32] = *b"snapshot-session-secret-32-byte!";
    const DIGEST_SECRET: [u8; 32] = [0x91; 32];
    const CHALLENGE: SnapshotSessionChallenge = SnapshotSessionChallenge::new([0xa1; 32]);
    const WATERMARK: u64 = 100;
    const GENERATION: u64 = 7;

    fn incarnation() -> EngineIncarnation {
        EngineIncarnation {
            engine_id: "engine-a".into(),
            model_revision: "revision-a".into(),
            image_digest: "sha256:image-a".into(),
            process_started_unix_ns: 42,
            attestation_sha256: vec![3; 32],
        }
    }

    fn pair_and_consumer() -> (UnixStream, UnixStream, Arc<SnapshotConsumer>) {
        let (router, companion) = UnixStream::pair().unwrap();
        let expected_peer_uid = router.peer_cred().unwrap().uid();
        let consumer = SnapshotConsumer::new(
            SnapshotConsumerConfig {
                expected_peer_uid,
                expected_engine_incarnation: incarnation(),
                minimum_snapshot_watermark: WATERMARK,
                minimum_companion_generation: GENERATION,
                group: SnapshotGroupKey {
                    data_parallel_rank: 0,
                    group_idx: 0,
                },
                session_limits: SnapshotSessionLimits::default(),
                snapshot_limits: SnapshotLimits::default(),
                index_limits: DigestIndexLimits::default(),
                tail_limits: TailWireLimits::default(),
                event_limits: KvWireLimits::default(),
            },
            SnapshotSessionSecret::new(SESSION_SECRET),
            DIGEST_SECRET,
            SnapshotActorLimits::default(),
        )
        .unwrap();
        (router, companion, Arc::new(consumer))
    }

    fn snapshot_response(challenge: SnapshotSessionChallenge, generation: u64) -> Vec<u8> {
        let digester = BlockDigester::new(DIGEST_SECRET);
        let mut body = SnapshotBody {
            engine_incarnation: incarnation(),
            watermark: WATERMARK,
            reset_scope: ResetScope::full_engine(),
            digest: DigestSpec {
                algorithm: DigestAlgorithm::HmacSha256V1,
                key_id: digester.key_id().to_vec(),
                digest_bytes: 32,
            },
            capacity: SnapshotCapacity::default(),
            groups: vec![GroupMetadata {
                data_parallel_rank: 0,
                group_idx: 0,
                attention_kind: AttentionKind::MlaAttention,
                disposition: GroupDisposition::Indexed,
                block_size: 64,
            }],
            records: Vec::new(),
        };
        body.refresh_capacity().unwrap();
        let snapshot = encode_snapshot(&body, SnapshotLimits::default()).unwrap();
        encode_authenticated_snapshot(
            &snapshot,
            SnapshotSessionBinding {
                challenge,
                engine_incarnation: &body.engine_incarnation,
                snapshot_watermark: body.watermark,
                digest_key_id: digester.key_id().as_bytes(),
                companion_generation: generation,
            },
            &SnapshotSessionSecret::new(SESSION_SECRET),
            SnapshotSessionLimits::default(),
        )
        .unwrap()
    }

    async fn read_hello(stream: &mut UnixStream) -> SnapshotSessionChallenge {
        let secret = SnapshotSessionSecret::new(SESSION_SECRET);
        let hello_len = encode_client_hello(
            SnapshotSessionChallenge::new([0; 32]),
            &secret,
            SnapshotSessionLimits::default(),
        )
        .unwrap()
        .len();
        let mut hello = vec![0; hello_len];
        stream.read_exact(&mut hello).await.unwrap();
        decode_client_hello(&hello, &secret, SnapshotSessionLimits::default()).unwrap()
    }

    fn tail(
        challenge: SnapshotSessionChallenge,
        frame_type: TailFrameType,
        message_sequence: u64,
        delivery_sequence: u64,
        event_watermark: u64,
        payload: &[u8],
        generation: u64,
    ) -> Vec<u8> {
        let secret = SnapshotSessionSecret::new(SESSION_SECRET);
        let key = TailSessionKey::derive(
            &secret,
            challenge,
            generation,
            TailDirection::CompanionToRouter,
        );
        let digester = BlockDigester::new(DIGEST_SECRET);
        encode_tail_frame(
            payload,
            TailFrameBinding {
                frame_type,
                direction: TailDirection::CompanionToRouter,
                session_id: challenge,
                message_sequence,
                delivery_sequence,
                event_watermark,
                engine_incarnation: &incarnation(),
                digest_key_id: digester.key_id().as_bytes(),
                companion_generation: generation,
            },
            &key,
            TailWireLimits::default(),
        )
        .unwrap()
    }

    async fn wait_until_published(publication: &SharedSnapshotPublication) {
        timeout_at(Instant::now() + Duration::from_secs(1), async {
            loop {
                if publication.lock().published_index().is_some() {
                    return;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn authentic_snapshot_and_sparse_tail_publish_atomically() {
        let (router, mut companion, consumer) = pair_and_consumer();
        let publication = Arc::clone(consumer.publication());
        let (release_sender, release_receiver) = oneshot::channel();
        let companion_task = tokio::spawn(async move {
            let challenge = read_hello(&mut companion).await;
            companion
                .write_all(&snapshot_response(challenge, GENERATION))
                .await
                .unwrap();
            let empty_batch =
                rmp_serde::to_vec(&(1.0, Vec::<serde_json::Value>::new(), 0)).unwrap();
            companion
                .write_all(&tail(
                    challenge,
                    TailFrameType::Event,
                    1,
                    1,
                    150,
                    &empty_batch,
                    GENERATION,
                ))
                .await
                .unwrap();
            companion
                .write_all(&tail(
                    challenge,
                    TailFrameType::CaughtUp,
                    2,
                    1,
                    150,
                    b"",
                    GENERATION,
                ))
                .await
                .unwrap();
            let _ = release_receiver.await;
            companion
                .write_all(&tail(
                    challenge,
                    TailFrameType::Disconnect,
                    3,
                    0,
                    0,
                    b"",
                    GENERATION,
                ))
                .await
                .unwrap();
        });
        let consumer_task = tokio::spawn(async move {
            consumer
                .consume(router, CHALLENGE, Instant::now() + Duration::from_secs(2))
                .await
        });

        wait_until_published(&publication).await;
        assert_eq!(
            publication.lock().published_index().unwrap().stats().nodes,
            0
        );
        release_sender.send(()).unwrap();
        let error = consumer_task.await.unwrap().unwrap_err();
        assert!(matches!(
            error,
            SnapshotConsumerError::Fenced(SnapshotTailFenceReason::Disconnected)
        ));
        companion_task.await.unwrap();
        assert!(publication.lock().published_index().is_none());
    }

    #[tokio::test]
    async fn malformed_tail_mac_discards_private_generation() {
        let (router, mut companion, consumer) = pair_and_consumer();
        let publication = Arc::clone(consumer.publication());
        let companion_task = tokio::spawn(async move {
            let challenge = read_hello(&mut companion).await;
            companion
                .write_all(&snapshot_response(challenge, GENERATION))
                .await
                .unwrap();
            let mut caught_up = tail(
                challenge,
                TailFrameType::CaughtUp,
                1,
                0,
                WATERMARK,
                b"",
                GENERATION,
            );
            *caught_up.last_mut().unwrap() ^= 1;
            companion.write_all(&caught_up).await.unwrap();
            pending::<()>().await;
        });
        let error = consumer
            .consume(router, CHALLENGE, Instant::now() + Duration::from_secs(2))
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            SnapshotConsumerError::Tail(TailWireError::AuthenticationFailed)
        ));
        companion_task.abort();
        assert!(publication.lock().published_index().is_none());
        assert_eq!(publication.lock().session_count(), 0);
    }

    #[tokio::test]
    async fn delivery_gap_fences_private_generation() {
        let (router, mut companion, consumer) = pair_and_consumer();
        let publication = Arc::clone(consumer.publication());
        let companion_task = tokio::spawn(async move {
            let challenge = read_hello(&mut companion).await;
            companion
                .write_all(&snapshot_response(challenge, GENERATION))
                .await
                .unwrap();
            let empty_batch =
                rmp_serde::to_vec(&(1.0, Vec::<serde_json::Value>::new(), 0)).unwrap();
            companion
                .write_all(&tail(
                    challenge,
                    TailFrameType::Event,
                    1,
                    2,
                    10_000,
                    &empty_batch,
                    GENERATION,
                ))
                .await
                .unwrap();
            pending::<()>().await;
        });
        let error = consumer
            .consume(router, CHALLENGE, Instant::now() + Duration::from_secs(2))
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            SnapshotConsumerError::Fenced(SnapshotTailFenceReason::TailGap)
        ));
        companion_task.abort();
        assert!(publication.lock().published_index().is_none());
    }

    #[tokio::test]
    async fn stale_generation_is_rejected_before_actor_admission() {
        let (router, mut companion, consumer) = pair_and_consumer();
        let publication = Arc::clone(consumer.publication());
        let companion_task = tokio::spawn(async move {
            let challenge = read_hello(&mut companion).await;
            companion
                .write_all(&snapshot_response(challenge, GENERATION - 1))
                .await
                .unwrap();
        });
        let error = consumer
            .consume(router, CHALLENGE, Instant::now() + Duration::from_secs(2))
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            SnapshotConsumerError::Session(SnapshotSessionError::StaleGeneration)
        ));
        companion_task.await.unwrap();
        assert_eq!(publication.lock().session_count(), 0);
    }

    #[tokio::test]
    async fn eof_after_publication_immediately_revokes_owner() {
        let (router, mut companion, consumer) = pair_and_consumer();
        let publication = Arc::clone(consumer.publication());
        let companion_task = tokio::spawn(async move {
            let challenge = read_hello(&mut companion).await;
            companion
                .write_all(&snapshot_response(challenge, GENERATION))
                .await
                .unwrap();
            companion
                .write_all(&tail(
                    challenge,
                    TailFrameType::CaughtUp,
                    1,
                    0,
                    WATERMARK,
                    b"",
                    GENERATION,
                ))
                .await
                .unwrap();
        });
        let error = consumer
            .consume(router, CHALLENGE, Instant::now() + Duration::from_secs(2))
            .await
            .unwrap_err();
        assert!(matches!(error, SnapshotConsumerError::Disconnected));
        companion_task.await.unwrap();
        assert!(publication.lock().published_index().is_none());
    }

    #[tokio::test]
    async fn task_cancellation_synchronously_revokes_publication() {
        let (router, mut companion, consumer) = pair_and_consumer();
        let publication = Arc::clone(consumer.publication());
        let companion_task = tokio::spawn(async move {
            let challenge = read_hello(&mut companion).await;
            companion
                .write_all(&snapshot_response(challenge, GENERATION))
                .await
                .unwrap();
            companion
                .write_all(&tail(
                    challenge,
                    TailFrameType::CaughtUp,
                    1,
                    0,
                    WATERMARK,
                    b"",
                    GENERATION,
                ))
                .await
                .unwrap();
            pending::<()>().await;
        });
        let consumer_task = tokio::spawn(async move {
            consumer
                .consume(router, CHALLENGE, Instant::now() + Duration::from_secs(30))
                .await
        });
        wait_until_published(&publication).await;
        consumer_task.abort();
        assert!(consumer_task.await.unwrap_err().is_cancelled());
        assert!(publication.lock().published_index().is_none());
        assert_eq!(publication.lock().session_count(), 0);
        companion_task.abort();
    }
}
