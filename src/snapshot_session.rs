//! Bounded authenticated one-shot exchange for a KV snapshot companion.
//!
//! Both messages use fixed binary outer frames. A client hello proves knowledge
//! of the session-auth key before a companion starts snapshot work. A server
//! response authenticates the exact fixed prefix, named-MessagePack metadata,
//! and opaque snapshot bytes. Decoding validates all declared lengths and the
//! MAC over borrowed slices before `MessagePack` can allocate or payload bytes
//! are copied.
//!
//! This is deployable only as the one-shot snapshot exchange inside a larger
//! transport. Production must additionally enforce Unix-socket permissions and
//! `SO_PEERCRED`, provision the session-auth secret independently from the block
//! digest key, remember challenges to prevent reuse, authenticate all later
//! tail/control frames, and fence on disconnect or lifecycle mismatch.

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
const MAGIC: &[u8; 8] = b"MDSNAP01";
const CLIENT_HELLO_TYPE: u8 = 1;
const SNAPSHOT_RESPONSE_TYPE: u8 = 2;
const CLIENT_TO_SERVER: u8 = 1;
const SERVER_TO_CLIENT: u8 = 2;
const HELLO_PREFIX_BYTES: usize = 8 + 2 + 1 + 1 + 4 + SNAPSHOT_SESSION_CHALLENGE_BYTES;
const HELLO_FRAME_BYTES: usize = HELLO_PREFIX_BYTES + SHA256_BYTES;
const RESPONSE_PREFIX_BYTES: usize = 8 + 2 + 1 + 1 + 8 + 4 + 8 + SNAPSHOT_SESSION_CHALLENGE_BYTES;
const RESPONSE_OVERHEAD_BYTES: usize = RESPONSE_PREFIX_BYTES + SHA256_BYTES;
/// Bytes required to validate the fixed response identity and declared total
/// before allocating the rest of a stream frame.
pub const SNAPSHOT_RESPONSE_LENGTH_PREFIX_BYTES: usize = 20;
const HELLO_AUTH_DOMAIN: &[u8] = b"mini-dynamo/snapshot-session/client-hello/auth/v1\0";
const RESPONSE_AUTH_DOMAIN: &[u8] = b"mini-dynamo/snapshot-session/server-response/auth/v1\0";

#[derive(Clone, Copy, Debug)]
pub struct SnapshotSessionLimits {
    pub max_hello_frame_bytes: usize,
    pub max_response_frame_bytes: usize,
    pub max_header_bytes: usize,
    pub max_snapshot_frame_bytes: usize,
    pub max_incarnation_component_bytes: usize,
}

impl Default for SnapshotSessionLimits {
    fn default() -> Self {
        Self {
            max_hello_frame_bytes: HELLO_FRAME_BYTES,
            max_response_frame_bytes: 32 * 1024 * 1024 + 4 * 1024,
            max_header_bytes: 4 * 1024,
            max_snapshot_frame_bytes: 32 * 1024 * 1024,
            max_incarnation_component_bytes: 512,
        }
    }
}

/// Caller-generated freshness challenge. Randomness and reuse tracking are
/// intentionally outside this module.
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

/// Exact 256-bit session-auth key, distinct from the block-digest key.
///
/// It intentionally implements neither serialization nor `Clone`; its debug
/// representation is redacted and its directly owned bytes are cleared on
/// drop as defense in depth.
pub struct SnapshotSessionSecret([u8; SNAPSHOT_SESSION_SECRET_BYTES]);

impl SnapshotSessionSecret {
    #[must_use]
    pub const fn new(bytes: [u8; SNAPSHOT_SESSION_SECRET_BYTES]) -> Self {
        Self(bytes)
    }

