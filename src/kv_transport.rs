//! ZMQ transport for vLLM KV-event live and replay endpoints.
//!
//! vLLM publishes `(topic, sequence, payload)` and exposes replay through a
//! ROUTER socket. Live delivery uses async pure-Rust ZMTP. Replay uses libzmq
//! on a bounded blocking worker: vLLM can synchronously burst tens of MB from
//! its replay buffer, a workload the async implementation does not drain
//! reliably. DEALER is required because one request has multiple streamed
//! replies before the explicit end marker.

use std::{
    sync::{Arc, Weak},
    time::{Duration, Instant},
};

use bytes::Bytes;
use futures::{StreamExt, channel::mpsc};
use thiserror::Error;
use zeromq::{Socket, SocketEvent, SocketOptions, SocketRecv, SubSocket, ZmqMessage};

use crate::kv_wire::{DecodeError, KvEventBatch, KvWireLimits, decode_batch};

const END_SEQUENCE: [u8; 8] = [u8::MAX; 8];
const REPLAY_CANCEL_POLL: Duration = Duration::from_millis(50);

#[derive(Clone, Debug)]
pub struct KvTransportConfig {
    pub live_endpoint: String,
    pub replay_endpoint: Option<String>,
    pub topic: String,
    pub connect_timeout: Duration,
    pub replay_timeout: Duration,
    pub max_replay_batches: usize,
    pub max_replay_tail_batches: usize,
    pub wire_limits: KvWireLimits,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SequencedBatch {
    pub sequence: u64,
    /// Original bounded `MessagePack` payload, retained so an authenticated
    /// local tail can forward the exact vLLM bytes after applying `batch`.
    pub payload: Bytes,
    pub batch: KvEventBatch,
}

#[derive(Clone, Debug, PartialEq)]
pub enum LiveActivity {
    Connected,
    Disconnected,
    Batch(SequencedBatch),
}

/// Content-free timing and volume diagnostics for the most recent replay.
///
/// The profile deliberately contains no event payloads, token IDs, hashes, or
/// endpoint identities. Consumers can safely expose it through bounded metric
/// labels even when a replay fails before its end marker arrives.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ReplayProfile {
    pub elapsed: Duration,
    pub time_to_first_frame: Option<Duration>,
    pub receive_wait: Duration,
    pub decode: Duration,
    pub fold: Duration,
    pub max_receive_gap: Duration,
    pub wire_bytes: usize,
    pub payload_bytes: usize,
    pub messages: usize,
    pub requested_batches: usize,
    pub tail_batches: usize,
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum KvTransportError {
    #[error("KV-event socket operation failed")]
    Socket,
    #[error("KV-event message has an invalid frame count")]
    InvalidFrameCount,
    #[error("KV-event message topic does not match the configured subscription")]
    TopicMismatch,
    #[error("KV-event message has an invalid sequence frame")]
    InvalidSequence,
    #[error("KV-event payload is invalid: {0}")]
    InvalidPayload(DecodeError),
    #[error("KV-event replay endpoint is not configured")]
    ReplayUnavailable,
    #[error("KV-event replay request exceeds its configured batch limit")]
    ReplayTooLarge,
    #[error("KV-event replay timed out and its response could not be drained")]
    ReplayTimeoutUndrained,
    #[error("KV-event replay response is incomplete or out of order")]
    InvalidReplay,
    #[error("KV-event replay was cancelled")]
    ReplayCancelled,
}

impl From<DecodeError> for KvTransportError {
    fn from(error: DecodeError) -> Self {
        Self::InvalidPayload(error)
    }
}

impl KvTransportError {
    #[must_use]
    pub const fn reason(&self) -> &'static str {
        match self {
            Self::Socket => "socket_error",
            Self::InvalidFrameCount => "invalid_frame_count",
            Self::TopicMismatch => "topic_mismatch",
            Self::InvalidSequence => "invalid_sequence",
            Self::InvalidPayload(error) => error.reason(),
            Self::ReplayUnavailable => "replay_unavailable",
            Self::ReplayTooLarge => "replay_too_large",
            Self::ReplayTimeoutUndrained => "replay_timeout_undrained",
            Self::InvalidReplay => "invalid_replay",
            Self::ReplayCancelled => "replay_cancelled",
        }
    }
}

pub struct ZmqKvEventSource {
    live: SubSocket,
    live_monitor: mpsc::Receiver<SocketEvent>,
    replay_endpoint: Option<String>,
    topic: Bytes,
    connect_timeout: Duration,
    replay_timeout: Duration,
    max_replay_batches: usize,
    max_replay_tail_batches: usize,
    wire_limits: KvWireLimits,
    last_replay_profile: Option<ReplayProfile>,
}

