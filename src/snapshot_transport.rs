//! Unix-domain transport for one authenticated snapshot exchange.
//!
//! The transport owns each stream for exactly one request. Peer credentials are
//! checked before any protocol bytes are read or written, and one absolute
//! deadline covers connection establishment, authentication, snapshot
//! production, I/O, and decoding. The generic connector deliberately performs
//! no path lookup, removal, or permission changes.

use std::future::Future;
use std::io;
use std::time::Duration;

use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::time::{Instant, timeout_at};

use crate::kv_snapshot::EngineIncarnation;
use crate::snapshot_session::{
    AuthenticatedSnapshot, SNAPSHOT_DIGEST_KEY_ID_BYTES, SnapshotSessionBinding,
    SnapshotSessionChallenge, SnapshotSessionError, SnapshotSessionExpectations,
    SnapshotSessionLimits, SnapshotSessionSecret, decode_authenticated_snapshot,
    decode_client_hello, encode_authenticated_snapshot, encode_client_hello,
};

#[derive(Debug, Error)]
pub enum SnapshotTransportError {
    #[error("snapshot transport timed out")]
    Timeout,
    #[error("snapshot transport I/O failed")]
    Io,
    #[error("snapshot peer credential lookup failed")]
    PeerCredentialFailed,
    #[error("snapshot peer user does not match")]
    PeerUidMismatch,
    #[error("snapshot transport frame was truncated")]
    Truncated,
    #[error("snapshot transport frame exceeds configured byte limit")]
    FrameTooLarge,
    #[error("snapshot client disconnected")]
    ClientDisconnected,
    #[error("snapshot producer failed")]
    ProducerFailed,
    #[error("snapshot session rejected")]
    Session(#[source] SnapshotSessionError),
}

impl SnapshotTransportError {
    #[must_use]
    pub const fn reason(&self) -> &'static str {
        match self {
            Self::Timeout => "timeout",
            Self::Io => "io_failed",
            Self::PeerCredentialFailed => "peer_credential_failed",
            Self::PeerUidMismatch => "peer_uid_mismatch",
            Self::Truncated => "truncated",
            Self::FrameTooLarge => "frame_too_large",
            Self::ClientDisconnected => "client_disconnected",
            Self::ProducerFailed => "producer_failed",
            Self::Session(error) => error.reason(),
        }
    }
}

/// Owned producer result. The session challenge is supplied by the transport
/// only after the authenticated client hello has been accepted.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SnapshotResponse {
    pub engine_incarnation: EngineIncarnation,
    pub snapshot_watermark: u64,
    pub digest_key_id: [u8; SNAPSHOT_DIGEST_KEY_ID_BYTES],
    pub companion_generation: u64,
    pub snapshot_frame: Vec<u8>,
}

/// Establish a stream with a caller-supplied connector and request one
/// authenticated snapshot.
///
/// Supplying the connector rather than a path keeps pathname resolution and
/// socket lifecycle policy outside this generic transport helper.
///
/// # Errors
///
/// Returns a content-free transport or session error on timeout, peer mismatch,
/// malformed input, failed authentication, an I/O failure, or a capacity breach.
pub async fn connect_and_request_snapshot<C>(
    connect: C,
    expected_peer_uid: u32,
    expected: SnapshotSessionExpectations<'_>,
    secret: &SnapshotSessionSecret,
    limits: SnapshotSessionLimits,
    timeout: Duration,
) -> Result<AuthenticatedSnapshot, SnapshotTransportError>
where
    C: Future<Output = io::Result<UnixStream>>,
{
    let deadline = Instant::now() + timeout;
    timeout_at(deadline, async {
        let stream = connect.await.map_err(|_| SnapshotTransportError::Io)?;
        request_snapshot_until(stream, expected_peer_uid, expected, secret, limits).await
    })
    .await
    .map_err(|_| SnapshotTransportError::Timeout)?
}

/// Request one authenticated snapshot on an already-connected Unix stream.
///
/// Dropping this future drops its owned stream, cancelling outstanding I/O and
/// allowing the server to abandon snapshot production.
///
/// # Errors
///
/// Returns a content-free transport or session error on timeout, peer mismatch,
/// malformed input, failed authentication, an I/O failure, or a capacity breach.
pub async fn request_snapshot(
    stream: UnixStream,
    expected_peer_uid: u32,
    expected: SnapshotSessionExpectations<'_>,
    secret: &SnapshotSessionSecret,
    limits: SnapshotSessionLimits,
    timeout: Duration,
) -> Result<AuthenticatedSnapshot, SnapshotTransportError> {
    let deadline = Instant::now() + timeout;
    timeout_at(
        deadline,
        request_snapshot_until(stream, expected_peer_uid, expected, secret, limits),
    )
    .await
    .map_err(|_| SnapshotTransportError::Timeout)?
}