    /// Derive domain-separated ephemeral material without exposing the
    /// long-lived session-auth secret. Protocol modules use this for
    /// direction-specific keys bound to an authenticated transcript.
    pub(crate) fn derive_subkey(&self, domain: &[u8], parts: &[&[u8]]) -> [u8; 32] {
        authenticate(self, domain, parts)
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

/// Metadata asserted by the snapshot producer and covered by the response MAC.
#[derive(Clone, Copy)]
pub struct SnapshotSessionBinding<'a> {
    pub challenge: SnapshotSessionChallenge,
    pub engine_incarnation: &'a EngineIncarnation,
    pub snapshot_watermark: u64,
    pub digest_key_id: &'a [u8; SNAPSHOT_DIGEST_KEY_ID_BYTES],
    pub companion_generation: u64,
}

/// Independently established client expectations.
///
/// Incarnation and digest key identity must match exactly. Watermark and
/// generation are monotonic freshness floors because their current exact
/// values are learned from the authenticated producer response.
#[derive(Clone, Copy)]
pub struct SnapshotSessionExpectations<'a> {
    pub challenge: SnapshotSessionChallenge,
    pub engine_incarnation: &'a EngineIncarnation,
    pub digest_key_id: &'a [u8; SNAPSHOT_DIGEST_KEY_ID_BYTES],
    pub minimum_snapshot_watermark: u64,
    pub minimum_companion_generation: u64,
}

#[derive(Debug, Eq, PartialEq)]
pub struct AuthenticatedSnapshot {
    engine_incarnation: EngineIncarnation,
    snapshot_watermark: u64,
    digest_key_id: [u8; SNAPSHOT_DIGEST_KEY_ID_BYTES],
    companion_generation: u64,
    snapshot_frame: Vec<u8>,
}

impl AuthenticatedSnapshot {
    #[must_use]
    pub const fn engine_incarnation(&self) -> &EngineIncarnation {
        &self.engine_incarnation
    }

    #[must_use]
    pub const fn snapshot_watermark(&self) -> u64 {
        self.snapshot_watermark
    }

    #[must_use]
    pub const fn digest_key_id(&self) -> &[u8; SNAPSHOT_DIGEST_KEY_ID_BYTES] {
        &self.digest_key_id
    }

    #[must_use]
    pub const fn companion_generation(&self) -> u64 {
        self.companion_generation
    }

    #[must_use]
    pub fn snapshot_frame(&self) -> &[u8] {
        &self.snapshot_frame
    }