impl ZmqKvEventSource {
    /// Connect to the live SUB endpoint and optional replay ROUTER endpoint.
    ///
    /// # Errors
    ///
    /// Returns [`KvTransportError::Socket`] if either configured connection or
    /// the live subscription cannot be established.
    pub async fn connect(config: KvTransportConfig) -> Result<Self, KvTransportError> {
        let mut socket_options = SocketOptions::default();
        socket_options.connect_timeout(config.connect_timeout);

        let mut live = SubSocket::with_options(socket_options);
        let live_monitor = live.monitor();
        live.connect(&config.live_endpoint)
            .await
            .map_err(|_| KvTransportError::Socket)?;
        live.subscribe(&config.topic)
            .await
            .map_err(|_| KvTransportError::Socket)?;

        Ok(Self {
            live,
            live_monitor,
            replay_endpoint: config.replay_endpoint,
            topic: Bytes::from(config.topic),
            connect_timeout: config.connect_timeout,
            replay_timeout: config.replay_timeout,
            max_replay_batches: config.max_replay_batches,
            max_replay_tail_batches: config.max_replay_tail_batches,
            wire_limits: config.wire_limits,
            last_replay_profile: None,
        })
    }

    /// Take diagnostics recorded by the last completed replay exchange.
    ///
    /// A profile is available for both success and transport/validation
    /// failure after the blocking worker returns. Preflight rejections that do
    /// not open a replay socket have no profile.
    pub fn take_replay_profile(&mut self) -> Option<ReplayProfile> {
        self.last_replay_profile.take()
    }

    /// Receive and decode one live event batch.
    ///
    /// # Errors
    ///
    /// Returns [`KvTransportError`] for socket, framing, topic, sequence, or
    /// bounded payload-decode failures. Errors never contain event payloads.
    pub async fn recv_live(&mut self) -> Result<SequencedBatch, KvTransportError> {
        let message = self
            .live
            .recv()
            .await
            .map_err(|_| KvTransportError::Socket)?;
        parse_live_message(&message, &self.topic, self.wire_limits)
    }

    /// Receive either a decoded live batch or a transport connection-state
    /// transition. Monitoring is required because SUB sockets reconnect in the
    /// background and a blocked message receive does not surface disconnects.
    ///
    /// # Errors
    ///
    /// Returns [`KvTransportError`] for socket, monitor, framing, or bounded
    /// payload-decode failures.
    pub async fn recv_live_activity(&mut self) -> Result<LiveActivity, KvTransportError> {
        loop {
            tokio::select! {
                message = self.live.recv() => {
                    let message = message.map_err(|_| KvTransportError::Socket)?;
                    return parse_live_message(&message, &self.topic, self.wire_limits)
                        .map(LiveActivity::Batch);
                }
                event = self.live_monitor.next() => match event {
                    Some(SocketEvent::Connected(_, _)) => return Ok(LiveActivity::Connected),
                    Some(SocketEvent::Disconnected(_)) => return Ok(LiveActivity::Disconnected),
                    Some(_) => {}
                    None => return Err(KvTransportError::Socket),
                },
            }
        }
    }

    /// Request and validate an inclusive replay sequence range.
    ///
    /// The vLLM ROUTER may stream batches newer than `through` after the
    /// requested range while servicing the request. Those tail batches are
    /// consumed but omitted: the live SUB socket will deliver them and the
    /// sequence fence will deduplicate them.
    ///
    /// # Errors
    ///
    /// Returns [`KvTransportError`] if replay is unavailable, out of order,
    /// incomplete, oversized, timed out, malformed, or rejected by the socket.
    /// The blocking replay worker drains through the end marker even after a
    /// validation error. This is important because vLLM services replay sends
    /// synchronously on the same thread that publishes new live events.
    pub async fn replay(
        &mut self,
        from: u64,
        through: u64,
    ) -> Result<Vec<SequencedBatch>, KvTransportError> {
        self.replay_fold(from, through, Vec::new(), |batches, batch| {
            batches.push(batch);
        })
        .await
    }

    /// Request a replay while folding each validated batch into bounded state.
    ///
    /// Unlike [`Self::replay`], this never retains decoded batches unless the
    /// caller's accumulator does so. The fold runs on the same blocking worker
    /// that drains libzmq, making it suitable for a large full-generation
    /// replay staged into an exact index one batch at a time.
    ///
    /// # Errors
    ///
    /// Returns the same bounded transport and validation errors as
    /// [`Self::replay`]. The accumulator is returned only after a valid end
    /// marker and complete requested range.
    pub async fn replay_fold<T, F>(
        &mut self,
        from: u64,
        through: u64,
        accumulator: T,
        fold: F,
    ) -> Result<T, KvTransportError>
    where
        T: Send + 'static,
        F: FnMut(&mut T, SequencedBatch) + Send + 'static,
    {
        self.replay_fold_mode(from, through, accumulator, fold, false)
            .await
    }

