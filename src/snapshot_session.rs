//! Authenticated, bounded wire primitives for snapshot companion sessions.
//!
//! This is an authenticated server-response frame foundation, not a complete
//! peer-authenticated session or handshake. It deliberately contains no socket,
//! transport-credential, key-negotiation, or task-lifecycle code. The caller
//! supplies a fresh challenge and the exact expected engine/snapshot binding,
//! while these primitives provide strict framing and response authentication.
//!
//! # Security status
//!
//! This slice is not deployable as a complete session protocol. The hello is
//! unauthenticated, response decoding uses bounded owned `Vec` fields, and the
//! exact watermark/generation expectations arrive out of band. A transport
//! must add peer-authenticated fixed framing, trusted expectation derivation,
//! freshness tracking, and connection lifecycle fencing before deployment.

use std::fmt;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::kv_snapshot::EngineIncarnation;

pub const SNAPSHOT_SESSION_SCHEMA_VERSION: u16 = 1;
pub const SNAPSHOT_SESSION_CHALLENGE_BYTES: usize = 32;
pub const SNAPSHOT_SESSION_SECRET_BYTES: usize = 32;
pub const SNAPSHOT_DIGEST_KEY_ID_BYTES: usize = 32;
const SHA256_BYTES: usize = 32;
const SHA256_BLOCK_BYTES: usize = 64;
/// Direction is part of the domain so a valid server response cannot be used
/// as an authenticated client message in a future bidirectional protocol.
const SERVER_RESPONSE_AUTH_DOMAIN: &[u8] =
    b"mini-dynamo/snapshot-session/server-response/auth/v1\0";

#[derive(Clone, Copy, Debug)]
pub struct SnapshotSessionLimits {
    pub max_hello_frame_bytes: usize,
    pub max_response_frame_bytes: usize,
    pub max_header_bytes: usize,
    pub max_snapshot_frame_bytes: usize,
    pub max_incarnation_component_bytes: usize,
    pub max_key_id_bytes: usize,
}

impl Default for SnapshotSessionLimits {
    fn default() -> Self {
        Self {
            max_hello_frame_bytes: 512,
            max_response_frame_bytes: 64 * 1024 * 1024 + 4 * 1024,
            max_header_bytes: 4 * 1024,
            max_snapshot_frame_bytes: 64 * 1024 * 1024,
            max_incarnation_component_bytes: 512,
            max_key_id_bytes: SNAPSHOT_DIGEST_KEY_ID_BYTES,
        }
    }
}

/// Caller-generated freshness challenge. Randomness is intentionally outside
/// this module so a deployment can use its audited entropy source.
#[derive(Clone, Copy, Eq, PartialEq)]
pub struct SnapshotSessionChallenge([u8; SNAPSHOT_SESSION_CHALLENGE_BYTES]);

impl SnapshotSessionChallenge {
    #[must_use]
    pub const fn new(bytes: [u8; SNAPSHOT_SESSION_CHALLENGE_BYTES]) -> Self {
        Self(bytes)
    }

    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; SNAPSHOT_SESSION_CHALLENGE_BYTES] {
        &self.0
    }
}

impl fmt::Debug for SnapshotSessionChallenge {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SnapshotSessionChallenge([REDACTED])")
    }
}

/// Exact 256-bit session-authenticator secret.
///
/// This is a distinct type and protocol domain from the block-digest key. A
/// deployment must derive/provision it independently and never reuse the block
/// digest secret. It has no serialization implementation and its debug
/// representation is always redacted.
pub struct SnapshotSessionSecret([u8; SNAPSHOT_SESSION_SECRET_BYTES]);

impl SnapshotSessionSecret {
    #[must_use]
    pub const fn new(bytes: [u8; SNAPSHOT_SESSION_SECRET_BYTES]) -> Self {
        Self(bytes)
    }
}

impl fmt::Debug for SnapshotSessionSecret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SnapshotSessionSecret([REDACTED])")
    }
}

impl Drop for SnapshotSessionSecret {
    fn drop(&mut self) {
        self.0.fill(0);
    }
}