async fn request_snapshot_until(
    mut stream: UnixStream,
    expected_peer_uid: u32,
    expected: SnapshotSessionExpectations<'_>,
    secret: &SnapshotSessionSecret,
    limits: SnapshotSessionLimits,
) -> Result<AuthenticatedSnapshot, SnapshotTransportError> {
    verify_peer(&stream, expected_peer_uid)?;
    let hello = encode_client_hello(expected.challenge, secret, limits)
        .map_err(SnapshotTransportError::Session)?;
    stream
        .write_all(&hello)
        .await
        .map_err(|_| SnapshotTransportError::Io)?;

    // Keep the write half open while awaiting the response. The server uses it
    // to detect a fully dropped client and cancel an in-flight producer future.
    let response = read_bounded_to_eof(&mut stream, limits.max_response_frame_bytes).await?;
    stream
        .shutdown()
        .await
        .map_err(|_| SnapshotTransportError::Io)?;
    decode_authenticated_snapshot(&response, expected, secret, limits)
        .map_err(SnapshotTransportError::Session)
}

/// Serve exactly one authenticated request on an accepted Unix stream.
///
/// `produce` is not invoked until peer credentials and the authenticated hello
/// are valid. If the client disappears while `produce` is pending, its future
/// is dropped immediately.
///
/// # Errors
///
/// Returns a content-free transport or session error on timeout, peer mismatch,
/// malformed input, failed authentication, producer failure, I/O failure, or a
/// capacity breach.
pub async fn serve_one_snapshot<F, Fut, E>(
    stream: UnixStream,
    expected_peer_uid: u32,
    secret: &SnapshotSessionSecret,
    limits: SnapshotSessionLimits,
    timeout: Duration,
    produce: F,
) -> Result<(), SnapshotTransportError>
where
    F: FnOnce(SnapshotSessionChallenge) -> Fut,
    Fut: Future<Output = Result<SnapshotResponse, E>>,
{
    let deadline = Instant::now() + timeout;
    timeout_at(
        deadline,
        serve_one_snapshot_until(stream, expected_peer_uid, secret, limits, produce),
    )
    .await
    .map_err(|_| SnapshotTransportError::Timeout)?
}

async fn serve_one_snapshot_until<F, Fut, E>(
    mut stream: UnixStream,
    expected_peer_uid: u32,
    secret: &SnapshotSessionSecret,
    limits: SnapshotSessionLimits,
    produce: F,
) -> Result<(), SnapshotTransportError>
where
    F: FnOnce(SnapshotSessionChallenge) -> Fut,
    Fut: Future<Output = Result<SnapshotResponse, E>>,
{
    verify_peer(&stream, expected_peer_uid)?;

    // The authenticated hello is fixed-width. Ask the session codec for that
    // width instead of duplicating its private framing offsets here.
    let hello_len = encode_client_hello(SnapshotSessionChallenge::new([0; 32]), secret, limits)
        .map_err(SnapshotTransportError::Session)?
        .len();
    let mut hello = vec![0; hello_len];
    match stream.read_exact(&mut hello).await {
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => {
            return Err(SnapshotTransportError::Truncated);
        }
        Err(_) => return Err(SnapshotTransportError::Io),
    }
    let challenge =
        decode_client_hello(&hello, secret, limits).map_err(SnapshotTransportError::Session)?;

    let mut extra = [0_u8; 1];
    let produced = tokio::select! {
        biased;
        read = stream.read(&mut extra) => match read {
            Ok(0) => return Err(SnapshotTransportError::ClientDisconnected),
            Ok(_) => return Err(SnapshotTransportError::FrameTooLarge),
            Err(_) => return Err(SnapshotTransportError::Io),
        },
        response = produce(challenge) => {
            response.map_err(|_| SnapshotTransportError::ProducerFailed)?
        }
    };

    let frame = encode_authenticated_snapshot(
        &produced.snapshot_frame,
        SnapshotSessionBinding {
            challenge,
            engine_incarnation: &produced.engine_incarnation,
            snapshot_watermark: produced.snapshot_watermark,
            digest_key_id: &produced.digest_key_id,
            companion_generation: produced.companion_generation,
        },
        secret,
        limits,
    )
    .map_err(SnapshotTransportError::Session)?;
    stream
        .write_all(&frame)
        .await
        .map_err(|_| SnapshotTransportError::Io)?;
    stream
        .shutdown()
        .await
        .map_err(|_| SnapshotTransportError::Io)
}

