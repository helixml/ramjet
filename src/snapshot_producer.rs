//! Companion-side authenticated snapshot and live-tail session production.
//!
//! [`SnapshotSupervisor`](crate::snapshot_supervisor) owns admission and gives
//! this handler an accepted stream plus one absolute deadline. This module
//! verifies the peer before reading protocol bytes, authenticates the client
//! hello, starts a bounded live-tail subscription before snapshot construction,
//! and writes a length-framed snapshot followed by authenticated tail frames.
//! Source callbacks return owned data; no source or engine lock is retained
//! across serialization or socket I/O.

use std::{
    future::Future,
    io,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use thiserror::Error;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{
        UnixStream,
        unix::{OwnedReadHalf, OwnedWriteHalf},
    },
    sync::{mpsc, watch},
    time::{Instant, timeout_at},
};

use crate::{
    kv_snapshot::EngineIncarnation,
    snapshot_session::{
        SNAPSHOT_DIGEST_KEY_ID_BYTES, SnapshotSessionBinding, SnapshotSessionChallenge,
        SnapshotSessionError, SnapshotSessionLimits, SnapshotSessionSecret, decode_client_hello,
        encode_authenticated_snapshot, encode_client_hello,
    },
    snapshot_tail_wire::{
        TailDirection, TailFrameBinding, TailFrameType, TailSessionKey, TailWireError,
        TailWireLimits, encode_tail_frame,
    },
};

const MAX_TAIL_QUEUE_CAPACITY: usize = 65_536;

pub type SnapshotBuildFuture = Pin<
    Box<
        dyn Future<Output = Result<ProducerSnapshot, SnapshotProducerSourceError>> + Send + 'static,
    >,
>;

#[derive(Clone, Debug)]
pub struct SnapshotProducerConfig {
    pub expected_peer_uid: u32,
    pub session_limits: SnapshotSessionLimits,
    pub tail_limits: TailWireLimits,
    pub tail_queue_capacity: usize,
}

/// Exact identity bound into both the snapshot response and every tail frame.
#[derive(Clone, Eq, PartialEq)]
pub struct ProducerIdentity {
    pub engine_incarnation: EngineIncarnation,
    pub digest_key_id: [u8; SNAPSHOT_DIGEST_KEY_ID_BYTES],
    pub companion_generation: u64,
}

impl std::fmt::Debug for ProducerIdentity {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProducerIdentity")
            .field("engine_incarnation", &"[REDACTED]")
            .field("digest_key_id", &"[REDACTED]")
            .field("companion_generation", &self.companion_generation)
            .finish()
    }
}

pub struct ProducerSnapshot {
    pub identity: ProducerIdentity,
    pub watermark: u64,
    pub frame: Vec<u8>,
}

impl std::fmt::Debug for ProducerSnapshot {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProducerSnapshot")
            .field("identity", &self.identity)
            .field("watermark", &self.watermark)
            .field("frame_bytes", &self.frame.len())
            .finish()
    }
}

pub enum ProducerTailEvent {
    Batch {
        identity: ProducerIdentity,
        event_watermark: u64,
        payload: Vec<u8>,
    },
    CaughtUp {
        identity: ProducerIdentity,
        event_watermark: u64,
    },
    IdentityChanged(ProducerIdentity),
    Disconnect(ProducerIdentity),
}

/// Cancellation shared with source work started outside the build future.
#[derive(Clone)]
pub struct SnapshotProducerCancellation {
    cancelled: Arc<AtomicBool>,
    changed: watch::Receiver<bool>,
}

impl SnapshotProducerCancellation {
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    pub async fn cancelled(&mut self) {
        while !self.is_cancelled() && !*self.changed.borrow() {
            if self.changed.changed().await.is_err() {
                break;
            }
        }
    }
}

/// Bounded producer-side tail sink. `send` applies backpressure and is woken by
/// client disconnect, deadline cancellation, or supervisor shutdown.
#[derive(Clone)]
pub struct SnapshotTailPublisher {
    sender: mpsc::Sender<ProducerTailEvent>,
    cancellation: SnapshotProducerCancellation,
    max_payload_bytes: usize,
}

impl SnapshotTailPublisher {
    /// Send one event while respecting bounded capacity and session cancellation.
    ///
    /// # Errors
    ///
    /// Returns a content-free source error when the session is cancelled or
    /// the handler has closed its receiver.
    pub async fn send(&self, event: ProducerTailEvent) -> Result<(), SnapshotProducerSourceError> {
        self.validate(&event)?;
        let mut cancellation = self.cancellation.clone();
        tokio::select! {
            biased;
            () = cancellation.cancelled() => Err(SnapshotProducerSourceError::Cancelled),
            result = self.sender.send(event) => {
                result.map_err(|_| SnapshotProducerSourceError::Cancelled)
            }
        }
    }