#[derive(Clone, Copy)]
pub struct SnapshotSessionBinding<'a> {
    pub challenge: SnapshotSessionChallenge,
    pub engine_incarnation: &'a EngineIncarnation,
    pub snapshot_watermark: u64,
    pub digest_key_id: &'a [u8],
    pub companion_generation: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuthenticatedSnapshot {
    pub engine_incarnation: EngineIncarnation,
    pub snapshot_watermark: u64,
    pub digest_key_id: Vec<u8>,
    pub companion_generation: u64,
    pub snapshot_frame: Vec<u8>,
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum SnapshotSessionError {
    #[error("snapshot session hello exceeds configured byte limit")]
    HelloFrameTooLarge,
    #[error("snapshot session response exceeds configured byte limit")]
    ResponseFrameTooLarge,
    #[error("snapshot session header exceeds configured byte limit")]
    HeaderTooLarge,
    #[error("snapshot session payload exceeds configured byte limit")]
    SnapshotFrameTooLarge,
    #[error("invalid snapshot session MessagePack")]
    InvalidMessagePack,
    #[error("unsupported snapshot session schema")]
    UnsupportedSchema,
    #[error("invalid snapshot session challenge")]
    InvalidChallenge,
    #[error("invalid snapshot session engine incarnation")]
    InvalidIncarnation,
    #[error("invalid snapshot session digest key identifier")]
    InvalidKeyId,
    #[error("invalid snapshot companion generation")]
    InvalidGeneration,
    #[error("invalid snapshot session payload length")]
    InvalidPayloadLength,
    #[error("invalid snapshot session payload checksum")]
    InvalidChecksum,
    #[error("invalid snapshot session authenticator")]
    InvalidAuthenticator,
    #[error("snapshot session authentication failed")]
    AuthenticationFailed,
    #[error("snapshot session challenge does not match")]
    ChallengeMismatch,
    #[error("snapshot session engine incarnation does not match")]
    IncarnationMismatch,
    #[error("snapshot session watermark does not match")]
    WatermarkMismatch,
    #[error("snapshot session digest key identifier does not match")]
    KeyIdMismatch,
    #[error("snapshot companion generation does not match")]
    GenerationMismatch,
    #[error("snapshot session encoding failed")]
    EncodeFailed,
}

impl SnapshotSessionError {
    #[must_use]
    pub const fn reason(&self) -> &'static str {
        match self {
            Self::HelloFrameTooLarge => "hello_frame_too_large",
            Self::ResponseFrameTooLarge => "response_frame_too_large",
            Self::HeaderTooLarge => "header_too_large",
            Self::SnapshotFrameTooLarge => "snapshot_frame_too_large",
            Self::InvalidMessagePack => "invalid_messagepack",
            Self::UnsupportedSchema => "unsupported_schema",
            Self::InvalidChallenge => "invalid_challenge",
            Self::InvalidIncarnation => "invalid_incarnation",
            Self::InvalidKeyId => "invalid_key_id",
            Self::InvalidGeneration => "invalid_generation",
            Self::InvalidPayloadLength => "invalid_payload_length",
            Self::InvalidChecksum => "invalid_checksum",
            Self::InvalidAuthenticator => "invalid_authenticator",
            Self::AuthenticationFailed => "authentication_failed",
            Self::ChallengeMismatch => "challenge_mismatch",
            Self::IncarnationMismatch => "incarnation_mismatch",
            Self::WatermarkMismatch => "watermark_mismatch",
            Self::KeyIdMismatch => "key_id_mismatch",
            Self::GenerationMismatch => "generation_mismatch",
            Self::EncodeFailed => "encode_failed",
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ClientHello {
    schema_version: u16,
    #[serde(with = "serde_bytes")]
    challenge: Vec<u8>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ResponseHeader {
    schema_version: u16,
    #[serde(with = "serde_bytes")]
    challenge: Vec<u8>,
    engine_incarnation: EngineIncarnation,
    snapshot_watermark: u64,
    #[serde(with = "serde_bytes")]
    digest_key_id: Vec<u8>,
    companion_generation: u64,
    snapshot_frame_bytes: u64,
    #[serde(with = "serde_bytes")]
    snapshot_frame_sha256: Vec<u8>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ResponseEnvelope {
    /// Opaque named-MessagePack [`ResponseHeader`]. Keeping its original bytes
    /// makes the MAC bind the exact wire representation, not a re-encoding.
    #[serde(with = "serde_bytes")]
    header: Vec<u8>,
    #[serde(with = "serde_bytes")]
    snapshot_frame: Vec<u8>,
    #[serde(with = "serde_bytes")]
    authenticator: Vec<u8>,
}

/// Encode the unauthenticated freshness challenge for a future session.
///
/// This hello alone does not authenticate either peer and must not be treated
/// as a complete handshake.
///
/// # Errors
///
/// Returns `SnapshotSessionError` if the named `MessagePack` frame cannot be
/// encoded within the configured limit.
pub fn encode_client_hello(
    challenge: SnapshotSessionChallenge,
    limits: SnapshotSessionLimits,
) -> Result<Vec<u8>, SnapshotSessionError> {
    let hello = ClientHello {
        schema_version: SNAPSHOT_SESSION_SCHEMA_VERSION,
        challenge: challenge.0.to_vec(),
    };
    let frame = rmp_serde::to_vec_named(&hello).map_err(|_| SnapshotSessionError::EncodeFailed)?;
    if frame.len() > limits.max_hello_frame_bytes {
        return Err(SnapshotSessionError::HelloFrameTooLarge);
    }
    Ok(frame)
}

/// Decode a bounded client hello. The returned challenge must be copied into
/// the authenticated response binding.
///
/// # Errors
///
/// Returns `SnapshotSessionError` for malformed, unsupported, or oversized
/// input.
pub fn decode_client_hello(
    frame: &[u8],
    limits: SnapshotSessionLimits,
) -> Result<SnapshotSessionChallenge, SnapshotSessionError> {
    if frame.len() > limits.max_hello_frame_bytes {
        return Err(SnapshotSessionError::HelloFrameTooLarge);
    }
    let hello: ClientHello =
        rmp_serde::from_slice(frame).map_err(|_| SnapshotSessionError::InvalidMessagePack)?;
    if hello.schema_version != SNAPSHOT_SESSION_SCHEMA_VERSION {
        return Err(SnapshotSessionError::UnsupportedSchema);
    }
    let challenge = hello
        .challenge
        .try_into()
        .map_err(|_| SnapshotSessionError::InvalidChallenge)?;
    Ok(SnapshotSessionChallenge(challenge))
}

/// Encode a snapshot response authenticated over the exact header and exact
/// snapshot frame bytes.
///
/// # Errors
///
/// Returns `SnapshotSessionError` if a binding is invalid, a configured
/// bound is exceeded, or `MessagePack` encoding fails.
pub fn encode_authenticated_snapshot(
    snapshot_frame: &[u8],
    binding: SnapshotSessionBinding<'_>,
    secret: &SnapshotSessionSecret,
    limits: SnapshotSessionLimits,
) -> Result<Vec<u8>, SnapshotSessionError> {
    validate_binding(binding, limits)?;
    if snapshot_frame.len() > limits.max_snapshot_frame_bytes {
        return Err(SnapshotSessionError::SnapshotFrameTooLarge);
    }
    let header = ResponseHeader {
        schema_version: SNAPSHOT_SESSION_SCHEMA_VERSION,
        challenge: binding.challenge.0.to_vec(),
        engine_incarnation: binding.engine_incarnation.clone(),
        snapshot_watermark: binding.snapshot_watermark,
        digest_key_id: binding.digest_key_id.to_vec(),
        companion_generation: binding.companion_generation,
        snapshot_frame_bytes: u64::try_from(snapshot_frame.len())
            .map_err(|_| SnapshotSessionError::SnapshotFrameTooLarge)?,
        snapshot_frame_sha256: Sha256::digest(snapshot_frame).to_vec(),
    };
    let header =
        rmp_serde::to_vec_named(&header).map_err(|_| SnapshotSessionError::EncodeFailed)?;
    if header.len() > limits.max_header_bytes {
        return Err(SnapshotSessionError::HeaderTooLarge);
    }
    let authenticator = authenticate(secret, &header, snapshot_frame).to_vec();
    let envelope = ResponseEnvelope {
        header,
        snapshot_frame: snapshot_frame.to_vec(),
        authenticator,
    };
    let frame =
        rmp_serde::to_vec_named(&envelope).map_err(|_| SnapshotSessionError::EncodeFailed)?;
    if frame.len() > limits.max_response_frame_bytes {
        return Err(SnapshotSessionError::ResponseFrameTooLarge);
    }
    Ok(frame)
}

/// Authenticate, decode, bound, and exactly match a snapshot response.
/// Authentication happens before header parsing or expectation checks.
///
/// # Errors
///
/// Returns `SnapshotSessionError` on malformed input, failed
/// authentication, a configured capacity breach, or a binding mismatch.
pub fn decode_authenticated_snapshot(
    frame: &[u8],
    expected: SnapshotSessionBinding<'_>,
    secret: &SnapshotSessionSecret,
    limits: SnapshotSessionLimits,
) -> Result<AuthenticatedSnapshot, SnapshotSessionError> {
    validate_binding(expected, limits)?;
    if frame.len() > limits.max_response_frame_bytes {
        return Err(SnapshotSessionError::ResponseFrameTooLarge);
    }
    let envelope: ResponseEnvelope =
        rmp_serde::from_slice(frame).map_err(|_| SnapshotSessionError::InvalidMessagePack)?;
    if envelope.header.len() > limits.max_header_bytes {
        return Err(SnapshotSessionError::HeaderTooLarge);
    }
    if envelope.snapshot_frame.len() > limits.max_snapshot_frame_bytes {
        return Err(SnapshotSessionError::SnapshotFrameTooLarge);
    }
    let authenticator_has_valid_length = envelope.authenticator.len() == SHA256_BYTES;
    let computed = authenticate(secret, &envelope.header, &envelope.snapshot_frame);
    if !constant_work_mac_eq(&envelope.authenticator, &computed) {
        return Err(if authenticator_has_valid_length {
            SnapshotSessionError::AuthenticationFailed
        } else {
            SnapshotSessionError::InvalidAuthenticator
        });
    }

    let header: ResponseHeader = rmp_serde::from_slice(&envelope.header)
        .map_err(|_| SnapshotSessionError::InvalidMessagePack)?;
    validate_response_header(&header, limits)?;
    if u64::try_from(envelope.snapshot_frame.len()).ok() != Some(header.snapshot_frame_bytes) {
        return Err(SnapshotSessionError::InvalidPayloadLength);
    }
    let checksum: [u8; SHA256_BYTES] = Sha256::digest(&envelope.snapshot_frame).into();
    if !constant_work_mac_eq(&header.snapshot_frame_sha256, &checksum) {
        return Err(SnapshotSessionError::InvalidChecksum);
    }

    if header.challenge.as_slice() != expected.challenge.as_bytes() {
        return Err(SnapshotSessionError::ChallengeMismatch);
    }
    if header.engine_incarnation != *expected.engine_incarnation {
        return Err(SnapshotSessionError::IncarnationMismatch);
    }
    if header.snapshot_watermark != expected.snapshot_watermark {
        return Err(SnapshotSessionError::WatermarkMismatch);
    }
    if header.digest_key_id != expected.digest_key_id {
        return Err(SnapshotSessionError::KeyIdMismatch);
    }
    if header.companion_generation != expected.companion_generation {
        return Err(SnapshotSessionError::GenerationMismatch);
    }

    Ok(AuthenticatedSnapshot {
        engine_incarnation: header.engine_incarnation,
        snapshot_watermark: header.snapshot_watermark,
        digest_key_id: header.digest_key_id,
        companion_generation: header.companion_generation,
        snapshot_frame: envelope.snapshot_frame,
    })
}

fn validate_binding(
    binding: SnapshotSessionBinding<'_>,
    limits: SnapshotSessionLimits,
) -> Result<(), SnapshotSessionError> {
    validate_incarnation(binding.engine_incarnation, limits)?;
    if binding.digest_key_id.len() != SNAPSHOT_DIGEST_KEY_ID_BYTES
        || binding.digest_key_id.len() > limits.max_key_id_bytes
    {
        return Err(SnapshotSessionError::InvalidKeyId);
    }
    if binding.companion_generation == 0 {
        return Err(SnapshotSessionError::InvalidGeneration);
    }
    Ok(())
}

fn validate_response_header(
    header: &ResponseHeader,
    limits: SnapshotSessionLimits,
) -> Result<(), SnapshotSessionError> {
    if header.schema_version != SNAPSHOT_SESSION_SCHEMA_VERSION {
        return Err(SnapshotSessionError::UnsupportedSchema);
    }
    if header.challenge.len() != SNAPSHOT_SESSION_CHALLENGE_BYTES {
        return Err(SnapshotSessionError::InvalidChallenge);
    }
    validate_incarnation(&header.engine_incarnation, limits)?;
    if header.digest_key_id.len() != SNAPSHOT_DIGEST_KEY_ID_BYTES
        || header.digest_key_id.len() > limits.max_key_id_bytes
    {
        return Err(SnapshotSessionError::InvalidKeyId);
    }
    if header.companion_generation == 0 {
        return Err(SnapshotSessionError::InvalidGeneration);
    }
    if header.snapshot_frame_sha256.len() != SHA256_BYTES {
        return Err(SnapshotSessionError::InvalidChecksum);
    }
    if header.snapshot_frame_bytes
        > u64::try_from(limits.max_snapshot_frame_bytes).unwrap_or(u64::MAX)
    {
        return Err(SnapshotSessionError::SnapshotFrameTooLarge);
    }
    Ok(())
}

fn validate_incarnation(
    incarnation: &EngineIncarnation,
    limits: SnapshotSessionLimits,
) -> Result<(), SnapshotSessionError> {
    let valid_component =
        |value: &str| !value.is_empty() && value.len() <= limits.max_incarnation_component_bytes;
    if !valid_component(&incarnation.engine_id)
        || !valid_component(&incarnation.model_revision)
        || !valid_component(&incarnation.image_digest)
        || incarnation.process_started_unix_ns == 0
        || incarnation.attestation_sha256.len() != SHA256_BYTES
    {
        return Err(SnapshotSessionError::InvalidIncarnation);
    }
    Ok(())
}

fn authenticate(
    secret: &SnapshotSessionSecret,
    header: &[u8],
    snapshot_frame: &[u8],
) -> [u8; SHA256_BYTES] {
    let mut hmac = HmacSha256::new(&secret.0);
    // The exact named header binds schema version, challenge/request,
    // incarnation, watermark/sequence, digest key identity, companion
    // generation/session, checksum, and the repeated payload length.
    hmac.update(SERVER_RESPONSE_AUTH_DOMAIN);
    hmac.update(
        &u64::try_from(header.len())
            .unwrap_or(u64::MAX)
            .to_be_bytes(),
    );
    hmac.update(header);
    hmac.update(
        &u64::try_from(snapshot_frame.len())
            .unwrap_or(u64::MAX)
            .to_be_bytes(),
    );
    hmac.update(snapshot_frame);
    hmac.finalize()
}

fn constant_work_mac_eq(candidate: &[u8], expected: &[u8; SHA256_BYTES]) -> bool {
    let mut difference = candidate.len() ^ SHA256_BYTES;
    for (index, expected_byte) in expected.iter().enumerate() {
        difference |= usize::from(candidate.get(index).copied().unwrap_or(0) ^ expected_byte);
    }
    difference == 0
}

struct HmacSha256 {
    inner: Sha256,
    outer_pad: [u8; SHA256_BLOCK_BYTES],
}

impl HmacSha256 {
    fn new(secret: &[u8; SNAPSHOT_SESSION_SECRET_BYTES]) -> Self {
        let mut inner_pad = [0x36_u8; SHA256_BLOCK_BYTES];
        let mut outer_pad = [0x5c_u8; SHA256_BLOCK_BYTES];
        for ((inner, outer), secret_byte) in inner_pad
            .iter_mut()
            .zip(&mut outer_pad)
            .zip(secret.iter().copied().chain(std::iter::repeat(0)))
        {
            *inner ^= secret_byte;
            *outer ^= secret_byte;
        }
        let mut inner = Sha256::new();
        inner.update(inner_pad);
        inner_pad.fill(0);
        Self { inner, outer_pad }
    }

    fn update(&mut self, bytes: &[u8]) {
        self.inner.update(bytes);
    }

    fn finalize(mut self) -> [u8; SHA256_BYTES] {
        let inner_digest = self.inner.finalize();
        let mut outer = Sha256::new();
        outer.update(self.outer_pad);
        outer.update(inner_digest);
        self.outer_pad.fill(0);
        outer.finalize().into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CHALLENGE: SnapshotSessionChallenge = SnapshotSessionChallenge::new([0x31; 32]);
    const SECRET_BYTES: [u8; 32] = *b"snapshot-session-secret-32-byte!";
    const KEY_ID: [u8; 32] = [0x6b; 32];

    fn incarnation() -> EngineIncarnation {
        EngineIncarnation {
            engine_id: "engine-a".to_owned(),
            model_revision: "revision-a".to_owned(),
            image_digest: "sha256:image-a".to_owned(),
            process_started_unix_ns: 42,
            attestation_sha256: vec![0xa5; 32],
        }
    }

    fn binding(incarnation: &EngineIncarnation) -> SnapshotSessionBinding<'_> {
        SnapshotSessionBinding {
            challenge: CHALLENGE,
            engine_incarnation: incarnation,
            snapshot_watermark: 9_123,
            digest_key_id: &KEY_ID,
            companion_generation: 7,
        }
    }

    fn encode_fixture() -> (Vec<u8>, EngineIncarnation) {
        let incarnation = incarnation();
        let secret = SnapshotSessionSecret::new(SECRET_BYTES);
        let frame = encode_authenticated_snapshot(
            b"opaque-kv-snapshot-frame",
            binding(&incarnation),
            &secret,
            SnapshotSessionLimits::default(),
        )
        .unwrap();
        (frame, incarnation)
    }

    #[test]
    fn hello_and_authenticated_snapshot_round_trip() {
        let limits = SnapshotSessionLimits::default();
        let hello = encode_client_hello(CHALLENGE, limits).unwrap();
        assert_eq!(decode_client_hello(&hello, limits).unwrap(), CHALLENGE);

        let (frame, incarnation) = encode_fixture();
        let secret = SnapshotSessionSecret::new(SECRET_BYTES);
        let decoded =
            decode_authenticated_snapshot(&frame, binding(&incarnation), &secret, limits).unwrap();
        assert_eq!(decoded.engine_incarnation, incarnation);
        assert_eq!(decoded.snapshot_watermark, 9_123);
        assert_eq!(decoded.digest_key_id, KEY_ID);
        assert_eq!(decoded.companion_generation, 7);
        assert_eq!(decoded.snapshot_frame, b"opaque-kv-snapshot-frame");
    }

    #[test]
    fn tampering_and_reparenting_fail_authentication() {
        let (frame, incarnation) = encode_fixture();
        let mut envelope: ResponseEnvelope = rmp_serde::from_slice(&frame).unwrap();
        let original_header = envelope.header.clone();
        envelope.header[5] ^= 1;
        let tampered_header = rmp_serde::to_vec_named(&envelope).unwrap();
        let secret = SnapshotSessionSecret::new(SECRET_BYTES);
        assert_eq!(
            decode_authenticated_snapshot(
                &tampered_header,
                binding(&incarnation),
                &secret,
                SnapshotSessionLimits::default(),
            ),
            Err(SnapshotSessionError::AuthenticationFailed)
        );

        envelope.header = original_header;
        envelope.snapshot_frame[3] ^= 1;
        let tampered = rmp_serde::to_vec_named(&envelope).unwrap();
        assert_eq!(
            decode_authenticated_snapshot(
                &tampered,
                binding(&incarnation),
                &secret,
                SnapshotSessionLimits::default(),
            ),
            Err(SnapshotSessionError::AuthenticationFailed)
        );

        let other_secret = SnapshotSessionSecret::new(SECRET_BYTES);
        let other = encode_authenticated_snapshot(
            b"another-snapshot",
            binding(&incarnation),
            &other_secret,
            SnapshotSessionLimits::default(),
        )
        .unwrap();
        let other: ResponseEnvelope = rmp_serde::from_slice(&other).unwrap();
        envelope.snapshot_frame = other.snapshot_frame;
        let reparented = rmp_serde::to_vec_named(&envelope).unwrap();
        assert_eq!(
            decode_authenticated_snapshot(
                &reparented,
                binding(&incarnation),
                &secret,
                SnapshotSessionLimits::default(),
            ),
            Err(SnapshotSessionError::AuthenticationFailed)
        );
    }

    #[test]
    fn wrong_secret_fails_authentication() {
        let (frame, incarnation) = encode_fixture();
        let wrong = SnapshotSessionSecret::new([0x44; 32]);
        assert_eq!(
            decode_authenticated_snapshot(
                &frame,
                binding(&incarnation),
                &wrong,
                SnapshotSessionLimits::default(),
            ),
            Err(SnapshotSessionError::AuthenticationFailed)
        );
    }

    #[test]
    fn exact_expectations_reject_replay_and_mixups() {
        let (frame, incarnation) = encode_fixture();
        let secret = SnapshotSessionSecret::new(SECRET_BYTES);
        let limits = SnapshotSessionLimits::default();

        let mut expected = binding(&incarnation);
        expected.challenge = SnapshotSessionChallenge::new([0x32; 32]);
        assert_eq!(
            decode_authenticated_snapshot(&frame, expected, &secret, limits),
            Err(SnapshotSessionError::ChallengeMismatch)
        );

        let wrong_incarnation = EngineIncarnation {
            engine_id: "engine-b".to_owned(),
            ..incarnation.clone()
        };
        assert_eq!(
            decode_authenticated_snapshot(
                &frame,
                SnapshotSessionBinding {
                    engine_incarnation: &wrong_incarnation,
                    ..binding(&incarnation)
                },
                &secret,
                limits,
            ),
            Err(SnapshotSessionError::IncarnationMismatch)
        );

        assert_eq!(
            decode_authenticated_snapshot(
                &frame,
                SnapshotSessionBinding {
                    snapshot_watermark: 9_124,
                    ..binding(&incarnation)
                },
                &secret,
                limits,
            ),
            Err(SnapshotSessionError::WatermarkMismatch)
        );
        assert_eq!(
            decode_authenticated_snapshot(
                &frame,
                SnapshotSessionBinding {
                    digest_key_id: &[0x7c; 32],
                    ..binding(&incarnation)
                },
                &secret,
                limits,
            ),
            Err(SnapshotSessionError::KeyIdMismatch)
        );
        assert_eq!(
            decode_authenticated_snapshot(
                &frame,
                SnapshotSessionBinding {
                    companion_generation: 8,
                    ..binding(&incarnation)
                },
                &secret,
                limits,
            ),
            Err(SnapshotSessionError::GenerationMismatch)
        );
    }

    #[test]
    fn wire_and_semantic_limits_are_strict() {
        let limits = SnapshotSessionLimits {
            max_hello_frame_bytes: 1,
            ..SnapshotSessionLimits::default()
        };
        assert_eq!(
            encode_client_hello(CHALLENGE, limits),
            Err(SnapshotSessionError::HelloFrameTooLarge)
        );

        let incarnation = incarnation();
        let secret = SnapshotSessionSecret::new(SECRET_BYTES);
        let limits = SnapshotSessionLimits {
            max_snapshot_frame_bytes: 3,
            ..SnapshotSessionLimits::default()
        };
        assert_eq!(
            encode_authenticated_snapshot(b"four", binding(&incarnation), &secret, limits),
            Err(SnapshotSessionError::SnapshotFrameTooLarge)
        );

        let limits = SnapshotSessionLimits {
            max_key_id_bytes: KEY_ID.len() - 1,
            ..SnapshotSessionLimits::default()
        };
        assert_eq!(
            encode_authenticated_snapshot(b"ok", binding(&incarnation), &secret, limits),
            Err(SnapshotSessionError::InvalidKeyId)
        );

        let oversized = EngineIncarnation {
            engine_id: "too-long".to_owned(),
            ..incarnation
        };
        let limits = SnapshotSessionLimits {
            max_incarnation_component_bytes: 3,
            ..SnapshotSessionLimits::default()
        };
        assert_eq!(
            encode_authenticated_snapshot(b"ok", binding(&oversized), &secret, limits),
            Err(SnapshotSessionError::InvalidIncarnation)
        );
    }

    #[test]
    fn secrets_and_errors_are_content_free() {
        let secret = SnapshotSessionSecret::new(SECRET_BYTES);
        assert_eq!(format!("{secret:?}"), "SnapshotSessionSecret([REDACTED])");
        assert_eq!(
            format!("{CHALLENGE:?}"),
            "SnapshotSessionChallenge([REDACTED])"
        );
        let (frame, _) = encode_fixture();
        assert!(
            !frame
                .windows(SECRET_BYTES.len())
                .any(|part| part == SECRET_BYTES)
        );

        for error in [
            SnapshotSessionError::AuthenticationFailed,
            SnapshotSessionError::IncarnationMismatch,
            SnapshotSessionError::WatermarkMismatch,
            SnapshotSessionError::KeyIdMismatch,
        ] {
            let display = error.to_string();
            assert!(!display.contains("engine-a"));
            assert!(!display.contains("9123"));
            assert!(!display.contains("sha256:image-a"));
            assert!(!error.reason().contains("engine"));
        }
    }
}