fn verify_peer(stream: &UnixStream, expected_uid: u32) -> Result<(), SnapshotTransportError> {
    let credential = stream
        .peer_cred()
        .map_err(|_| SnapshotTransportError::PeerCredentialFailed)?;
    if credential.uid() != expected_uid {
        return Err(SnapshotTransportError::PeerUidMismatch);
    }
    Ok(())
}

async fn read_bounded_to_eof(
    stream: &mut UnixStream,
    limit: usize,
) -> Result<Vec<u8>, SnapshotTransportError> {
    let read_limit = limit
        .checked_add(1)
        .ok_or(SnapshotTransportError::FrameTooLarge)?;
    let mut bytes = Vec::with_capacity(read_limit.min(8 * 1024));
    stream
        .take(u64::try_from(read_limit).map_err(|_| SnapshotTransportError::FrameTooLarge)?)
        .read_to_end(&mut bytes)
        .await
        .map_err(|_| SnapshotTransportError::Io)?;
    if bytes.len() > limit {
        return Err(SnapshotTransportError::FrameTooLarge);
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use std::convert::Infallible;
    use std::future::pending;

    use tokio::sync::oneshot;

    use super::*;

    const KEY_ID: [u8; SNAPSHOT_DIGEST_KEY_ID_BYTES] = [7; SNAPSHOT_DIGEST_KEY_ID_BYTES];
    const SECRET: [u8; 32] = [11; 32];
    const CHALLENGE: SnapshotSessionChallenge = SnapshotSessionChallenge::new([13; 32]);

    fn incarnation() -> EngineIncarnation {
        EngineIncarnation {
            engine_id: "engine-a".into(),
            model_revision: "revision-a".into(),
            image_digest: "sha256:image-a".into(),
            process_started_unix_ns: 42,
            attestation_sha256: vec![5; 32],
        }
    }

    fn response() -> SnapshotResponse {
        SnapshotResponse {
            engine_incarnation: incarnation(),
            snapshot_watermark: 99,
            digest_key_id: KEY_ID,
            companion_generation: 3,
            snapshot_frame: b"snapshot".to_vec(),
        }
    }

    fn expectations(incarnation: &EngineIncarnation) -> SnapshotSessionExpectations<'_> {
        SnapshotSessionExpectations {
            challenge: CHALLENGE,
            engine_incarnation: incarnation,
            digest_key_id: &KEY_ID,
            minimum_snapshot_watermark: 90,
            minimum_companion_generation: 2,
        }
    }

    fn pair_with_uid() -> (UnixStream, UnixStream, u32) {
        let (client, server) = UnixStream::pair().unwrap();
        let uid = client.peer_cred().unwrap().uid();
        (client, server, uid)
    }

    #[tokio::test]
    async fn authenticated_exchange_succeeds() {
        let (client, server, uid) = pair_with_uid();
        let server_task = tokio::spawn(async move {
            let secret = SnapshotSessionSecret::new(SECRET);
            serve_one_snapshot(
                server,
                uid,
                &secret,
                SnapshotSessionLimits::default(),
                Duration::from_secs(1),
                |_| async { Ok::<_, Infallible>(response()) },
            )
            .await
        });
        let incarnation = incarnation();
        let secret = SnapshotSessionSecret::new(SECRET);
        let snapshot = request_snapshot(
            client,
            uid,
            expectations(&incarnation),
            &secret,
            SnapshotSessionLimits::default(),
            Duration::from_secs(1),
        )
        .await
        .unwrap();
        assert_eq!(snapshot.snapshot_frame(), b"snapshot");
        server_task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn wrong_peer_uid_is_rejected_before_protocol() {
        let (client, _server, uid) = pair_with_uid();
        let incarnation = incarnation();
        let secret = SnapshotSessionSecret::new(SECRET);
        let error = request_snapshot(
            client,
            uid.wrapping_add(1),
            expectations(&incarnation),
            &secret,
            SnapshotSessionLimits::default(),
            Duration::from_secs(1),
        )
        .await
        .unwrap_err();
        assert!(matches!(error, SnapshotTransportError::PeerUidMismatch));
    }

    #[tokio::test]
    async fn wrong_client_key_is_rejected() {
        let (client, server, uid) = pair_with_uid();
        let server_task = tokio::spawn(async move {
            let secret = SnapshotSessionSecret::new(SECRET);
            serve_one_snapshot(
                server,
                uid,
                &secret,
                SnapshotSessionLimits::default(),
                Duration::from_secs(1),
                |_| async { Ok::<_, Infallible>(response()) },
            )
            .await
        });
        let incarnation = incarnation();
        let wrong_secret = SnapshotSessionSecret::new([12; 32]);
        let _ = request_snapshot(
            client,
            uid,
            expectations(&incarnation),
            &wrong_secret,
            SnapshotSessionLimits::default(),
            Duration::from_secs(1),
        )
        .await;
        let error = server_task.await.unwrap().unwrap_err();
        assert!(matches!(
            error,
            SnapshotTransportError::Session(SnapshotSessionError::AuthenticationFailed)
        ));
    }

    async fn consume_hello(stream: &mut UnixStream, secret: &SnapshotSessionSecret) {
        let length = encode_client_hello(
            SnapshotSessionChallenge::new([0; 32]),
            secret,
            SnapshotSessionLimits::default(),
        )
        .unwrap()
        .len();
        let mut hello = vec![0; length];
        stream.read_exact(&mut hello).await.unwrap();
    }

    #[tokio::test]
    async fn truncated_response_is_rejected() {
        let (client, mut server, uid) = pair_with_uid();
        tokio::spawn(async move {
            let secret = SnapshotSessionSecret::new(SECRET);
            consume_hello(&mut server, &secret).await;
            server.write_all(b"short").await.unwrap();
            server.shutdown().await.unwrap();
        });
        let incarnation = incarnation();
        let secret = SnapshotSessionSecret::new(SECRET);
        let error = request_snapshot(
            client,
            uid,
            expectations(&incarnation),
            &secret,
            SnapshotSessionLimits::default(),
            Duration::from_secs(1),
        )
        .await
        .unwrap_err();
        assert!(matches!(
            error,
            SnapshotTransportError::Session(SnapshotSessionError::InvalidFrameLength)
        ));
    }

    #[tokio::test]
    async fn oversized_response_is_detected_with_one_extra_byte() {
        let (client, mut server, uid) = pair_with_uid();
        let limits = SnapshotSessionLimits {
            max_response_frame_bytes: 128,
            ..SnapshotSessionLimits::default()
        };
        tokio::spawn(async move {
            let secret = SnapshotSessionSecret::new(SECRET);
            consume_hello(&mut server, &secret).await;
            server.write_all(&[0; 129]).await.unwrap();
            server.shutdown().await.unwrap();
        });
        let incarnation = incarnation();
        let secret = SnapshotSessionSecret::new(SECRET);
        let error = request_snapshot(
            client,
            uid,
            expectations(&incarnation),
            &secret,
            limits,
            Duration::from_secs(1),
        )
        .await
        .unwrap_err();
        assert!(matches!(error, SnapshotTransportError::FrameTooLarge));
    }

    #[tokio::test]
    async fn slow_response_uses_one_absolute_timeout() {
        let (client, mut server, uid) = pair_with_uid();
        tokio::spawn(async move {
            let secret = SnapshotSessionSecret::new(SECRET);
            consume_hello(&mut server, &secret).await;
            tokio::time::sleep(Duration::from_secs(1)).await;
        });
        let incarnation = incarnation();
        let secret = SnapshotSessionSecret::new(SECRET);
        let error = request_snapshot(
            client,
            uid,
            expectations(&incarnation),
            &secret,
            SnapshotSessionLimits::default(),
            Duration::from_millis(20),
        )
        .await
        .unwrap_err();
        assert!(matches!(error, SnapshotTransportError::Timeout));
    }

    #[tokio::test]
    async fn dropping_client_cancels_server_producer() {
        let (client, server, uid) = pair_with_uid();
        let (started_tx, started_rx) = oneshot::channel();
        let server_task = tokio::spawn(async move {
            let secret = SnapshotSessionSecret::new(SECRET);
            serve_one_snapshot(
                server,
                uid,
                &secret,
                SnapshotSessionLimits::default(),
                Duration::from_secs(5),
                |_| async move {
                    let _ = started_tx.send(());
                    pending::<Result<SnapshotResponse, Infallible>>().await
                },
            )
            .await
        });
        let incarnation = incarnation();
        let client_task = tokio::spawn(async move {
            let secret = SnapshotSessionSecret::new(SECRET);
            request_snapshot(
                client,
                uid,
                expectations(&incarnation),
                &secret,
                SnapshotSessionLimits::default(),
                Duration::from_secs(5),
            )
            .await
        });
        started_rx.await.unwrap();
        client_task.abort();
        let error = tokio::time::timeout(Duration::from_millis(200), server_task)
            .await
            .expect("server retained a disconnected request")
            .unwrap()
            .unwrap_err();
        assert!(matches!(error, SnapshotTransportError::ClientDisconnected));
    }
}