    /// Non-blocking bounded send for engine callbacks that cannot await.
    ///
    /// # Errors
    ///
    /// Returns `QueueFull` on backpressure and `Cancelled` after session close.
    pub fn try_send(&self, event: ProducerTailEvent) -> Result<(), SnapshotProducerSourceError> {
        self.validate(&event)?;
        if self.cancellation.is_cancelled() {
            return Err(SnapshotProducerSourceError::Cancelled);
        }
        self.sender.try_send(event).map_err(|error| match error {
            mpsc::error::TrySendError::Full(_) => SnapshotProducerSourceError::QueueFull,
            mpsc::error::TrySendError::Closed(_) => SnapshotProducerSourceError::Cancelled,
        })
    }

    fn validate(&self, event: &ProducerTailEvent) -> Result<(), SnapshotProducerSourceError> {
        if matches!(
            event,
            ProducerTailEvent::Batch { payload, .. } if payload.len() > self.max_payload_bytes
        ) {
            return Err(SnapshotProducerSourceError::PayloadTooLarge);
        }
        Ok(())
    }
}

#[cfg(test)]
pub(crate) fn test_tail_channel(
    capacity: usize,
    max_payload_bytes: usize,
) -> (
    SnapshotTailPublisher,
    SnapshotProducerCancellation,
    mpsc::Receiver<ProducerTailEvent>,
    watch::Sender<bool>,
    Arc<AtomicBool>,
) {
    let (sender, receiver) = mpsc::channel(capacity);
    let (changed_sender, changed) = watch::channel(false);
    let cancelled = Arc::new(AtomicBool::new(false));
    let cancellation = SnapshotProducerCancellation {
        cancelled: Arc::clone(&cancelled),
        changed,
    };
    (
        SnapshotTailPublisher {
            sender,
            cancellation: cancellation.clone(),
            max_payload_bytes,
        },
        cancellation,
        receiver,
        changed_sender,
        cancelled,
    )
}

/// Engine-independent source boundary. `start` must establish live-tail
/// capture before returning its snapshot build future and must return quickly.
/// The future and every task using the publisher must observe cancellation.
pub trait SnapshotProducerSource: Send + Sync + 'static {
    /// Begin tail capture and return independently owned snapshot-build work.
    ///
    /// # Errors
    ///
    /// Returns a content-free source error when subscription cannot be
    /// established. No snapshot build should begin after that failure.
    fn start(
        &self,
        publisher: SnapshotTailPublisher,
        cancellation: SnapshotProducerCancellation,
    ) -> Result<SnapshotBuildFuture, SnapshotProducerSourceError>;
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum SnapshotProducerSourceError {
    #[error("snapshot producer source failed")]
    Failed,
    #[error("snapshot producer source was cancelled")]
    Cancelled,
    #[error("snapshot producer tail queue is full")]
    QueueFull,
    #[error("snapshot producer tail payload is too large")]
    PayloadTooLarge,
}