    /// Stream both the requested replay range and bounded post-range tail
    /// batches received before the explicit end marker.
    ///
    /// This is the safe handoff for a caller that subscribed live before
    /// replay: replay tail closes the interval while the SUB receiver is not
    /// being polled, and later SUB duplicates are sequence-fenced.
    ///
    /// # Errors
    ///
    /// Returns the same bounded transport and validation errors as
    /// [`Self::replay_fold`].
    pub async fn replay_fold_with_tail<T, F>(
        &mut self,
        from: u64,
        through: u64,
        accumulator: T,
        fold: F,
    ) -> Result<T, KvTransportError>
    where
        T: Send + 'static,
        F: FnMut(&mut T, SequencedBatch) + Send + 'static,
    {
        self.replay_fold_mode(from, through, accumulator, fold, true)
            .await
    }

    async fn replay_fold_mode<T, F>(
        &mut self,
        from: u64,
        through: u64,
        accumulator: T,
        fold: F,
        fold_tail: bool,
    ) -> Result<T, KvTransportError>
    where
        T: Send + 'static,
        F: FnMut(&mut T, SequencedBatch) + Send + 'static,
    {
        self.last_replay_profile = None;
        let expected_count = through
            .checked_sub(from)
            .and_then(|distance| distance.checked_add(1))
            .and_then(|count| usize::try_from(count).ok())
            .ok_or(KvTransportError::InvalidReplay)?;
        if expected_count > self.max_replay_batches {
            return Err(KvTransportError::ReplayTooLarge);
        }
        let endpoint = self
            .replay_endpoint
            .clone()
            .ok_or(KvTransportError::ReplayUnavailable)?;
        let max_messages = expected_count
            .checked_add(self.max_replay_tail_batches)
            .and_then(|count| count.checked_add(1))
            .ok_or(KvTransportError::ReplayTooLarge)?;
        let topic = self.topic.clone();
        let limits = self.wire_limits;
        let connect_timeout = self.connect_timeout;
        let replay_timeout = self.replay_timeout;
        // The blocking worker cannot be aborted by dropping its Tokio join
        // handle. Keep the sole strong witness in this async future so a
        // shutdown/client cancellation becomes visible to libzmq promptly.
        let replay_alive = Arc::new(());
        let worker_alive = Arc::downgrade(&replay_alive);
        let (result, profile) = tokio::task::spawn_blocking(move || {
            blocking_replay_exchange(
                &endpoint,
                from,
                through,
                max_messages,
                &topic,
                connect_timeout,
                replay_timeout,
                limits,
                accumulator,
                fold,
                fold_tail,
                &worker_alive,
            )
        })
        .await
        .map_err(|_| KvTransportError::Socket)?;
        drop(replay_alive);
        self.last_replay_profile = Some(profile);
        result
    }
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn blocking_replay_exchange<T, F>(
    endpoint: &str,
    from: u64,
    through: u64,
    max_messages: usize,
    topic: &[u8],
    connect_timeout: Duration,
    replay_timeout: Duration,
    limits: KvWireLimits,
    mut accumulator: T,
    mut fold: F,
    fold_tail: bool,
    replay_alive: &Weak<()>,
) -> (Result<T, KvTransportError>, ReplayProfile)
where
    F: FnMut(&mut T, SequencedBatch),
{
    let started = Instant::now();
    let deadline = started + replay_timeout;
    let mut profile = ReplayProfile::default();
    let result = (|| {
        let context = zmq::Context::new();
        let replay = context
            .socket(zmq::DEALER)
            .map_err(|_| KvTransportError::Socket)?;
        replay.set_linger(0).map_err(|_| KvTransportError::Socket)?;
        replay
            .set_immediate(true)
            .map_err(|_| KvTransportError::Socket)?;
        replay
            .set_connect_timeout(timeout_millis(connect_timeout.min(replay_timeout)))
            .map_err(|_| KvTransportError::Socket)?;
        replay
            .set_rcvhwm(i32::try_from(max_messages.max(1_024)).unwrap_or(i32::MAX))
            .map_err(|_| KvTransportError::Socket)?;
        replay
            .connect(endpoint)
            .map_err(|_| KvTransportError::Socket)?;
        let send_timeout = deadline
            .checked_duration_since(Instant::now())
            .filter(|remaining| !remaining.is_zero())
            .ok_or(KvTransportError::ReplayTimeoutUndrained)?;
        replay
            .set_sndtimeo(timeout_millis(send_timeout))
            .map_err(|_| KvTransportError::Socket)?;
        replay
            .send_multipart([&[][..], &from.to_be_bytes()], 0)
            .map_err(|_| KvTransportError::Socket)?;

        if Instant::now() >= deadline {
            return Err(KvTransportError::ReplayTimeoutUndrained);
        }
        let mut last_requested = None;
        let mut last_tail = None;
        let mut completed_requested_range = false;
        let mut messages = 0_usize;
        let mut validation_error = None;
        let mut current_receive_gap = Duration::ZERO;
        loop {
            if replay_alive.strong_count() == 0 {
                return Err(KvTransportError::ReplayCancelled);
            }
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .filter(|remaining| !remaining.is_zero())
                .ok_or(KvTransportError::ReplayTimeoutUndrained)?;
            let poll_timeout = remaining.min(REPLAY_CANCEL_POLL);
            replay
                .set_rcvtimeo(timeout_millis(poll_timeout))
                .map_err(|_| KvTransportError::Socket)?;
            let receive_started = Instant::now();
            let frames = match replay.recv_multipart(0) {
                Ok(frames) => frames,
                Err(zmq::Error::EAGAIN) => {
                    let waited = receive_started.elapsed();
                    profile.receive_wait = profile.receive_wait.saturating_add(waited);
                    current_receive_gap = current_receive_gap.saturating_add(waited);
                    if replay_alive.strong_count() == 0 {
                        profile.max_receive_gap = profile.max_receive_gap.max(current_receive_gap);
                        return Err(KvTransportError::ReplayCancelled);
                    }
                    if Instant::now() >= deadline {
                        profile.max_receive_gap = profile.max_receive_gap.max(current_receive_gap);
                        return Err(KvTransportError::ReplayTimeoutUndrained);
                    }
                    continue;
                }
                Err(_) => return Err(KvTransportError::Socket),
            };
            let waited = receive_started.elapsed();
            profile.receive_wait = profile.receive_wait.saturating_add(waited);
            current_receive_gap = current_receive_gap.saturating_add(waited);
            profile.max_receive_gap = profile.max_receive_gap.max(current_receive_gap);
            current_receive_gap = Duration::ZERO;
            profile
                .time_to_first_frame
                .get_or_insert_with(|| started.elapsed());
            profile.wire_bytes = profile.wire_bytes.saturating_add(
                frames
                    .iter()
                    .map(Vec::len)
                    .fold(0_usize, usize::saturating_add),
            );
            profile.payload_bytes = profile
                .payload_bytes
                .saturating_add(frames.get(3).map_or(0, Vec::len));
            let decode_started = Instant::now();
            let Ok(message) =
                ZmqMessage::try_from(frames.into_iter().map(Bytes::from).collect::<Vec<_>>())
            else {
                profile.decode = profile.decode.saturating_add(decode_started.elapsed());
                validation_error.get_or_insert(KvTransportError::InvalidFrameCount);
                continue;
            };
            let parsed = match parse_replay_message(&message, topic, limits) {
                Ok(parsed) => parsed,
                Err(error) => {
                    profile.decode = profile.decode.saturating_add(decode_started.elapsed());
                    validation_error.get_or_insert(error);
                    continue;
                }
            };
            profile.decode = profile.decode.saturating_add(decode_started.elapsed());
            match parsed {
                None => {
                    return match validation_error {
                        Some(error) => Err(error),
                        None if completed_requested_range => Ok(accumulator),
                        None => Err(KvTransportError::InvalidReplay),
                    };
                }
                Some(batch) => {
                    messages = messages.saturating_add(1);
                    profile.messages = messages;
                    if messages >= max_messages {
                        validation_error.get_or_insert(KvTransportError::ReplayTooLarge);
                    }
                    if validation_error.is_some() {
                        continue;
                    }
                    if batch.sequence > through && completed_requested_range {
                        if last_tail.is_some_and(|last| batch.sequence <= last) {
                            validation_error.get_or_insert(KvTransportError::InvalidReplay);
                            continue;
                        }
                        last_tail = Some(batch.sequence);
                        profile.tail_batches = profile.tail_batches.saturating_add(1);
                        if fold_tail {
                            let fold_started = Instant::now();
                            fold(&mut accumulator, batch);
                            profile.fold = profile.fold.saturating_add(fold_started.elapsed());
                        }
                        continue;
                    }
                    if batch.sequence < from
                        || batch.sequence > through
                        || last_requested.is_some_and(|last| batch.sequence <= last)
                    {
                        validation_error.get_or_insert(KvTransportError::InvalidReplay);
                        continue;
                    }
                    last_requested = Some(batch.sequence);
                    completed_requested_range = batch.sequence == through;
                    profile.requested_batches = profile.requested_batches.saturating_add(1);
                    let fold_started = Instant::now();
                    fold(&mut accumulator, batch);
                    profile.fold = profile.fold.saturating_add(fold_started.elapsed());
                }
            }
        }
    })();
    profile.elapsed = started.elapsed();
    (result, profile)
}

fn timeout_millis(timeout: Duration) -> i32 {
    i32::try_from(timeout.as_millis().max(1)).unwrap_or(i32::MAX)
}

fn parse_live_message(
    message: &ZmqMessage,
    topic: &[u8],
    limits: KvWireLimits,
) -> Result<SequencedBatch, KvTransportError> {
    if message.len() != 3 {
        return Err(KvTransportError::InvalidFrameCount);
    }
    if message.get(0).is_none_or(|frame| frame.as_ref() != topic) {
        return Err(KvTransportError::TopicMismatch);
    }
    decode_frames(message.get(1), message.get(2), limits)
}

fn parse_replay_message(
    message: &ZmqMessage,
    topic: &[u8],
    limits: KvWireLimits,
) -> Result<Option<SequencedBatch>, KvTransportError> {
    if message.len() != 4 {
        return Err(KvTransportError::InvalidFrameCount);
    }
    if message.get(0).is_none_or(|delimiter| !delimiter.is_empty()) {
        return Err(KvTransportError::InvalidFrameCount);
    }
    let sequence = message.get(2).ok_or(KvTransportError::InvalidFrameCount)?;
    if sequence.as_ref() == END_SEQUENCE {
        let valid_end = message.get(1).is_some_and(Bytes::is_empty)
            && message.get(3).is_some_and(Bytes::is_empty);
        return valid_end
            .then_some(None)
            .ok_or(KvTransportError::InvalidReplay);
    }
    if message.get(1).is_none_or(|frame| frame.as_ref() != topic) {
        return Err(KvTransportError::TopicMismatch);
    }
    decode_frames(Some(sequence), message.get(3), limits).map(Some)
}

fn decode_frames(
    sequence: Option<&Bytes>,
    payload: Option<&Bytes>,
    limits: KvWireLimits,
) -> Result<SequencedBatch, KvTransportError> {
    let sequence: [u8; 8] = sequence
        .ok_or(KvTransportError::InvalidFrameCount)?
        .as_ref()
        .try_into()
        .map_err(|_| KvTransportError::InvalidSequence)?;
    let payload = payload.ok_or(KvTransportError::InvalidFrameCount)?;
    Ok(SequencedBatch {
        sequence: u64::from_be_bytes(sequence),
        payload: payload.clone(),
        batch: decode_batch(payload, limits)?,
    })
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    use zeromq::{PubSocket, RouterSocket, SocketSend};

    use super::*;

    const EMPTY_BATCH: &[u8] = &[
        0x93, 0xcb, 0x3f, 0xf0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x90, 0x00,
    ];

    fn message(frames: Vec<Bytes>) -> ZmqMessage {
        ZmqMessage::try_from(frames).unwrap()
    }

    #[test]
    fn parses_live_vllm_framing() {
        let live = message(vec![
            Bytes::from_static(b"kv"),
            Bytes::copy_from_slice(&42_u64.to_be_bytes()),
            Bytes::from_static(EMPTY_BATCH),
        ]);
        let parsed = parse_live_message(&live, b"kv", KvWireLimits::default()).unwrap();
        assert_eq!(parsed.sequence, 42);
        assert_eq!(parsed.payload.as_ref(), EMPTY_BATCH);
        assert!(parsed.batch.events.is_empty());
        assert_eq!(parsed.batch.data_parallel_rank, Some(0));
    }

    #[test]
    fn parses_dealer_replay_framing_and_end_marker() {
        let replay = message(vec![
            Bytes::new(),
            Bytes::from_static(b"kv"),
            Bytes::copy_from_slice(&7_u64.to_be_bytes()),
            Bytes::from_static(EMPTY_BATCH),
        ]);
        let parsed = parse_replay_message(&replay, b"kv", KvWireLimits::default())
            .unwrap()
            .unwrap();
        assert_eq!(parsed.sequence, 7);
        assert_eq!(parsed.payload.as_ref(), EMPTY_BATCH);
        let end = message(vec![
            Bytes::new(),
            Bytes::new(),
            Bytes::from_static(&END_SEQUENCE),
            Bytes::new(),
        ]);
        assert_eq!(
            parse_replay_message(&end, b"kv", KvWireLimits::default()).unwrap(),
            None
        );
    }

    #[test]
    fn framing_errors_do_not_include_payload_data() {
        let wrong_topic = message(vec![
            Bytes::from_static(b"other"),
            Bytes::copy_from_slice(&1_u64.to_be_bytes()),
            Bytes::from_static(EMPTY_BATCH),
        ]);
        let error = parse_live_message(&wrong_topic, b"kv", KvWireLimits::default()).unwrap_err();
        assert_eq!(error, KvTransportError::TopicMismatch);
        assert_eq!(
            error.to_string(),
            "KV-event message topic does not match the configured subscription"
        );

        let short_sequence = message(vec![
            Bytes::from_static(b"kv"),
            Bytes::from_static(b"short"),
            Bytes::from_static(EMPTY_BATCH),
        ]);
        assert_eq!(
            parse_live_message(&short_sequence, b"kv", KvWireLimits::default()),
            Err(KvTransportError::InvalidSequence)
        );
    }

    #[tokio::test]
    async fn async_live_and_libzmq_streaming_replay_interoperate() {
        let mut publisher = PubSocket::new();
        let live_endpoint = publisher.bind("tcp://127.0.0.1:0").await.unwrap();
        let mut replay_server = RouterSocket::new();
        let replay_endpoint = replay_server.bind("tcp://127.0.0.1:0").await.unwrap();
        let mut source = ZmqKvEventSource::connect(KvTransportConfig {
            live_endpoint: live_endpoint.to_string(),
            replay_endpoint: Some(replay_endpoint.to_string()),
            topic: "kv".to_owned(),
            connect_timeout: Duration::from_secs(2),
            replay_timeout: Duration::from_secs(5),
            max_replay_batches: 8,
            max_replay_tail_batches: 2,
            wire_limits: KvWireLimits::default(),
        })
        .await
        .unwrap();

        assert!(matches!(
            source.recv_live_activity().await.unwrap(),
            LiveActivity::Connected
        ));
        // The monitor reports the TCP handshake before the SUB subscription
        // command has necessarily reached the publisher (ZMQ slow joiner).
        tokio::time::sleep(Duration::from_millis(20)).await;

        publisher
            .send(message(vec![
                Bytes::from_static(b"kv"),
                Bytes::copy_from_slice(&4_u64.to_be_bytes()),
                Bytes::from_static(EMPTY_BATCH),
            ]))
            .await
            .unwrap();
        let LiveActivity::Batch(live) = source.recv_live_activity().await.unwrap() else {
            panic!("expected a live batch");
        };
        assert_eq!(live.sequence, 4);

        let (replay_drained_tx, replay_drained_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let request = replay_server.recv().await.unwrap();
            assert_eq!(request.len(), 3);
            assert!(request.get(1).unwrap().is_empty());
            assert_eq!(request.get(2).unwrap().as_ref(), 5_u64.to_be_bytes());
            let identity = request.get(0).unwrap().clone();
            tokio::time::sleep(Duration::from_millis(25)).await;
            // Sequence 5 emitted no KV event and is legitimately absent from
            // the retained publisher stream; sequence 7 is a live tail.
            for sequence in [6_u64, 7] {
                replay_server
                    .send(message(vec![
                        identity.clone(),
                        Bytes::new(),
                        Bytes::from_static(b"kv"),
                        Bytes::copy_from_slice(&sequence.to_be_bytes()),
                        Bytes::from_static(EMPTY_BATCH),
                    ]))
                    .await
                    .unwrap();
            }
            replay_server
                .send(message(vec![
                    identity,
                    Bytes::new(),
                    Bytes::new(),
                    Bytes::from_static(&END_SEQUENCE),
                    Bytes::new(),
                ]))
                .await
                .unwrap();
            // `send` queues data to the socket actor; retain the ROUTER until
            // the real client confirms that it drained the end marker.
            replay_drained_rx.await.unwrap();
        });
        let replay = source
            .replay_fold(5, 6, Vec::new(), |sequences, batch| {
                std::thread::sleep(Duration::from_millis(10));
                sequences.push(batch.sequence);
            })
            .await
            .unwrap();
        replay_drained_tx.send(()).unwrap();
        assert_eq!(replay, vec![6]);
        server.await.unwrap();
        let profile = source.take_replay_profile().unwrap();
        assert_eq!(profile.messages, 2);
        assert_eq!(profile.requested_batches, 1);
        assert_eq!(profile.tail_batches, 1);
        assert_eq!(profile.payload_bytes, EMPTY_BATCH.len() * 2);
        assert!(profile.wire_bytes > profile.payload_bytes);
        assert!(profile.time_to_first_frame.unwrap() >= Duration::from_millis(20));
        assert!(profile.fold >= Duration::from_millis(5));

        assert_eq!(
            source.replay(7, 6).await,
            Err(KvTransportError::InvalidReplay)
        );
        assert_eq!(source.take_replay_profile(), None);
        drop(publisher);
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(2), source.recv_live_activity())
                .await
                .unwrap()
                .unwrap(),
            LiveActivity::Disconnected
        ));
    }

    #[tokio::test]
    async fn replay_rejects_tail_before_requested_range_is_complete() {
        let mut publisher = PubSocket::new();
        let live_endpoint = publisher.bind("tcp://127.0.0.1:0").await.unwrap();
        let mut replay_server = RouterSocket::new();
        let replay_endpoint = replay_server.bind("tcp://127.0.0.1:0").await.unwrap();
        let mut source = ZmqKvEventSource::connect(KvTransportConfig {
            live_endpoint: live_endpoint.to_string(),
            replay_endpoint: Some(replay_endpoint.to_string()),
            topic: "kv".to_owned(),
            connect_timeout: Duration::from_secs(2),
            replay_timeout: Duration::from_secs(2),
            max_replay_batches: 8,
            max_replay_tail_batches: 2,
            wire_limits: KvWireLimits::default(),
        })
        .await
        .unwrap();

        let (replay_drained_tx, replay_drained_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let request = replay_server.recv().await.unwrap();
            let identity = request.get(0).unwrap().clone();
            replay_server
                .send(message(vec![
                    identity.clone(),
                    Bytes::new(),
                    Bytes::from_static(b"kv"),
                    Bytes::copy_from_slice(&7_u64.to_be_bytes()),
                    Bytes::from_static(EMPTY_BATCH),
                ]))
                .await
                .unwrap();
            replay_server
                .send(message(vec![
                    identity,
                    Bytes::new(),
                    Bytes::new(),
                    Bytes::from_static(&END_SEQUENCE),
                    Bytes::new(),
                ]))
                .await
                .unwrap();
            replay_drained_rx.await.unwrap();
        });

        let replay = source.replay(5, 6).await;
        replay_drained_tx.send(()).unwrap();
        assert_eq!(replay, Err(KvTransportError::InvalidReplay));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn timed_out_blocking_replay_is_bounded() {
        let mut publisher = PubSocket::new();
        let live_endpoint = publisher.bind("tcp://127.0.0.1:0").await.unwrap();
        let mut replay_server = RouterSocket::new();
        let replay_endpoint = replay_server.bind("tcp://127.0.0.1:0").await.unwrap();
        let mut source = ZmqKvEventSource::connect(KvTransportConfig {
            live_endpoint: live_endpoint.to_string(),
            replay_endpoint: Some(replay_endpoint.to_string()),
            topic: "kv".to_owned(),
            connect_timeout: Duration::from_secs(2),
            replay_timeout: Duration::from_millis(200),
            max_replay_batches: 8,
            max_replay_tail_batches: 2,
            wire_limits: KvWireLimits::default(),
        })
        .await
        .unwrap();

        let server = tokio::spawn(async move {
            let request = replay_server.recv().await.unwrap();
            let identity = request.get(0).unwrap().clone();
            replay_server
                .send(message(vec![
                    identity,
                    Bytes::new(),
                    Bytes::from_static(b"kv"),
                    Bytes::copy_from_slice(&0_u64.to_be_bytes()),
                    Bytes::from_static(EMPTY_BATCH),
                ]))
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_millis(300)).await;
        });

        let started = Instant::now();
        assert_eq!(
            source.replay(0, 0).await,
            Err(KvTransportError::ReplayTimeoutUndrained)
        );
        assert!(started.elapsed() >= Duration::from_millis(150));
        assert!(started.elapsed() < Duration::from_secs(1));
        let profile = source.take_replay_profile().unwrap();
        assert_eq!(profile.messages, 1);
        assert_eq!(profile.requested_batches, 1);
        assert_eq!(profile.payload_bytes, EMPTY_BATCH.len());
        assert!(profile.time_to_first_frame.is_some());
        // Socket setup and executor scheduling share the same absolute replay
        // deadline. Under a loaded CI runner they can legitimately consume
        // most of it before the receive loop starts, so receive-only telemetry
        // has no deterministic lower bound. The end-to-end assertions above
        // are the timeout contract; these assertions cover telemetry shape.
        assert!(profile.receive_wait > Duration::ZERO);
        assert!(profile.max_receive_gap > Duration::ZERO);
        assert!(profile.max_receive_gap <= profile.receive_wait);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn stalled_replay_has_a_bounded_drain_window() {
        let mut publisher = PubSocket::new();
        let live_endpoint = publisher.bind("tcp://127.0.0.1:0").await.unwrap();
        let mut replay_server = RouterSocket::new();
        let replay_endpoint = replay_server.bind("tcp://127.0.0.1:0").await.unwrap();
        let mut source = ZmqKvEventSource::connect(KvTransportConfig {
            live_endpoint: live_endpoint.to_string(),
            replay_endpoint: Some(replay_endpoint.to_string()),
            topic: "kv".to_owned(),
            connect_timeout: Duration::from_secs(2),
            replay_timeout: Duration::from_millis(100),
            max_replay_batches: 8,
            max_replay_tail_batches: 2,
            wire_limits: KvWireLimits::default(),
        })
        .await
        .unwrap();

        let server = tokio::spawn(async move {
            let _request = replay_server.recv().await.unwrap();
            tokio::time::sleep(Duration::from_millis(100)).await;
        });
        let started = Instant::now();
        let replay = tokio::time::timeout(Duration::from_millis(200), source.replay(0, 0))
            .await
            .expect("the blocking replay deadline must remain bounded");
        assert_eq!(replay, Err(KvTransportError::ReplayTimeoutUndrained));
        assert!(started.elapsed() >= Duration::from_millis(15));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn dropping_replay_promptly_stops_blocking_worker() {
        #[derive(Debug)]
        struct DropSignal(Arc<AtomicBool>);

        impl Drop for DropSignal {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Release);
            }
        }

        let mut publisher = PubSocket::new();
        let live_endpoint = publisher.bind("tcp://127.0.0.1:0").await.unwrap();
        let mut replay_server = RouterSocket::new();
        let replay_endpoint = replay_server.bind("tcp://127.0.0.1:0").await.unwrap();
        let mut source = ZmqKvEventSource::connect(KvTransportConfig {
            live_endpoint: live_endpoint.to_string(),
            replay_endpoint: Some(replay_endpoint.to_string()),
            topic: "kv".to_owned(),
            connect_timeout: Duration::from_secs(2),
            replay_timeout: Duration::from_secs(5),
            max_replay_batches: 8,
            max_replay_tail_batches: 2,
            wire_limits: KvWireLimits::default(),
        })
        .await
        .unwrap();

        let dropped = Arc::new(AtomicBool::new(false));
        let accumulator = DropSignal(dropped.clone());
        let replay = tokio::spawn(async move {
            source
                .replay_fold(0, 0, accumulator, |_accumulator, _batch| {})
                .await
        });
        tokio::time::timeout(Duration::from_secs(2), replay_server.recv())
            .await
            .expect("blocking worker must send its request")
            .unwrap();

        let started = Instant::now();
        replay.abort();
        let _ = replay.await;
        tokio::time::timeout(Duration::from_millis(500), async {
            while !dropped.load(Ordering::Acquire) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("cancellation must release the blocking replay accumulator");
        assert!(started.elapsed() < Duration::from_millis(250));
    }

    #[tokio::test]
    async fn replay_reconnect_uses_a_fresh_router_identity() {
        let mut publisher = PubSocket::new();
        let live_endpoint = publisher.bind("tcp://127.0.0.1:0").await.unwrap();
        let mut replay_server = RouterSocket::new();
        let replay_endpoint = replay_server.bind("tcp://127.0.0.1:0").await.unwrap();
        let config = KvTransportConfig {
            live_endpoint: live_endpoint.to_string(),
            replay_endpoint: Some(replay_endpoint.to_string()),
            topic: "kv".to_owned(),
            connect_timeout: Duration::from_secs(2),
            replay_timeout: Duration::from_millis(100),
            max_replay_batches: 8,
            max_replay_tail_batches: 2,
            wire_limits: KvWireLimits::default(),
        };

        let mut first = ZmqKvEventSource::connect(config.clone()).await.unwrap();
        let first_replay = tokio::spawn(async move { first.replay(0, 0).await });
        let first_request = tokio::time::timeout(Duration::from_secs(2), replay_server.recv())
            .await
            .unwrap()
            .unwrap();
        let first_identity = first_request.get(0).unwrap().clone();
        assert_eq!(
            first_replay.await.unwrap(),
            Err(KvTransportError::ReplayTimeoutUndrained)
        );

        let mut second = ZmqKvEventSource::connect(config).await.unwrap();
        let second_replay = tokio::spawn(async move { second.replay(0, 0).await });
        let second_request = tokio::time::timeout(Duration::from_secs(2), replay_server.recv())
            .await
            .unwrap()
            .unwrap();
        let second_identity = second_request.get(0).unwrap().clone();
        assert_ne!(first_identity, second_identity);

        replay_server
            .send(message(vec![
                second_identity.clone(),
                Bytes::new(),
                Bytes::from_static(b"kv"),
                Bytes::copy_from_slice(&0_u64.to_be_bytes()),
                Bytes::from_static(EMPTY_BATCH),
            ]))
            .await
            .unwrap();
        replay_server
            .send(message(vec![
                second_identity,
                Bytes::new(),
                Bytes::new(),
                Bytes::from_static(&END_SEQUENCE),
                Bytes::new(),
            ]))
            .await
            .unwrap();
        assert_eq!(second_replay.await.unwrap().unwrap().len(), 1);
    }
}
