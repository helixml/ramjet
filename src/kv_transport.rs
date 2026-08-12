//! ZMQ transport for vLLM KV-event live and replay endpoints.
//!
//! vLLM publishes `(topic, sequence, payload)` and exposes replay through a
//! ROUTER socket. Live delivery uses async pure-Rust ZMTP. Replay uses libzmq
//! on a bounded blocking worker: vLLM can synchronously burst tens of MB from
//! its replay buffer, a workload the async implementation does not drain
//! reliably. DEALER is required because one request has multiple streamed
//! replies before the explicit end marker.

use std::time::{Duration, Instant};

use bytes::Bytes;
use futures::{StreamExt, channel::mpsc};
use thiserror::Error;
use zeromq::{Socket, SocketEvent, SocketOptions, SocketRecv, SubSocket, ZmqMessage};

use crate::kv_wire::{DecodeError, KvEventBatch, KvWireLimits, decode_batch};

const END_SEQUENCE: [u8; 8] = [u8::MAX; 8];

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
    pub batch: KvEventBatch,
}

#[derive(Clone, Debug, PartialEq)]
pub enum LiveActivity {
    Connected,
    Disconnected,
    Batch(SequencedBatch),
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
        })
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
        tokio::task::spawn_blocking(move || {
            blocking_replay_exchange(
                &endpoint,
                from,
                through,
                expected_count,
                max_messages,
                &topic,
                connect_timeout,
                replay_timeout,
                limits,
            )
        })
        .await
        .map_err(|_| KvTransportError::Socket)?
    }
}

#[allow(clippy::too_many_arguments)]
fn blocking_replay_exchange(
    endpoint: &str,
    from: u64,
    through: u64,
    expected_count: usize,
    max_messages: usize,
    topic: &[u8],
    connect_timeout: Duration,
    replay_timeout: Duration,
    limits: KvWireLimits,
) -> Result<Vec<SequencedBatch>, KvTransportError> {
    let context = zmq::Context::new();
    let replay = context
        .socket(zmq::DEALER)
        .map_err(|_| KvTransportError::Socket)?;
    replay.set_linger(0).map_err(|_| KvTransportError::Socket)?;
    replay
        .set_immediate(true)
        .map_err(|_| KvTransportError::Socket)?;
    replay
        .set_connect_timeout(timeout_millis(connect_timeout))
        .map_err(|_| KvTransportError::Socket)?;
    replay
        .set_sndtimeo(timeout_millis(connect_timeout))
        .map_err(|_| KvTransportError::Socket)?;
    replay
        .set_rcvhwm(i32::try_from(max_messages.max(1_024)).unwrap_or(i32::MAX))
        .map_err(|_| KvTransportError::Socket)?;
    replay
        .connect(endpoint)
        .map_err(|_| KvTransportError::Socket)?;
    replay
        .send_multipart([&[][..], &from.to_be_bytes()], 0)
        .map_err(|_| KvTransportError::Socket)?;

    let deadline = Instant::now() + replay_timeout;
    let mut expected = Some(from);
    let mut batches = Vec::with_capacity(expected_count);
    let mut messages = 0_usize;
    let mut validation_error = None;
    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .filter(|remaining| !remaining.is_zero())
            .ok_or(KvTransportError::ReplayTimeoutUndrained)?;
        replay
            .set_rcvtimeo(timeout_millis(remaining))
            .map_err(|_| KvTransportError::Socket)?;
        let frames = match replay.recv_multipart(0) {
            Ok(frames) => frames,
            Err(zmq::Error::EAGAIN) => return Err(KvTransportError::ReplayTimeoutUndrained),
            Err(_) => return Err(KvTransportError::Socket),
        };
        let Ok(message) =
            ZmqMessage::try_from(frames.into_iter().map(Bytes::from).collect::<Vec<_>>())
        else {
            validation_error.get_or_insert(KvTransportError::InvalidFrameCount);
            continue;
        };
        let parsed = match parse_replay_message(&message, topic, limits) {
            Ok(parsed) => parsed,
            Err(error) => {
                validation_error.get_or_insert(error);
                continue;
            }
        };
        match parsed {
            None => {
                return match validation_error {
                    Some(error) => Err(error),
                    None if batches.len() == expected_count => Ok(batches),
                    None => Err(KvTransportError::InvalidReplay),
                };
            }
            Some(batch) => {
                messages = messages.saturating_add(1);
                if messages >= max_messages {
                    validation_error.get_or_insert(KvTransportError::ReplayTooLarge);
                }
                if validation_error.is_some() {
                    continue;
                }
                if batch.sequence > through && expected.is_none() {
                    continue;
                }
                if Some(batch.sequence) != expected {
                    validation_error.get_or_insert(KvTransportError::InvalidReplay);
                    continue;
                }
                expected = if batch.sequence == through {
                    None
                } else {
                    batch.sequence.checked_add(1)
                };
                if expected.is_none() && batch.sequence != through {
                    validation_error.get_or_insert(KvTransportError::InvalidReplay);
                    continue;
                }
                batches.push(batch);
            }
        }
    }
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
        batch: decode_batch(payload, limits)?,
    })
}

#[cfg(test)]
mod tests {
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
        assert_eq!(
            parse_replay_message(&replay, b"kv", KvWireLimits::default())
                .unwrap()
                .unwrap()
                .sequence,
            7
        );
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
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

        let server = tokio::spawn(async move {
            let request = replay_server.recv().await.unwrap();
            assert_eq!(request.len(), 3);
            assert!(request.get(1).unwrap().is_empty());
            assert_eq!(request.get(2).unwrap().as_ref(), 5_u64.to_be_bytes());
            let identity = request.get(0).unwrap().clone();
            for sequence in [5_u64, 6, 7] {
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
        });
        let replay = source.replay(5, 6).await.unwrap();
        assert_eq!(
            replay
                .iter()
                .map(|batch| batch.sequence)
                .collect::<Vec<_>>(),
            vec![5, 6]
        );
        server.await.unwrap();
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
        });

        assert_eq!(
            source.replay(5, 6).await,
            Err(KvTransportError::InvalidReplay)
        );
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
            let _request = replay_server.recv().await.unwrap();
            tokio::time::sleep(Duration::from_millis(300)).await;
        });

        let started = Instant::now();
        assert_eq!(
            source.replay(0, 0).await,
            Err(KvTransportError::ReplayTimeoutUndrained)
        );
        assert!(started.elapsed() >= Duration::from_millis(150));
        assert!(started.elapsed() < Duration::from_secs(1));
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