#[derive(Debug, Error)]
pub enum SnapshotProducerError {
    #[error("snapshot producer configuration is invalid")]
    InvalidConfig,
    #[error("snapshot producer timed out")]
    Timeout,
    #[error("snapshot producer I/O failed")]
    Io,
    #[error("snapshot producer peer credential lookup failed")]
    PeerCredentialFailed,
    #[error("snapshot producer peer user does not match")]
    PeerUidMismatch,
    #[error("snapshot producer client disconnected")]
    ClientDisconnected,
    #[error("snapshot producer protocol input is invalid")]
    ProtocolViolation,
    #[error("snapshot producer hello was truncated")]
    Truncated,
    #[error("snapshot producer session authentication failed")]
    Session(#[source] SnapshotSessionError),
    #[error("snapshot producer tail framing failed")]
    Tail(#[source] TailWireError),
    #[error("snapshot producer source failed")]
    Source(#[source] SnapshotProducerSourceError),
    #[error("snapshot producer tail source closed")]
    TailClosed,
    #[error("snapshot producer event watermark regressed")]
    WatermarkRegression,
    #[error("snapshot producer identity changed")]
    IdentityChanged,
    #[error("snapshot producer sequence was exhausted")]
    SequenceOverflow,
}

impl SnapshotProducerError {
    #[must_use]
    pub const fn reason(&self) -> &'static str {
        match self {
            Self::InvalidConfig => "invalid_config",
            Self::Timeout => "timeout",
            Self::Io => "io_failed",
            Self::PeerCredentialFailed => "peer_credential_failed",
            Self::PeerUidMismatch => "peer_uid_mismatch",
            Self::ClientDisconnected => "client_disconnected",
            Self::ProtocolViolation => "protocol_violation",
            Self::Truncated => "truncated",
            Self::Session(error) => error.reason(),
            Self::Tail(error) => error.reason(),
            Self::Source(error) => match error {
                SnapshotProducerSourceError::Failed => "source_failed",
                SnapshotProducerSourceError::Cancelled => "source_cancelled",
                SnapshotProducerSourceError::QueueFull => "source_queue_full",
                SnapshotProducerSourceError::PayloadTooLarge => "source_payload_too_large",
            },
            Self::TailClosed => "tail_closed",
            Self::WatermarkRegression => "watermark_regression",
            Self::IdentityChanged => "identity_changed",
            Self::SequenceOverflow => "sequence_overflow",
        }
    }
}

/// Ready-to-share handler for `supervise_snapshot_sessions`.
pub struct SnapshotProducer {
    config: SnapshotProducerConfig,
    session_secret: Arc<SnapshotSessionSecret>,
    source: Arc<dyn SnapshotProducerSource>,
}

impl SnapshotProducer {
    /// Construct a producer with a bounded per-client tail queue.
    ///
    /// # Errors
    ///
    /// Rejects zero capacity or inconsistent frame/payload limits.
    pub fn new(
        config: SnapshotProducerConfig,
        session_secret: Arc<SnapshotSessionSecret>,
        source: Arc<dyn SnapshotProducerSource>,
    ) -> Result<Self, SnapshotProducerError> {
        if config.tail_queue_capacity == 0
            || config.tail_queue_capacity > MAX_TAIL_QUEUE_CAPACITY
            || config.tail_limits.max_frame_bytes == 0
            || config.tail_limits.max_payload_bytes >= config.tail_limits.max_frame_bytes
        {
            return Err(SnapshotProducerError::InvalidConfig);
        }
        Ok(Self {
            config,
            session_secret,
            source,
        })
    }

    /// Handle one supervisor-admitted stream under its supplied absolute deadline.
    ///
    /// # Errors
    ///
    /// Returns a content-free peer, protocol, source, framing, I/O, identity,
    /// cancellation, or deadline error.
    pub async fn handle(
        &self,
        stream: UnixStream,
        deadline: Instant,
    ) -> Result<(), SnapshotProducerError> {
        timeout_at(deadline, self.handle_until(stream))
            .await
            .map_err(|_| SnapshotProducerError::Timeout)?
    }

    async fn handle_until(&self, mut stream: UnixStream) -> Result<(), SnapshotProducerError> {
        verify_peer(&stream, self.config.expected_peer_uid)?;
        let challenge = read_authenticated_hello(
            &mut stream,
            &self.session_secret,
            self.config.session_limits,
        )
        .await?;
        let (reader, writer) = stream.into_split();
        let (cancel_sender, cancel_receiver) = watch::channel(false);
        let cancellation = SnapshotProducerCancellation {
            cancelled: Arc::new(AtomicBool::new(false)),
            changed: cancel_receiver,
        };
        let _cancel_guard = CancellationGuard {
            sender: cancel_sender,
            flag: Arc::clone(&cancellation.cancelled),
        };
        let (tail_sender, tail_receiver) = mpsc::channel(self.config.tail_queue_capacity);
        let publisher = SnapshotTailPublisher {
            sender: tail_sender,
            cancellation: cancellation.clone(),
            max_payload_bytes: self.config.tail_limits.max_payload_bytes,
        };
        let build = self
            .source
            .start(publisher, cancellation)
            .map_err(SnapshotProducerError::Source)?;
        self.produce(reader, writer, challenge, build, tail_receiver)
            .await
    }

    async fn produce(
        &self,
        mut reader: OwnedReadHalf,
        mut writer: OwnedWriteHalf,
        challenge: SnapshotSessionChallenge,
        mut build: SnapshotBuildFuture,
        mut tail: mpsc::Receiver<ProducerTailEvent>,
    ) -> Result<(), SnapshotProducerError> {
        let snapshot = tokio::select! {
            biased;
            read = read_client_signal(&mut reader) => return Err(read),
            result = &mut build => result.map_err(SnapshotProducerError::Source)?,
        };
        let response = encode_authenticated_snapshot(
            &snapshot.frame,
            SnapshotSessionBinding {
                challenge,
                engine_incarnation: &snapshot.identity.engine_incarnation,
                snapshot_watermark: snapshot.watermark,
                digest_key_id: &snapshot.identity.digest_key_id,
                companion_generation: snapshot.identity.companion_generation,
            },
            &self.session_secret,
            self.config.session_limits,
        )
        .map_err(SnapshotProducerError::Session)?;
        write_frame(&mut writer, &response).await?;

        let tail_key = TailSessionKey::derive(
            &self.session_secret,
            challenge,
            snapshot.identity.companion_generation,
            TailDirection::CompanionToRouter,
        );
        let mut state = TailWriteState {
            identity: snapshot.identity,
            challenge,
            message_sequence: 1,
            delivery_sequence: 0,
            event_watermark: snapshot.watermark,
        };
        loop {
            let event = tokio::select! {
                biased;
                read = read_client_signal(&mut reader) => return Err(read),
                event = tail.recv() => event.ok_or(SnapshotProducerError::TailClosed)?,
            };
            let terminal = state.encode(event, &tail_key, self.config.tail_limits)?;
            write_frame(&mut writer, &terminal.frame).await?;
            if let Some(outcome) = terminal.outcome {
                return outcome;
            }
        }
    }
}

struct CancellationGuard {
    sender: watch::Sender<bool>,
    flag: Arc<AtomicBool>,
}

impl Drop for CancellationGuard {
    fn drop(&mut self) {
        self.flag.store(true, Ordering::Release);
        let _ = self.sender.send(true);
    }
}

struct TailWriteState {
    identity: ProducerIdentity,
    challenge: SnapshotSessionChallenge,
    message_sequence: u64,
    delivery_sequence: u64,
    event_watermark: u64,
}

struct EncodedTail {
    frame: Vec<u8>,
    outcome: Option<Result<(), SnapshotProducerError>>,
}

impl TailWriteState {
    fn encode(
        &mut self,
        event: ProducerTailEvent,
        key: &TailSessionKey,
        limits: TailWireLimits,
    ) -> Result<EncodedTail, SnapshotProducerError> {
        let (frame_type, identity, delivery, watermark, payload, outcome) = match event {
            ProducerTailEvent::Batch {
                identity,
                event_watermark,
                payload,
            } => {
                self.require_identity(&identity)?;
                if event_watermark <= self.event_watermark {
                    return Err(SnapshotProducerError::WatermarkRegression);
                }
                let delivery = self
                    .delivery_sequence
                    .checked_add(1)
                    .ok_or(SnapshotProducerError::SequenceOverflow)?;
                self.delivery_sequence = delivery;
                self.event_watermark = event_watermark;
                (
                    TailFrameType::Event,
                    identity,
                    delivery,
                    event_watermark,
                    payload,
                    None,
                )
            }
            ProducerTailEvent::CaughtUp {
                identity,
                event_watermark,
            } => {
                self.require_identity(&identity)?;
                if event_watermark != self.event_watermark {
                    return Err(SnapshotProducerError::WatermarkRegression);
                }
                (
                    TailFrameType::CaughtUp,
                    identity,
                    self.delivery_sequence,
                    event_watermark,
                    Vec::new(),
                    None,
                )
            }
            ProducerTailEvent::IdentityChanged(identity) => (
                TailFrameType::Identity,
                identity,
                0,
                0,
                Vec::new(),
                Some(Err(SnapshotProducerError::IdentityChanged)),
            ),
            ProducerTailEvent::Disconnect(identity) => {
                self.require_identity(&identity)?;
                (
                    TailFrameType::Disconnect,
                    identity,
                    0,
                    0,
                    Vec::new(),
                    Some(Ok(())),
                )
            }
        };
        let frame = encode_tail_frame(
            &payload,
            TailFrameBinding {
                frame_type,
                direction: TailDirection::CompanionToRouter,
                session_id: self.challenge,
                message_sequence: self.message_sequence,
                delivery_sequence: delivery,
                event_watermark: watermark,
                engine_incarnation: &identity.engine_incarnation,
                digest_key_id: &identity.digest_key_id,
                companion_generation: identity.companion_generation,
            },
            key,
            limits,
        )
        .map_err(SnapshotProducerError::Tail)?;
        self.message_sequence = self
            .message_sequence
            .checked_add(1)
            .ok_or(SnapshotProducerError::SequenceOverflow)?;
        Ok(EncodedTail { frame, outcome })
    }

    fn require_identity(&self, identity: &ProducerIdentity) -> Result<(), SnapshotProducerError> {
        if identity != &self.identity {
            return Err(SnapshotProducerError::IdentityChanged);
        }
        Ok(())
    }
}

async fn read_authenticated_hello(
    stream: &mut UnixStream,
    secret: &SnapshotSessionSecret,
    limits: SnapshotSessionLimits,
) -> Result<SnapshotSessionChallenge, SnapshotProducerError> {
    let hello_len = encode_client_hello(SnapshotSessionChallenge::new([0; 32]), secret, limits)
        .map_err(SnapshotProducerError::Session)?
        .len();
    let mut hello = vec![0; hello_len];
    match stream.read_exact(&mut hello).await {
        Ok(_) => {
            decode_client_hello(&hello, secret, limits).map_err(SnapshotProducerError::Session)
        }
        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => {
            Err(SnapshotProducerError::Truncated)
        }
        Err(_) => Err(SnapshotProducerError::Io),
    }
}

fn verify_peer(stream: &UnixStream, expected_uid: u32) -> Result<(), SnapshotProducerError> {
    let credential = stream
        .peer_cred()
        .map_err(|_| SnapshotProducerError::PeerCredentialFailed)?;
    if credential.uid() != expected_uid {
        return Err(SnapshotProducerError::PeerUidMismatch);
    }
    Ok(())
}

async fn read_client_signal(reader: &mut OwnedReadHalf) -> SnapshotProducerError {
    let mut byte = [0_u8; 1];
    match reader.read(&mut byte).await {
        Ok(0) => SnapshotProducerError::ClientDisconnected,
        Ok(_) => SnapshotProducerError::ProtocolViolation,
        Err(_) => SnapshotProducerError::Io,
    }
}

async fn write_frame(
    writer: &mut OwnedWriteHalf,
    frame: &[u8],
) -> Result<(), SnapshotProducerError> {
    writer
        .write_all(frame)
        .await
        .map_err(|_| SnapshotProducerError::ClientDisconnected)
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use tokio::io::AsyncReadExt;

    use super::*;
    use crate::{
        snapshot_session::{
            SNAPSHOT_RESPONSE_LENGTH_PREFIX_BYTES, SnapshotSessionExpectations,
            authenticated_snapshot_frame_length, decode_authenticated_snapshot,
        },
        snapshot_tail::{SnapshotAction, SnapshotTailFence},
        snapshot_tail_wire::{
            TAIL_FRAME_LENGTH_PREFIX_BYTES, TailFrameDecoder, TailSessionExpectations,
            TailWireError, VerifiedTailAction, tail_frame_length,
        },
    };

    const SECRET: [u8; 32] = *b"snapshot-session-secret-32-byte!";
    const KEY_ID: [u8; 32] = [0x61; 32];
    const CHALLENGE: SnapshotSessionChallenge = SnapshotSessionChallenge::new([0x41; 32]);

    fn identity(name: &str, generation: u64) -> ProducerIdentity {
        ProducerIdentity {
            engine_incarnation: EngineIncarnation {
                engine_id: name.into(),
                model_revision: "revision".into(),
                image_digest: "sha256:image".into(),
                process_started_unix_ns: 42,
                attestation_sha256: vec![3; 32],
            },
            digest_key_id: KEY_ID,
            companion_generation: generation,
        }
    }

    struct ScriptedSource {
        snapshot: Mutex<Option<ProducerSnapshot>>,
        events: Mutex<Option<Vec<ProducerTailEvent>>>,
        cancelled: Arc<AtomicBool>,
        pending_build: bool,
    }

    impl SnapshotProducerSource for ScriptedSource {
        fn start(
            &self,
            publisher: SnapshotTailPublisher,
            mut cancellation: SnapshotProducerCancellation,
        ) -> Result<SnapshotBuildFuture, SnapshotProducerSourceError> {
            let events = self.events.lock().unwrap().take().unwrap_or_default();
            let cancelled = Arc::clone(&self.cancelled);
            tokio::spawn(async move {
                for event in events {
                    if publisher.send(event).await.is_err() {
                        break;
                    }
                }
                cancellation.cancelled().await;
                cancelled.store(true, Ordering::Release);
            });
            if self.pending_build {
                Ok(Box::pin(std::future::pending()))
            } else {
                let snapshot = self.snapshot.lock().unwrap().take().unwrap();
                Ok(Box::pin(async move { Ok(snapshot) }))
            }
        }
    }

    fn source(snapshot: ProducerSnapshot, events: Vec<ProducerTailEvent>) -> Arc<ScriptedSource> {
        Arc::new(ScriptedSource {
            snapshot: Mutex::new(Some(snapshot)),
            events: Mutex::new(Some(events)),
            cancelled: Arc::new(AtomicBool::new(false)),
            pending_build: false,
        })
    }

    fn config(uid: u32) -> SnapshotProducerConfig {
        SnapshotProducerConfig {
            expected_peer_uid: uid,
            session_limits: SnapshotSessionLimits::default(),
            tail_limits: TailWireLimits::default(),
            tail_queue_capacity: 2,
        }
    }

    fn snapshot(identity: ProducerIdentity, watermark: u64) -> ProducerSnapshot {
        ProducerSnapshot {
            identity,
            watermark,
            frame: b"opaque-snapshot".to_vec(),
        }
    }

    async fn read_snapshot(stream: &mut UnixStream, limits: SnapshotSessionLimits) -> Vec<u8> {
        let mut frame = vec![0; SNAPSHOT_RESPONSE_LENGTH_PREFIX_BYTES];
        stream.read_exact(&mut frame).await.unwrap();
        let total = authenticated_snapshot_frame_length(&frame, limits).unwrap();
        frame.resize(total, 0);
        stream
            .read_exact(&mut frame[SNAPSHOT_RESPONSE_LENGTH_PREFIX_BYTES..])
            .await
            .unwrap();
        frame
    }

    async fn read_tail(stream: &mut UnixStream, limits: TailWireLimits) -> Vec<u8> {
        let mut frame = vec![0; TAIL_FRAME_LENGTH_PREFIX_BYTES];
        stream.read_exact(&mut frame).await.unwrap();
        let total = tail_frame_length(&frame, limits).unwrap();
        frame.resize(total, 0);
        stream
            .read_exact(&mut frame[TAIL_FRAME_LENGTH_PREFIX_BYTES..])
            .await
            .unwrap();
        frame
    }

    async fn hello(stream: &mut UnixStream, secret: &SnapshotSessionSecret) {
        let frame =
            encode_client_hello(CHALLENGE, secret, SnapshotSessionLimits::default()).unwrap();
        stream.write_all(&frame).await.unwrap();
    }

    fn producer(uid: u32, source: Arc<dyn SnapshotProducerSource>) -> SnapshotProducer {
        SnapshotProducer::new(
            config(uid),
            Arc::new(SnapshotSessionSecret::new(SECRET)),
            source,
        )
        .unwrap()
    }

    fn tail_decoder<'a>(
        key: &'a TailSessionKey,
        identity: &'a ProducerIdentity,
    ) -> TailFrameDecoder<'a> {
        TailFrameDecoder::new(
            key,
            TailSessionExpectations {
                direction: TailDirection::CompanionToRouter,
                session_id: CHALLENGE,
                first_message_sequence: 1,
                engine_incarnation: &identity.engine_incarnation,
                digest_key_id: &KEY_ID,
                companion_generation: identity.companion_generation,
            },
            TailWireLimits::default(),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn streams_snapshot_sparse_events_caught_up_and_disconnect() {
        let (server, mut client) = UnixStream::pair().unwrap();
        let uid = server.peer_cred().unwrap().uid();
        let id = identity("engine-a", 7);
        let events = vec![
            ProducerTailEvent::Batch {
                identity: id.clone(),
                event_watermark: 100,
                payload: b"event-a".to_vec(),
            },
            ProducerTailEvent::CaughtUp {
                identity: id.clone(),
                event_watermark: 100,
            },
            ProducerTailEvent::Batch {
                identity: id.clone(),
                event_watermark: 10_000,
                payload: b"event-b".to_vec(),
            },
            ProducerTailEvent::Disconnect(id.clone()),
        ];
        let producer = producer(uid, source(snapshot(id.clone(), 10), events));
        let task = tokio::spawn(async move {
            producer
                .handle(server, Instant::now() + std::time::Duration::from_secs(2))
                .await
        });
        hello(&mut client, &SnapshotSessionSecret::new(SECRET)).await;
        let response = read_snapshot(&mut client, SnapshotSessionLimits::default()).await;
        let authenticated = decode_authenticated_snapshot(
            &response,
            SnapshotSessionExpectations {
                challenge: CHALLENGE,
                engine_incarnation: &id.engine_incarnation,
                digest_key_id: &KEY_ID,
                minimum_snapshot_watermark: 0,
                minimum_companion_generation: 1,
            },
            &SnapshotSessionSecret::new(SECRET),
            SnapshotSessionLimits::default(),
        )
        .unwrap();
        let mut lifecycle = SnapshotTailFence::start_bootstrap(
            id.engine_incarnation.clone(),
            0,
            KEY_ID.to_vec(),
            7,
        );
        assert!(matches!(
            lifecycle.accept_snapshot(
                &authenticated,
                crate::kv_snapshot::ResetScope::full_engine()
            ),
            SnapshotAction::Accepted { .. }
        ));
        let key = TailSessionKey::derive(
            &SnapshotSessionSecret::new(SECRET),
            CHALLENGE,
            7,
            TailDirection::CompanionToRouter,
        );
        let mut decoder = tail_decoder(&key, &id);
        let first = decoder
            .decode(&read_tail(&mut client, TailWireLimits::default()).await)
            .unwrap();
        assert!(matches!(
            first.apply_to(&mut lifecycle),
            VerifiedTailAction::Apply {
                delivery_sequence: 1,
                event_watermark: 100,
                ref payload,
            } if payload == b"event-a"
        ));
        let caught_up = decoder
            .decode(&read_tail(&mut client, TailWireLimits::default()).await)
            .unwrap();
        assert!(matches!(
            caught_up.apply_to(&mut lifecycle),
            VerifiedTailAction::Ready
        ));
        let second = decoder
            .decode(&read_tail(&mut client, TailWireLimits::default()).await)
            .unwrap();
        assert!(matches!(
            second.apply_to(&mut lifecycle),
            VerifiedTailAction::Apply {
                delivery_sequence: 2,
                event_watermark: 10_000,
                ref payload,
            } if payload == b"event-b"
        ));
        let disconnected = decoder
            .decode(&read_tail(&mut client, TailWireLimits::default()).await)
            .unwrap();
        assert!(matches!(
            disconnected.apply_to(&mut lifecycle),
            VerifiedTailAction::Fenced(crate::snapshot_tail::SnapshotTailFenceReason::Disconnected)
        ));
        assert!(task.await.unwrap().is_ok());
    }

    #[tokio::test]
    async fn bad_hello_and_wrong_peer_never_start_source() {
        let (server, mut client) = UnixStream::pair().unwrap();
        let uid = server.peer_cred().unwrap().uid();
        let bad_source = source(snapshot(identity("engine", 1), 0), Vec::new());
        let untouched = Arc::clone(&bad_source);
        let bad_producer = producer(uid, bad_source);
        let task = tokio::spawn(async move {
            bad_producer
                .handle(server, Instant::now() + std::time::Duration::from_secs(1))
                .await
        });
        let mut bad = encode_client_hello(
            CHALLENGE,
            &SnapshotSessionSecret::new(SECRET),
            SnapshotSessionLimits::default(),
        )
        .unwrap();
        let last = bad.len() - 1;
        bad[last] ^= 1;
        client.write_all(&bad).await.unwrap();
        assert!(matches!(
            task.await.unwrap(),
            Err(SnapshotProducerError::Session(_))
        ));
        assert!(untouched.snapshot.lock().unwrap().is_some());

        let (server, _client) = UnixStream::pair().unwrap();
        let actual = server.peer_cred().unwrap().uid();
        let source = source(snapshot(identity("engine", 1), 0), Vec::new());
        let untouched = Arc::clone(&source);
        let error = producer(actual.wrapping_add(1), source)
            .handle(server, Instant::now() + std::time::Duration::from_secs(1))
            .await
            .unwrap_err();
        assert!(matches!(error, SnapshotProducerError::PeerUidMismatch));
        assert!(untouched.snapshot.lock().unwrap().is_some());
    }

    #[tokio::test]
    async fn client_drop_cancels_pending_build_and_tail_producer() {
        let (server, mut client) = UnixStream::pair().unwrap();
        let uid = server.peer_cred().unwrap().uid();
        let source = Arc::new(ScriptedSource {
            snapshot: Mutex::new(None),
            events: Mutex::new(Some(Vec::new())),
            cancelled: Arc::new(AtomicBool::new(false)),
            pending_build: true,
        });
        let cancelled = Arc::clone(&source.cancelled);
        let producer = producer(uid, source);
        let task = tokio::spawn(async move {
            producer
                .handle(server, Instant::now() + std::time::Duration::from_secs(2))
                .await
        });
        hello(&mut client, &SnapshotSessionSecret::new(SECRET)).await;
        drop(client);
        assert!(matches!(
            task.await.unwrap(),
            Err(SnapshotProducerError::ClientDisconnected)
        ));
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while !cancelled.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn one_absolute_deadline_cancels_pending_source() {
        let (server, mut client) = UnixStream::pair().unwrap();
        let uid = server.peer_cred().unwrap().uid();
        let source = Arc::new(ScriptedSource {
            snapshot: Mutex::new(None),
            events: Mutex::new(Some(Vec::new())),
            cancelled: Arc::new(AtomicBool::new(false)),
            pending_build: true,
        });
        let cancelled = Arc::clone(&source.cancelled);
        let producer = producer(uid, source);
        let task = tokio::spawn(async move {
            producer
                .handle(
                    server,
                    Instant::now() + std::time::Duration::from_millis(30),
                )
                .await
        });
        hello(&mut client, &SnapshotSessionSecret::new(SECRET)).await;
        assert!(matches!(
            task.await.unwrap(),
            Err(SnapshotProducerError::Timeout)
        ));
        assert!(cancelled.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn bounded_queue_reports_backpressure() {
        let (sender, _receiver) = mpsc::channel(1);
        let (cancel_sender, cancel_receiver) = watch::channel(false);
        let publisher = SnapshotTailPublisher {
            sender,
            cancellation: SnapshotProducerCancellation {
                cancelled: Arc::new(AtomicBool::new(false)),
                changed: cancel_receiver,
            },
            max_payload_bytes: 8,
        };
        let id = identity("engine", 1);
        publisher
            .try_send(ProducerTailEvent::Disconnect(id.clone()))
            .unwrap();
        assert_eq!(
            publisher
                .try_send(ProducerTailEvent::Disconnect(id))
                .unwrap_err(),
            SnapshotProducerSourceError::QueueFull
        );
        assert_eq!(
            publisher
                .try_send(ProducerTailEvent::Batch {
                    identity: identity("engine", 1),
                    event_watermark: 1,
                    payload: vec![0; 9],
                })
                .unwrap_err(),
            SnapshotProducerSourceError::PayloadTooLarge
        );
        drop(cancel_sender);
    }

    #[tokio::test]
    async fn slow_reader_hits_same_deadline_and_cancels_backpressured_source() {
        let (server, mut client) = UnixStream::pair().unwrap();
        let uid = server.peer_cred().unwrap().uid();
        let id = identity("engine", 1);
        let events = (1..=12)
            .map(|sequence| ProducerTailEvent::Batch {
                identity: id.clone(),
                event_watermark: sequence * 100,
                payload: vec![0x5a; 1024 * 1024],
            })
            .collect();
        let source = source(snapshot(id, 0), events);
        let cancelled = Arc::clone(&source.cancelled);
        let producer = producer(uid, source);
        let task = tokio::spawn(async move {
            producer
                .handle(
                    server,
                    Instant::now() + std::time::Duration::from_millis(100),
                )
                .await
        });
        hello(&mut client, &SnapshotSessionSecret::new(SECRET)).await;
        let _ = read_snapshot(&mut client, SnapshotSessionLimits::default()).await;
        // Deliberately stop reading before the large tail. The socket write and
        // bounded source queue must remain inside the original session budget.
        assert!(matches!(
            task.await.unwrap(),
            Err(SnapshotProducerError::Timeout)
        ));
        assert!(cancelled.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn identity_rollover_sends_fencing_control_then_ends() {
        let (server, mut client) = UnixStream::pair().unwrap();
        let uid = server.peer_cred().unwrap().uid();
        let old = identity("old", 1);
        let new = identity("new", 2);
        let producer = producer(
            uid,
            source(
                snapshot(old.clone(), 4),
                vec![ProducerTailEvent::IdentityChanged(new)],
            ),
        );
        let task = tokio::spawn(async move {
            producer
                .handle(server, Instant::now() + std::time::Duration::from_secs(1))
                .await
        });
        hello(&mut client, &SnapshotSessionSecret::new(SECRET)).await;
        let _ = read_snapshot(&mut client, SnapshotSessionLimits::default()).await;
        let frame = read_tail(&mut client, TailWireLimits::default()).await;
        let key = TailSessionKey::derive(
            &SnapshotSessionSecret::new(SECRET),
            CHALLENGE,
            1,
            TailDirection::CompanionToRouter,
        );
        let mut decoder = tail_decoder(&key, &old);
        assert!(matches!(
            decoder.decode(&frame),
            Err(TailWireError::IncarnationMismatch)
        ));
        assert!(matches!(
            task.await.unwrap(),
            Err(SnapshotProducerError::IdentityChanged)
        ));
    }
}