    #[must_use]
    pub fn into_snapshot_frame(self) -> Vec<u8> {
        self.snapshot_frame
    }
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum SnapshotSessionError {
    #[error("snapshot session hello exceeds configured byte limit")]
    HelloFrameTooLarge,
    #[error("snapshot session response exceeds configured byte limit")]
    ResponseFrameTooLarge,
    #[error("snapshot session header exceeds configured byte limit")]
    HeaderTooLarge,
    #[error("snapshot session payload exceeds configured byte limit")]
    SnapshotFrameTooLarge,
    #[error("invalid snapshot session frame length")]
    InvalidFrameLength,
    #[error("invalid snapshot session MessagePack")]
    InvalidMessagePack,
    #[error("unsupported snapshot session schema")]
    UnsupportedSchema,
    #[error("invalid snapshot session message type")]
    InvalidMessageType,
    #[error("invalid snapshot session direction")]
    InvalidDirection,
    #[error("invalid snapshot session magic")]
    InvalidMagic,
    #[error("invalid snapshot session engine incarnation")]
    InvalidIncarnation,
    #[error("invalid snapshot companion generation")]
    InvalidGeneration,
    #[error("invalid snapshot session payload checksum")]
    InvalidChecksum,
    #[error("snapshot session authentication failed")]
    AuthenticationFailed,
    #[error("snapshot session challenge does not match")]
    ChallengeMismatch,
    #[error("snapshot session engine incarnation does not match")]
    IncarnationMismatch,
    #[error("snapshot session watermark is stale")]
    StaleWatermark,
    #[error("snapshot session digest key identifier does not match")]
    KeyIdMismatch,
    #[error("snapshot companion generation is stale")]
    StaleGeneration,
    #[error("snapshot session encoding failed")]
    EncodeFailed,
}

impl SnapshotSessionError {
    #[must_use]
    pub const fn reason(self) -> &'static str {
        match self {
            Self::HelloFrameTooLarge => "hello_frame_too_large",
            Self::ResponseFrameTooLarge => "response_frame_too_large",
            Self::HeaderTooLarge => "header_too_large",
            Self::SnapshotFrameTooLarge => "snapshot_frame_too_large",
            Self::InvalidFrameLength => "invalid_frame_length",
            Self::InvalidMessagePack => "invalid_messagepack",
            Self::UnsupportedSchema => "unsupported_schema",
            Self::InvalidMessageType => "invalid_message_type",
            Self::InvalidDirection => "invalid_direction",
            Self::InvalidMagic => "invalid_magic",
            Self::InvalidIncarnation => "invalid_incarnation",
            Self::InvalidGeneration => "invalid_generation",
            Self::InvalidChecksum => "invalid_checksum",
            Self::AuthenticationFailed => "authentication_failed",
            Self::ChallengeMismatch => "challenge_mismatch",
            Self::IncarnationMismatch => "incarnation_mismatch",
            Self::StaleWatermark => "stale_watermark",
            Self::KeyIdMismatch => "key_id_mismatch",
            Self::StaleGeneration => "stale_generation",
            Self::EncodeFailed => "encode_failed",
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ResponseMetadata {
    engine_incarnation: EngineIncarnation,
    snapshot_watermark: u64,
    #[serde(with = "serde_bytes")]
    digest_key_id: Vec<u8>,
    companion_generation: u64,
    #[serde(with = "serde_bytes")]
    snapshot_frame_sha256: Vec<u8>,
}

/// Encode an authenticated fixed-width client hello.
///
/// # Errors
///
/// Returns `SnapshotSessionError` when the configured frame bound is smaller
/// than the protocol's fixed hello size.
pub fn encode_client_hello(
    challenge: SnapshotSessionChallenge,
    secret: &SnapshotSessionSecret,
    limits: SnapshotSessionLimits,
) -> Result<Vec<u8>, SnapshotSessionError> {
    if HELLO_FRAME_BYTES > limits.max_hello_frame_bytes {
        return Err(SnapshotSessionError::HelloFrameTooLarge);
    }
    let mut frame = Vec::with_capacity(HELLO_FRAME_BYTES);
    frame.extend_from_slice(MAGIC);
    frame.extend_from_slice(&SNAPSHOT_SESSION_SCHEMA_VERSION.to_be_bytes());
    frame.push(CLIENT_HELLO_TYPE);
    frame.push(CLIENT_TO_SERVER);
    frame.extend_from_slice(
        &u32::try_from(HELLO_FRAME_BYTES)
            .map_err(|_| SnapshotSessionError::EncodeFailed)?
            .to_be_bytes(),
    );
    frame.extend_from_slice(challenge.as_bytes());
    let authenticator = authenticate(secret, HELLO_AUTH_DOMAIN, &[&frame]);
    frame.extend_from_slice(&authenticator);
    Ok(frame)
}

/// Verify and decode an authenticated client hello without deserializing or
/// allocating attacker-declared fields.
///
/// # Errors
///
/// Returns `SnapshotSessionError` for malformed, unauthenticated, unsupported,
/// or oversized input.
pub fn decode_client_hello(
    frame: &[u8],
    secret: &SnapshotSessionSecret,
    limits: SnapshotSessionLimits,
) -> Result<SnapshotSessionChallenge, SnapshotSessionError> {
    if frame.len() > limits.max_hello_frame_bytes {
        return Err(SnapshotSessionError::HelloFrameTooLarge);
    }
    if frame.len() != HELLO_FRAME_BYTES {
        return Err(SnapshotSessionError::InvalidFrameLength);
    }
    validate_fixed_header(frame, CLIENT_HELLO_TYPE, CLIENT_TO_SERVER)?;
    if usize::try_from(read_u32(frame, 12)?).ok() != Some(HELLO_FRAME_BYTES) {
        return Err(SnapshotSessionError::InvalidFrameLength);
    }
    let prefix = &frame[..HELLO_PREFIX_BYTES];
    let authenticator = &frame[HELLO_PREFIX_BYTES..];
    let expected = authenticate(secret, HELLO_AUTH_DOMAIN, &[prefix]);
    if !constant_work_mac_eq(authenticator, &expected) {
        return Err(SnapshotSessionError::AuthenticationFailed);
    }
    let challenge = frame[16..HELLO_PREFIX_BYTES]
        .try_into()
        .map_err(|_| SnapshotSessionError::InvalidFrameLength)?;
    Ok(SnapshotSessionChallenge(challenge))
}

/// Encode an authenticated response with a fixed binary envelope and named
/// `MessagePack` metadata.
///
/// # Errors
///
/// Returns `SnapshotSessionError` when metadata is invalid, encoding fails, or
/// a configured bound is exceeded.
pub fn encode_authenticated_snapshot(
    snapshot_frame: &[u8],
    binding: SnapshotSessionBinding<'_>,
    secret: &SnapshotSessionSecret,
    limits: SnapshotSessionLimits,
) -> Result<Vec<u8>, SnapshotSessionError> {
    validate_incarnation(binding.engine_incarnation, limits)?;
    if binding.companion_generation == 0 {
        return Err(SnapshotSessionError::InvalidGeneration);
    }
    if snapshot_frame.len() > limits.max_snapshot_frame_bytes {
        return Err(SnapshotSessionError::SnapshotFrameTooLarge);
    }
    let metadata = ResponseMetadata {
        engine_incarnation: binding.engine_incarnation.clone(),
        snapshot_watermark: binding.snapshot_watermark,
        digest_key_id: binding.digest_key_id.to_vec(),
        companion_generation: binding.companion_generation,
        snapshot_frame_sha256: Sha256::digest(snapshot_frame).to_vec(),
    };
    let metadata =
        rmp_serde::to_vec_named(&metadata).map_err(|_| SnapshotSessionError::EncodeFailed)?;
    if metadata.len() > limits.max_header_bytes {
        return Err(SnapshotSessionError::HeaderTooLarge);
    }
    let total = RESPONSE_OVERHEAD_BYTES
        .checked_add(metadata.len())
        .and_then(|value| value.checked_add(snapshot_frame.len()))
        .ok_or(SnapshotSessionError::ResponseFrameTooLarge)?;
    if total > limits.max_response_frame_bytes {
        return Err(SnapshotSessionError::ResponseFrameTooLarge);
    }

    let mut frame = Vec::with_capacity(total);
    frame.extend_from_slice(MAGIC);
    frame.extend_from_slice(&SNAPSHOT_SESSION_SCHEMA_VERSION.to_be_bytes());
    frame.push(SNAPSHOT_RESPONSE_TYPE);
    frame.push(SERVER_TO_CLIENT);
    frame.extend_from_slice(
        &u64::try_from(total)
            .map_err(|_| SnapshotSessionError::ResponseFrameTooLarge)?
            .to_be_bytes(),
    );
    frame.extend_from_slice(
        &u32::try_from(metadata.len())
            .map_err(|_| SnapshotSessionError::HeaderTooLarge)?
            .to_be_bytes(),
    );
    frame.extend_from_slice(
        &u64::try_from(snapshot_frame.len())
            .map_err(|_| SnapshotSessionError::SnapshotFrameTooLarge)?
            .to_be_bytes(),
    );
    frame.extend_from_slice(binding.challenge.as_bytes());
    frame.extend_from_slice(&metadata);
    frame.extend_from_slice(snapshot_frame);
    let authenticator = authenticate(secret, RESPONSE_AUTH_DOMAIN, &[&frame]);
    frame.extend_from_slice(&authenticator);
    Ok(frame)
}

/// Authenticate and decode a response. No response field is deserialized and
/// no response-owned payload/header buffer is allocated before length checks
/// and MAC verification over borrowed input slices succeed.
///
/// # Errors
///
/// Returns `SnapshotSessionError` on malformed input, failed authentication, a
/// configured capacity breach, or an independent expectation mismatch.
pub fn decode_authenticated_snapshot(
    frame: &[u8],
    expected: SnapshotSessionExpectations<'_>,
    secret: &SnapshotSessionSecret,
    limits: SnapshotSessionLimits,
) -> Result<AuthenticatedSnapshot, SnapshotSessionError> {
    validate_incarnation(expected.engine_incarnation, limits)?;
    if frame.len() > limits.max_response_frame_bytes {
        return Err(SnapshotSessionError::ResponseFrameTooLarge);
    }
    if frame.len() < RESPONSE_OVERHEAD_BYTES {
        return Err(SnapshotSessionError::InvalidFrameLength);
    }
    validate_fixed_header(frame, SNAPSHOT_RESPONSE_TYPE, SERVER_TO_CLIENT)?;

    let declared_total = usize::try_from(read_u64(frame, 12)?)
        .map_err(|_| SnapshotSessionError::ResponseFrameTooLarge)?;
    let metadata_len =
        usize::try_from(read_u32(frame, 20)?).map_err(|_| SnapshotSessionError::HeaderTooLarge)?;
    let snapshot_len = usize::try_from(read_u64(frame, 24)?)
        .map_err(|_| SnapshotSessionError::SnapshotFrameTooLarge)?;
    if declared_total > limits.max_response_frame_bytes {
        return Err(SnapshotSessionError::ResponseFrameTooLarge);
    }
    if metadata_len > limits.max_header_bytes {
        return Err(SnapshotSessionError::HeaderTooLarge);
    }
    if snapshot_len > limits.max_snapshot_frame_bytes {
        return Err(SnapshotSessionError::SnapshotFrameTooLarge);
    }
    let expected_total = RESPONSE_OVERHEAD_BYTES
        .checked_add(metadata_len)
        .and_then(|value| value.checked_add(snapshot_len))
        .ok_or(SnapshotSessionError::InvalidFrameLength)?;
    if declared_total != expected_total || frame.len() != expected_total {
        return Err(SnapshotSessionError::InvalidFrameLength);
    }

    let challenge: &[u8; SNAPSHOT_SESSION_CHALLENGE_BYTES] = frame[32..RESPONSE_PREFIX_BYTES]
        .try_into()
        .map_err(|_| SnapshotSessionError::InvalidFrameLength)?;
    let metadata_end = RESPONSE_PREFIX_BYTES + metadata_len;
    let snapshot_end = metadata_end + snapshot_len;
    let metadata_bytes = &frame[RESPONSE_PREFIX_BYTES..metadata_end];
    let snapshot_bytes = &frame[metadata_end..snapshot_end];
    let authenticator = &frame[snapshot_end..];
    let computed = authenticate(secret, RESPONSE_AUTH_DOMAIN, &[&frame[..snapshot_end]]);
    if !constant_work_mac_eq(authenticator, &computed) {
        return Err(SnapshotSessionError::AuthenticationFailed);
    }
    if !constant_work_mac_eq(challenge, expected.challenge.as_bytes()) {
        return Err(SnapshotSessionError::ChallengeMismatch);
    }

    let metadata: ResponseMetadata = rmp_serde::from_slice(metadata_bytes)
        .map_err(|_| SnapshotSessionError::InvalidMessagePack)?;
    validate_response_metadata(&metadata, limits)?;
    if metadata.engine_incarnation != *expected.engine_incarnation {
        return Err(SnapshotSessionError::IncarnationMismatch);
    }
    if metadata.digest_key_id.as_slice() != expected.digest_key_id {
        return Err(SnapshotSessionError::KeyIdMismatch);
    }
    if metadata.snapshot_watermark < expected.minimum_snapshot_watermark {
        return Err(SnapshotSessionError::StaleWatermark);
    }
    if metadata.companion_generation < expected.minimum_companion_generation {
        return Err(SnapshotSessionError::StaleGeneration);
    }
    let checksum: [u8; SHA256_BYTES] = Sha256::digest(snapshot_bytes).into();
    if !constant_work_mac_eq(&metadata.snapshot_frame_sha256, &checksum) {
        return Err(SnapshotSessionError::InvalidChecksum);
    }
    let digest_key_id = metadata
        .digest_key_id
        .try_into()
        .map_err(|_| SnapshotSessionError::KeyIdMismatch)?;

    Ok(AuthenticatedSnapshot {
        engine_incarnation: metadata.engine_incarnation,
        snapshot_watermark: metadata.snapshot_watermark,
        digest_key_id,
        companion_generation: metadata.companion_generation,
        snapshot_frame: snapshot_bytes.to_vec(),
    })
}

/// Validate the fixed response prefix and return its bounded declared length.
///
/// Stream transports use this before allocating or reading the remainder of a
/// response. Full length consistency, authentication, and payload bounds are
/// still enforced by [`decode_authenticated_snapshot`].
///
/// # Errors
///
/// Returns [`SnapshotSessionError`] for a truncated or invalid prefix, or a
/// declared length outside the configured response bound.
pub fn authenticated_snapshot_frame_length(
    prefix: &[u8],
    limits: SnapshotSessionLimits,
) -> Result<usize, SnapshotSessionError> {
    if prefix.len() != SNAPSHOT_RESPONSE_LENGTH_PREFIX_BYTES {
        return Err(SnapshotSessionError::InvalidFrameLength);
    }
    validate_fixed_header(prefix, SNAPSHOT_RESPONSE_TYPE, SERVER_TO_CLIENT)?;
    let declared_total = usize::try_from(read_u64(prefix, 12)?)
        .map_err(|_| SnapshotSessionError::ResponseFrameTooLarge)?;
    if declared_total < RESPONSE_OVERHEAD_BYTES {
        return Err(SnapshotSessionError::InvalidFrameLength);
    }
    if declared_total > limits.max_response_frame_bytes {
        return Err(SnapshotSessionError::ResponseFrameTooLarge);
    }
    Ok(declared_total)
}

fn validate_fixed_header(
    frame: &[u8],
    expected_type: u8,
    expected_direction: u8,
) -> Result<(), SnapshotSessionError> {
    if frame.get(..8) != Some(MAGIC.as_slice()) {
        return Err(SnapshotSessionError::InvalidMagic);
    }
    if read_u16(frame, 8)? != SNAPSHOT_SESSION_SCHEMA_VERSION {
        return Err(SnapshotSessionError::UnsupportedSchema);
    }
    if frame.get(10).copied() != Some(expected_type) {
        return Err(SnapshotSessionError::InvalidMessageType);
    }
    if frame.get(11).copied() != Some(expected_direction) {
        return Err(SnapshotSessionError::InvalidDirection);
    }
    Ok(())
}

fn validate_response_metadata(
    metadata: &ResponseMetadata,
    limits: SnapshotSessionLimits,
) -> Result<(), SnapshotSessionError> {
    validate_incarnation(&metadata.engine_incarnation, limits)?;
    if metadata.digest_key_id.len() != SNAPSHOT_DIGEST_KEY_ID_BYTES {
        return Err(SnapshotSessionError::KeyIdMismatch);
    }
    if metadata.companion_generation == 0 {
        return Err(SnapshotSessionError::InvalidGeneration);
    }
    if metadata.snapshot_frame_sha256.len() != SHA256_BYTES {
        return Err(SnapshotSessionError::InvalidChecksum);
    }
    Ok(())
}

fn validate_incarnation(
    incarnation: &EngineIncarnation,
    limits: SnapshotSessionLimits,
) -> Result<(), SnapshotSessionError> {
    let valid =
        |value: &str| !value.is_empty() && value.len() <= limits.max_incarnation_component_bytes;
    if !valid(&incarnation.engine_id)
        || !valid(&incarnation.model_revision)
        || !valid(&incarnation.image_digest)
        || incarnation.process_started_unix_ns == 0
        || incarnation.attestation_sha256.len() != SHA256_BYTES
    {
        return Err(SnapshotSessionError::InvalidIncarnation);
    }
    Ok(())
}

fn read_u16(frame: &[u8], offset: usize) -> Result<u16, SnapshotSessionError> {
    frame
        .get(offset..offset + 2)
        .and_then(|bytes| bytes.try_into().ok())
        .map(u16::from_be_bytes)
        .ok_or(SnapshotSessionError::InvalidFrameLength)
}

fn read_u32(frame: &[u8], offset: usize) -> Result<u32, SnapshotSessionError> {
    frame
        .get(offset..offset + 4)
        .and_then(|bytes| bytes.try_into().ok())
        .map(u32::from_be_bytes)
        .ok_or(SnapshotSessionError::InvalidFrameLength)
}

fn read_u64(frame: &[u8], offset: usize) -> Result<u64, SnapshotSessionError> {
    frame
        .get(offset..offset + 8)
        .and_then(|bytes| bytes.try_into().ok())
        .map(u64::from_be_bytes)
        .ok_or(SnapshotSessionError::InvalidFrameLength)
}

fn authenticate(
    secret: &SnapshotSessionSecret,
    domain: &[u8],
    parts: &[&[u8]],
) -> [u8; SHA256_BYTES] {
    let mut hmac = HmacSha256::new(&secret.0);
    hmac.update(domain);
    for part in parts {
        hmac.update(&u64::try_from(part.len()).unwrap_or(u64::MAX).to_be_bytes());
        hmac.update(part);
    }
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
        for (index, secret_byte) in secret.iter().enumerate() {
            inner_pad[index] ^= secret_byte;
            outer_pad[index] ^= secret_byte;
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

    fn expectations(incarnation: &EngineIncarnation) -> SnapshotSessionExpectations<'_> {
        SnapshotSessionExpectations {
            challenge: CHALLENGE,
            engine_incarnation: incarnation,
            digest_key_id: &KEY_ID,
            minimum_snapshot_watermark: 9_000,
            minimum_companion_generation: 6,
        }
    }

    fn response() -> (Vec<u8>, EngineIncarnation) {
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
    fn authenticated_exchange_round_trips() {
        let limits = SnapshotSessionLimits::default();
        let secret = SnapshotSessionSecret::new(SECRET_BYTES);
        let hello = encode_client_hello(CHALLENGE, &secret, limits).unwrap();
        assert_eq!(
            decode_client_hello(&hello, &secret, limits).unwrap(),
            CHALLENGE
        );

        let (frame, incarnation) = response();
        let decoded =
            decode_authenticated_snapshot(&frame, expectations(&incarnation), &secret, limits)
                .unwrap();
        assert_eq!(decoded.engine_incarnation(), &incarnation);
        assert_eq!(decoded.snapshot_watermark(), 9_123);
        assert_eq!(decoded.digest_key_id(), &KEY_ID);
        assert_eq!(decoded.companion_generation(), 7);
        assert_eq!(decoded.snapshot_frame(), b"opaque-kv-snapshot-frame");
    }

    #[test]
    fn unauthenticated_hello_is_rejected() {
        let limits = SnapshotSessionLimits::default();
        let secret = SnapshotSessionSecret::new(SECRET_BYTES);
        let mut hello = encode_client_hello(CHALLENGE, &secret, limits).unwrap();
        hello[20] ^= 1;
        assert_eq!(
            decode_client_hello(&hello, &secret, limits),
            Err(SnapshotSessionError::AuthenticationFailed)
        );
        let wrong = SnapshotSessionSecret::new([0x44; 32]);
        let hello = encode_client_hello(CHALLENGE, &secret, limits).unwrap();
        assert_eq!(
            decode_client_hello(&hello, &wrong, limits),
            Err(SnapshotSessionError::AuthenticationFailed)
        );
    }

    #[test]
    fn response_tamper_is_rejected_before_decode() {
        let (mut frame, incarnation) = response();
        let secret = SnapshotSessionSecret::new(SECRET_BYTES);
        frame[RESPONSE_PREFIX_BYTES + 3] ^= 1;
        assert_eq!(
            decode_authenticated_snapshot(
                &frame,
                expectations(&incarnation),
                &secret,
                SnapshotSessionLimits::default(),
            ),
            Err(SnapshotSessionError::AuthenticationFailed)
        );

        let (mut frame, _) = response();
        let last_payload_byte = frame.len() - SHA256_BYTES - 1;
        frame[last_payload_byte] ^= 1;
        assert_eq!(
            decode_authenticated_snapshot(
                &frame,
                expectations(&incarnation),
                &secret,
                SnapshotSessionLimits::default(),
            ),
            Err(SnapshotSessionError::AuthenticationFailed)
        );

        let (frame, _) = response();
        let wrong = SnapshotSessionSecret::new([0x44; 32]);
        assert_eq!(
            decode_authenticated_snapshot(
                &frame,
                expectations(&incarnation),
                &wrong,
                SnapshotSessionLimits::default(),
            ),
            Err(SnapshotSessionError::AuthenticationFailed)
        );
    }

    #[test]
    fn malicious_lengths_are_rejected_without_deserialization() {
        let (mut frame, incarnation) = response();
        let secret = SnapshotSessionSecret::new(SECRET_BYTES);
        frame[20..24].copy_from_slice(&u32::MAX.to_be_bytes());
        assert_eq!(
            decode_authenticated_snapshot(
                &frame,
                expectations(&incarnation),
                &secret,
                SnapshotSessionLimits::default(),
            ),
            Err(SnapshotSessionError::HeaderTooLarge)
        );

        let (mut frame, _) = response();
        frame[24..32].copy_from_slice(&u64::MAX.to_be_bytes());
        assert_eq!(
            decode_authenticated_snapshot(
                &frame,
                expectations(&incarnation),
                &secret,
                SnapshotSessionLimits::default(),
            ),
            Err(SnapshotSessionError::SnapshotFrameTooLarge)
        );
    }

    #[test]
    fn stale_floors_and_identity_mixups_are_rejected() {
        let (frame, incarnation) = response();
        let secret = SnapshotSessionSecret::new(SECRET_BYTES);
        let limits = SnapshotSessionLimits::default();
        let mut expected = expectations(&incarnation);
        expected.minimum_snapshot_watermark = 9_124;
        assert_eq!(
            decode_authenticated_snapshot(&frame, expected, &secret, limits),
            Err(SnapshotSessionError::StaleWatermark)
        );
        let mut expected = expectations(&incarnation);
        expected.minimum_companion_generation = 8;
        assert_eq!(
            decode_authenticated_snapshot(&frame, expected, &secret, limits),
            Err(SnapshotSessionError::StaleGeneration)
        );
        let mut expected = expectations(&incarnation);
        expected.challenge = SnapshotSessionChallenge::new([0x32; 32]);
        assert_eq!(
            decode_authenticated_snapshot(&frame, expected, &secret, limits),
            Err(SnapshotSessionError::ChallengeMismatch)
        );
        let wrong_incarnation = EngineIncarnation {
            engine_id: "engine-b".to_owned(),
            ..incarnation.clone()
        };
        let mut expected = expectations(&incarnation);
        expected.engine_incarnation = &wrong_incarnation;
        assert_eq!(
            decode_authenticated_snapshot(&frame, expected, &secret, limits),
            Err(SnapshotSessionError::IncarnationMismatch)
        );
        let wrong_key = [0x7c; 32];
        let mut expected = expectations(&incarnation);
        expected.digest_key_id = &wrong_key;
        assert_eq!(
            decode_authenticated_snapshot(&frame, expected, &secret, limits),
            Err(SnapshotSessionError::KeyIdMismatch)
        );
    }

    #[test]
    fn truncation_trailing_version_type_and_direction_are_rejected() {
        let (frame, incarnation) = response();
        let secret = SnapshotSessionSecret::new(SECRET_BYTES);
        let limits = SnapshotSessionLimits::default();
        let decode = |candidate: &[u8]| {
            decode_authenticated_snapshot(candidate, expectations(&incarnation), &secret, limits)
        };
        assert_eq!(
            decode(&frame[..frame.len() - 1]),
            Err(SnapshotSessionError::InvalidFrameLength)
        );
        let mut trailing = frame.clone();
        trailing.push(0);
        assert_eq!(
            decode(&trailing),
            Err(SnapshotSessionError::InvalidFrameLength)
        );
        let mut version = frame.clone();
        version[9] ^= 1;
        assert_eq!(
            decode(&version),
            Err(SnapshotSessionError::UnsupportedSchema)
        );
        let mut message_type = frame.clone();
        message_type[10] = CLIENT_HELLO_TYPE;
        assert_eq!(
            decode(&message_type),
            Err(SnapshotSessionError::InvalidMessageType)
        );
        let mut direction = frame;
        direction[11] = CLIENT_TO_SERVER;
        assert_eq!(
            decode(&direction),
            Err(SnapshotSessionError::InvalidDirection)
        );
    }

    #[test]
    fn direction_domains_prevent_cross_message_authentication() {
        let limits = SnapshotSessionLimits::default();
        let secret = SnapshotSessionSecret::new(SECRET_BYTES);
        let mut hello = encode_client_hello(CHALLENGE, &secret, limits).unwrap();
        hello[10] = SNAPSHOT_RESPONSE_TYPE;
        hello[11] = SERVER_TO_CLIENT;
        assert_eq!(
            decode_client_hello(&hello, &secret, limits),
            Err(SnapshotSessionError::InvalidMessageType)
        );

        let authentic_prefix = &hello[..HELLO_PREFIX_BYTES];
        assert_ne!(
            authenticate(&secret, HELLO_AUTH_DOMAIN, &[authentic_prefix]),
            authenticate(&secret, RESPONSE_AUTH_DOMAIN, &[authentic_prefix])
        );
    }

    #[test]
    fn redaction_and_secret_nonserialization_hold() {
        let secret = SnapshotSessionSecret::new(SECRET_BYTES);
        assert_eq!(format!("{secret:?}"), "SnapshotSessionSecret([REDACTED])");
        assert_eq!(
            format!("{CHALLENGE:?}"),
            "SnapshotSessionChallenge([REDACTED])"
        );
        let (frame, _) = response();
        assert!(
            !frame
                .windows(SECRET_BYTES.len())
                .any(|part| part == SECRET_BYTES)
        );
        for error in [
            SnapshotSessionError::AuthenticationFailed,
            SnapshotSessionError::IncarnationMismatch,
            SnapshotSessionError::StaleWatermark,
            SnapshotSessionError::KeyIdMismatch,
        ] {
            assert!(!error.to_string().contains("engine-a"));
            assert!(!error.reason().contains("engine-a"));
        }
    }
}
