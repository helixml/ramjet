//! Authenticated framing for snapshot tail and lifecycle control messages.
//!
//! Every frame has a fixed binary prefix followed by small named-MessagePack
//! identity metadata, an opaque event payload, and an HMAC-SHA256 tag. The
//! decoder checks all fixed-width lengths and authenticates borrowed input
//! before `MessagePack` decoding or payload allocation. A successful decode is
//! intentionally consumable only through [`VerifiedTailFrame::apply_to`], so
//! event bytes cannot be separated from the lifecycle decision that admitted
//! them.
//!
//! [`TailSessionKey`] is not a third long-lived deployment secret. It is an
//! ephemeral, direction-specific key derived here from the authenticated
//! snapshot-session secret, challenge, generation, and direction. UDS peer
//! authentication remains outside this codec.

use std::fmt;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::{
    kv_snapshot::EngineIncarnation,
    snapshot_session::{
        SNAPSHOT_DIGEST_KEY_ID_BYTES, SnapshotSessionChallenge, SnapshotSessionSecret,
    },
    snapshot_tail::{
        AuthenticatedCaughtUpFrame, AuthenticatedIdentityFrame, AuthenticatedTailFrame,
        CaughtUpAction, IdentityAction, SnapshotTailFence, SnapshotTailFenceReason, TailAction,
    },
};

pub const SNAPSHOT_TAIL_SCHEMA_VERSION: u16 = 1;
pub const TAIL_SESSION_KEY_BYTES: usize = 32;

const SHA256_BYTES: usize = 32;
const SHA256_BLOCK_BYTES: usize = 64;
const MAGIC: &[u8; 8] = b"MDTAIL01";
const AUTH_DOMAIN: &[u8] = b"mini-dynamo/snapshot-tail/frame/auth/v1\0";
const KEY_DERIVATION_DOMAIN: &[u8] = b"mini-dynamo/snapshot-tail/key-derivation/v1\0";
const PREFIX_BYTES: usize = 96;
const FRAME_OVERHEAD_BYTES: usize = PREFIX_BYTES + SHA256_BYTES;
/// Bytes required to validate the fixed tail identity and declared total
/// before allocating the rest of a stream frame.
pub const TAIL_FRAME_LENGTH_PREFIX_BYTES: usize = 20;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum TailFrameType {
    Event = 1,
    CaughtUp = 2,
    Identity = 3,
    Disconnect = 4,
}

impl TryFrom<u8> for TailFrameType {
    type Error = TailWireError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::Event),
            2 => Ok(Self::CaughtUp),
            3 => Ok(Self::Identity),
            4 => Ok(Self::Disconnect),
            _ => Err(TailWireError::InvalidMessageType),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum TailDirection {
    RouterToCompanion = 1,
    CompanionToRouter = 2,
}

impl TryFrom<u8> for TailDirection {
    type Error = TailWireError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::RouterToCompanion),
            2 => Ok(Self::CompanionToRouter),
            _ => Err(TailWireError::InvalidDirection),
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct TailWireLimits {
    pub max_frame_bytes: usize,
    pub max_metadata_bytes: usize,
    pub max_payload_bytes: usize,
    pub max_incarnation_component_bytes: usize,
}

impl Default for TailWireLimits {
    fn default() -> Self {
        Self {
            max_frame_bytes: 8 * 1024 * 1024 + 4 * 1024,
            max_metadata_bytes: 4 * 1024,
            max_payload_bytes: 8 * 1024 * 1024,
            max_incarnation_component_bytes: 512,
        }
    }
}

/// Ephemeral key derived by the transport from one authenticated session.
///
/// It deliberately implements neither `Clone` nor serialization, redacts its
/// debug representation, and clears directly owned bytes on drop.
pub struct TailSessionKey([u8; TAIL_SESSION_KEY_BYTES]);

impl TailSessionKey {
    /// Derive an ephemeral direction-specific key from the authenticated
    /// snapshot session, its fresh challenge, and companion generation.
    #[must_use]
    pub fn derive(
        secret: &SnapshotSessionSecret,
        session_id: SnapshotSessionChallenge,
        companion_generation: u64,
        direction: TailDirection,
    ) -> Self {
        let generation = companion_generation.to_be_bytes();
        let direction = [direction as u8];
        Self(secret.derive_subkey(
            KEY_DERIVATION_DOMAIN,
            &[session_id.as_bytes(), &generation, &direction],
        ))
    }
}

impl fmt::Debug for TailSessionKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("TailSessionKey([REDACTED])")
    }
}

impl Drop for TailSessionKey {
    fn drop(&mut self) {
        self.0.fill(0);
    }
}

#[derive(Clone, Copy)]
pub struct TailFrameBinding<'a> {
    pub frame_type: TailFrameType,
    pub direction: TailDirection,
    pub session_id: SnapshotSessionChallenge,
    pub message_sequence: u64,
    pub delivery_sequence: u64,
    pub event_watermark: u64,
    pub engine_incarnation: &'a EngineIncarnation,
    pub digest_key_id: &'a [u8; SNAPSHOT_DIGEST_KEY_ID_BYTES],
    pub companion_generation: u64,
}

#[derive(Clone, Copy)]
pub struct TailSessionExpectations<'a> {
    pub direction: TailDirection,
    pub session_id: SnapshotSessionChallenge,
    pub first_message_sequence: u64,
    pub engine_incarnation: &'a EngineIncarnation,
    pub digest_key_id: &'a [u8; SNAPSHOT_DIGEST_KEY_ID_BYTES],
    pub companion_generation: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct IdentityMetadata {
    engine_incarnation: EngineIncarnation,
    #[serde(with = "serde_bytes")]
    digest_key_id: Vec<u8>,
}

/// Stateful verifier for one direction of one authenticated session.
///
/// Message sequence is dense across event and control frames. It advances only
/// after the entire frame, including semantic metadata, has been authenticated
/// and validated. Replaying any accepted frame therefore fails closed.
pub struct TailFrameDecoder<'a> {
    key: &'a TailSessionKey,
    direction: TailDirection,
    session_id: SnapshotSessionChallenge,
    next_message_sequence: u64,
    engine_incarnation: &'a EngineIncarnation,
    digest_key_id: &'a [u8; SNAPSHOT_DIGEST_KEY_ID_BYTES],
    companion_generation: u64,
    limits: TailWireLimits,
}

impl<'a> TailFrameDecoder<'a> {
    /// Construct a decoder bound to independently authenticated expectations.
    ///
    /// # Errors
    ///
    /// Rejects an invalid incarnation, zero generation, or sequence zero.
    pub fn new(
        key: &'a TailSessionKey,
        expected: TailSessionExpectations<'a>,
        limits: TailWireLimits,
    ) -> Result<Self, TailWireError> {
        validate_incarnation(expected.engine_incarnation, limits)?;
        if expected.companion_generation == 0 {
            return Err(TailWireError::InvalidGeneration);
        }
        if expected.first_message_sequence == 0 {
            return Err(TailWireError::InvalidMessageSequence);
        }
        Ok(Self {
            key,
            direction: expected.direction,
            session_id: expected.session_id,
            next_message_sequence: expected.first_message_sequence,
            engine_incarnation: expected.engine_incarnation,
            digest_key_id: expected.digest_key_id,
            companion_generation: expected.companion_generation,
            limits,
        })
    }

    /// Authenticate and decode one frame without allocating from untrusted
    /// length declarations.
    ///
    /// # Errors
    ///
    /// Returns a content-free [`TailWireError`] on malformed, oversized,
    /// unauthenticated, replayed, or identity-mismatched input.
    pub fn decode(&mut self, frame: &[u8]) -> Result<VerifiedTailFrame, TailWireError> {
        if frame.len() > self.limits.max_frame_bytes {
            return Err(TailWireError::FrameTooLarge);
        }
        if frame.len() < FRAME_OVERHEAD_BYTES {
            return Err(TailWireError::InvalidFrameLength);
        }
        if frame.get(..8) != Some(MAGIC.as_slice()) {
            return Err(TailWireError::InvalidMagic);
        }
        if read_u16(frame, 8)? != SNAPSHOT_TAIL_SCHEMA_VERSION {
            return Err(TailWireError::UnsupportedSchema);
        }
        let frame_type = TailFrameType::try_from(frame[10])?;
        let direction = TailDirection::try_from(frame[11])?;
        if direction != self.direction {
            return Err(TailWireError::InvalidDirection);
        }

        let declared_total =
            usize::try_from(read_u64(frame, 12)?).map_err(|_| TailWireError::FrameTooLarge)?;
        let metadata_len =
            usize::try_from(read_u32(frame, 20)?).map_err(|_| TailWireError::MetadataTooLarge)?;
        let payload_len =
            usize::try_from(read_u64(frame, 24)?).map_err(|_| TailWireError::PayloadTooLarge)?;
        if declared_total > self.limits.max_frame_bytes {
            return Err(TailWireError::FrameTooLarge);
        }
        if metadata_len > self.limits.max_metadata_bytes {
            return Err(TailWireError::MetadataTooLarge);
        }
        if payload_len > self.limits.max_payload_bytes {
            return Err(TailWireError::PayloadTooLarge);
        }
        let expected_total = FRAME_OVERHEAD_BYTES
            .checked_add(metadata_len)
            .and_then(|value| value.checked_add(payload_len))
            .ok_or(TailWireError::InvalidFrameLength)?;
        if declared_total != expected_total || frame.len() != expected_total {
            return Err(TailWireError::InvalidFrameLength);
        }

        let authenticated_end = expected_total - SHA256_BYTES;
        let supplied_mac = &frame[authenticated_end..];
        let computed_mac = authenticate(self.key, &frame[..authenticated_end]);
        if !constant_work_mac_eq(supplied_mac, &computed_mac) {
            return Err(TailWireError::AuthenticationFailed);
        }

        if !constant_work_mac_eq(&frame[32..64], self.session_id.as_bytes()) {
            return Err(TailWireError::SessionMismatch);
        }
        let message_sequence = read_u64(frame, 64)?;
        if message_sequence != self.next_message_sequence {
            return Err(TailWireError::InvalidMessageSequence);
        }
        if message_sequence == u64::MAX {
            return Err(TailWireError::SequenceOverflow);
        }
        let delivery_sequence = read_u64(frame, 72)?;
        let event_watermark = read_u64(frame, 80)?;
        let companion_generation = read_u64(frame, 88)?;

        let metadata_end = PREFIX_BYTES + metadata_len;
        let metadata: IdentityMetadata = rmp_serde::from_slice(&frame[PREFIX_BYTES..metadata_end])
            .map_err(|_| TailWireError::InvalidMessagePack)?;
        validate_incarnation(&metadata.engine_incarnation, self.limits)?;
        if metadata.digest_key_id.len() != SNAPSHOT_DIGEST_KEY_ID_BYTES {
            return Err(TailWireError::InvalidKeyId);
        }
        if companion_generation == 0 {
            return Err(TailWireError::InvalidGeneration);
        }
        if metadata.engine_incarnation != *self.engine_incarnation {
            return Err(TailWireError::IncarnationMismatch);
        }
        if !constant_work_mac_eq(&metadata.digest_key_id, self.digest_key_id) {
            return Err(TailWireError::KeyIdMismatch);
        }
        if companion_generation != self.companion_generation {
            return Err(TailWireError::GenerationMismatch);
        }
        validate_kind_fields(frame_type, delivery_sequence, event_watermark, payload_len)?;

        let payload = frame[metadata_end..authenticated_end].to_vec();
        self.next_message_sequence = message_sequence + 1;
        Ok(VerifiedTailFrame {
            frame_type,
            direction,
            message_sequence,
            delivery_sequence,
            event_watermark,
            engine_incarnation: metadata.engine_incarnation,
            digest_key_id: metadata
                .digest_key_id
                .try_into()
                .map_err(|_| TailWireError::InvalidKeyId)?,
            companion_generation,
            payload,
        })
    }
}

/// A fully authenticated owned frame. Fields remain private so event payload
/// can be released only by the matching lifecycle transition.
pub struct VerifiedTailFrame {
    frame_type: TailFrameType,
    direction: TailDirection,
    message_sequence: u64,
    delivery_sequence: u64,
    event_watermark: u64,
    engine_incarnation: EngineIncarnation,
    digest_key_id: [u8; SNAPSHOT_DIGEST_KEY_ID_BYTES],
    companion_generation: u64,
    payload: Vec<u8>,
}

impl fmt::Debug for VerifiedTailFrame {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("VerifiedTailFrame")
            .field("frame_type", &self.frame_type)
            .field("direction", &self.direction)
            .field("message_sequence", &self.message_sequence)
            .field("delivery_sequence", &self.delivery_sequence)
            .field("event_watermark", &self.event_watermark)
            .field("engine_incarnation", &"[REDACTED]")
            .field("digest_key_id", &"[REDACTED]")
            .field("companion_generation", &self.companion_generation)
            .field("payload_bytes", &self.payload.len())
            .finish()
    }
}

impl VerifiedTailFrame {
    /// Apply authenticated identity and sequencing to the lifecycle fence.
    /// Tail payload is returned only for an admitted `Apply` transition.
    #[must_use]
    pub fn apply_to(self, fence: &mut SnapshotTailFence) -> VerifiedTailAction {
        match self.frame_type {
            TailFrameType::Event => {
                let authenticated = AuthenticatedTailFrame::from_authenticated_session(
                    &self.engine_incarnation,
                    &self.digest_key_id,
                    self.companion_generation,
                    self.delivery_sequence,
                    self.event_watermark,
                );
                match fence.accept_tail(&authenticated) {
                    TailAction::Apply {
                        delivery_sequence,
                        event_watermark,
                    } => VerifiedTailAction::Apply {
                        delivery_sequence,
                        event_watermark,
                        payload: self.payload,
                    },
                    TailAction::Duplicate => VerifiedTailAction::Duplicate,
                    TailAction::Fenced(reason) => VerifiedTailAction::Fenced(reason),
                }
            }
            TailFrameType::CaughtUp => {
                let authenticated = AuthenticatedCaughtUpFrame::from_authenticated_session(
                    &self.engine_incarnation,
                    &self.digest_key_id,
                    self.companion_generation,
                    self.delivery_sequence,
                    self.event_watermark,
                );
                match fence.caught_up(&authenticated) {
                    CaughtUpAction::Ready => VerifiedTailAction::Ready,
                    CaughtUpAction::AlreadyReady => VerifiedTailAction::AlreadyReady,
                    CaughtUpAction::Fenced(reason) => VerifiedTailAction::Fenced(reason),
                }
            }
            TailFrameType::Identity => {
                let authenticated = AuthenticatedIdentityFrame::from_authenticated_session(
                    &self.engine_incarnation,
                    &self.digest_key_id,
                    self.companion_generation,
                );
                match fence.observe_identity(&authenticated) {
                    IdentityAction::Current => VerifiedTailAction::IdentityCurrent,
                    IdentityAction::Fenced(reason) => VerifiedTailAction::Fenced(reason),
                }
            }
            TailFrameType::Disconnect => {
                fence.disconnected();
                VerifiedTailAction::Fenced(SnapshotTailFenceReason::Disconnected)
            }
        }
    }
}

pub enum VerifiedTailAction {
    /// The lifecycle has admitted this exact payload. A downstream payload
    /// decode or index-apply error must immediately fence this bootstrap and
    /// must never publish its partially updated index. Call
    /// [`SnapshotTailFence::application_failed`] before discarding the private
    /// generation when either operation fails.
    Apply {
        delivery_sequence: u64,
        event_watermark: u64,
        payload: Vec<u8>,
    },
    Duplicate,
    Ready,
    AlreadyReady,
    IdentityCurrent,
    Fenced(SnapshotTailFenceReason),
}

impl fmt::Debug for VerifiedTailAction {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Apply {
                delivery_sequence,
                event_watermark,
                payload,
            } => formatter
                .debug_struct("Apply")
                .field("delivery_sequence", delivery_sequence)
                .field("event_watermark", event_watermark)
                .field("payload_bytes", &payload.len())
                .finish(),
            Self::Duplicate => formatter.write_str("Duplicate"),
            Self::Ready => formatter.write_str("Ready"),
            Self::AlreadyReady => formatter.write_str("AlreadyReady"),
            Self::IdentityCurrent => formatter.write_str("IdentityCurrent"),
            Self::Fenced(reason) => formatter.debug_tuple("Fenced").field(reason).finish(),
        }
    }
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum TailWireError {
    #[error("snapshot tail frame exceeds configured byte limit")]
    FrameTooLarge,
    #[error("snapshot tail metadata exceeds configured byte limit")]
    MetadataTooLarge,
    #[error("snapshot tail payload exceeds configured byte limit")]
    PayloadTooLarge,
    #[error("invalid snapshot tail frame length")]
    InvalidFrameLength,
    #[error("invalid snapshot tail magic")]
    InvalidMagic,
    #[error("unsupported snapshot tail schema")]
    UnsupportedSchema,
    #[error("invalid snapshot tail message type")]
    InvalidMessageType,
    #[error("invalid snapshot tail direction")]
    InvalidDirection,
    #[error("snapshot tail authentication failed")]
    AuthenticationFailed,
    #[error("snapshot tail session does not match")]
    SessionMismatch,
    #[error("invalid snapshot tail message sequence")]
    InvalidMessageSequence,
    #[error("snapshot tail sequence overflow")]
    SequenceOverflow,
    #[error("invalid snapshot tail MessagePack")]
    InvalidMessagePack,
    #[error("invalid snapshot tail engine incarnation")]
    InvalidIncarnation,
    #[error("snapshot tail engine incarnation does not match")]
    IncarnationMismatch,
    #[error("invalid snapshot tail digest key identifier")]
    InvalidKeyId,
    #[error("snapshot tail digest key identifier does not match")]
    KeyIdMismatch,
    #[error("invalid snapshot tail companion generation")]
    InvalidGeneration,
    #[error("snapshot tail companion generation does not match")]
    GenerationMismatch,
    #[error("invalid snapshot tail frame fields")]
    InvalidFrameFields,
    #[error("snapshot tail encoding failed")]
    EncodeFailed,
}

impl TailWireError {
    #[must_use]
    pub const fn reason(self) -> &'static str {
        match self {
            Self::FrameTooLarge => "frame_too_large",
            Self::MetadataTooLarge => "metadata_too_large",
            Self::PayloadTooLarge => "payload_too_large",
            Self::InvalidFrameLength => "invalid_frame_length",
            Self::InvalidMagic => "invalid_magic",
            Self::UnsupportedSchema => "unsupported_schema",
            Self::InvalidMessageType => "invalid_message_type",
            Self::InvalidDirection => "invalid_direction",
            Self::AuthenticationFailed => "authentication_failed",
            Self::SessionMismatch => "session_mismatch",
            Self::InvalidMessageSequence => "invalid_message_sequence",
            Self::SequenceOverflow => "sequence_overflow",
            Self::InvalidMessagePack => "invalid_messagepack",
            Self::InvalidIncarnation => "invalid_incarnation",
            Self::IncarnationMismatch => "incarnation_mismatch",
            Self::InvalidKeyId => "invalid_key_id",
            Self::KeyIdMismatch => "key_id_mismatch",
            Self::InvalidGeneration => "invalid_generation",
            Self::GenerationMismatch => "generation_mismatch",
            Self::InvalidFrameFields => "invalid_frame_fields",
            Self::EncodeFailed => "encode_failed",
        }
    }
}

/// Encode one authenticated frame.
///
/// # Errors
///
/// Rejects invalid metadata, type-specific fields, or configured size limits.
pub fn encode_tail_frame(
    payload: &[u8],
    binding: TailFrameBinding<'_>,
    key: &TailSessionKey,
    limits: TailWireLimits,
) -> Result<Vec<u8>, TailWireError> {
    validate_incarnation(binding.engine_incarnation, limits)?;
    if binding.companion_generation == 0 {
        return Err(TailWireError::InvalidGeneration);
    }
    if binding.message_sequence == 0 || binding.message_sequence == u64::MAX {
        return Err(TailWireError::InvalidMessageSequence);
    }
    validate_kind_fields(
        binding.frame_type,
        binding.delivery_sequence,
        binding.event_watermark,
        payload.len(),
    )?;
    if payload.len() > limits.max_payload_bytes {
        return Err(TailWireError::PayloadTooLarge);
    }

    let metadata = rmp_serde::to_vec_named(&IdentityMetadata {
        engine_incarnation: binding.engine_incarnation.clone(),
        digest_key_id: binding.digest_key_id.to_vec(),
    })
    .map_err(|_| TailWireError::EncodeFailed)?;
    if metadata.len() > limits.max_metadata_bytes {
        return Err(TailWireError::MetadataTooLarge);
    }
    let total = FRAME_OVERHEAD_BYTES
        .checked_add(metadata.len())
        .and_then(|value| value.checked_add(payload.len()))
        .ok_or(TailWireError::FrameTooLarge)?;
    if total > limits.max_frame_bytes {
        return Err(TailWireError::FrameTooLarge);
    }

    let mut frame = Vec::with_capacity(total);
    frame.extend_from_slice(MAGIC);
    frame.extend_from_slice(&SNAPSHOT_TAIL_SCHEMA_VERSION.to_be_bytes());
    frame.push(binding.frame_type as u8);
    frame.push(binding.direction as u8);
    frame.extend_from_slice(
        &u64::try_from(total)
            .map_err(|_| TailWireError::FrameTooLarge)?
            .to_be_bytes(),
    );
    frame.extend_from_slice(
        &u32::try_from(metadata.len())
            .map_err(|_| TailWireError::MetadataTooLarge)?
            .to_be_bytes(),
    );
    frame.extend_from_slice(
        &u64::try_from(payload.len())
            .map_err(|_| TailWireError::PayloadTooLarge)?
            .to_be_bytes(),
    );
    frame.extend_from_slice(binding.session_id.as_bytes());
    frame.extend_from_slice(&binding.message_sequence.to_be_bytes());
    frame.extend_from_slice(&binding.delivery_sequence.to_be_bytes());
    frame.extend_from_slice(&binding.event_watermark.to_be_bytes());
    frame.extend_from_slice(&binding.companion_generation.to_be_bytes());
    debug_assert_eq!(frame.len(), PREFIX_BYTES);
    frame.extend_from_slice(&metadata);
    frame.extend_from_slice(payload);
    let authenticator = authenticate(key, &frame);
    frame.extend_from_slice(&authenticator);
    Ok(frame)
}

/// Validate the fixed tail prefix and return its bounded declared length.
///
/// Full field consistency, authentication, and metadata/payload bounds remain
/// the responsibility of [`TailFrameDecoder::decode`].
///
/// # Errors
///
/// Returns [`TailWireError`] for a truncated or invalid prefix, or a declared
/// length outside the configured tail-frame bound.
pub fn tail_frame_length(prefix: &[u8], limits: TailWireLimits) -> Result<usize, TailWireError> {
    if prefix.len() != TAIL_FRAME_LENGTH_PREFIX_BYTES {
        return Err(TailWireError::InvalidFrameLength);
    }
    if prefix.get(..8) != Some(MAGIC.as_slice()) {
        return Err(TailWireError::InvalidMagic);
    }
    if read_u16(prefix, 8)? != SNAPSHOT_TAIL_SCHEMA_VERSION {
        return Err(TailWireError::UnsupportedSchema);
    }
    TailFrameType::try_from(prefix[10])?;
    TailDirection::try_from(prefix[11])?;
    let declared_total =
        usize::try_from(read_u64(prefix, 12)?).map_err(|_| TailWireError::FrameTooLarge)?;
    if declared_total < FRAME_OVERHEAD_BYTES {
        return Err(TailWireError::InvalidFrameLength);
    }
    if declared_total > limits.max_frame_bytes {
        return Err(TailWireError::FrameTooLarge);
    }
    Ok(declared_total)
}

fn validate_kind_fields(
    frame_type: TailFrameType,
    delivery_sequence: u64,
    event_watermark: u64,
    payload_len: usize,
) -> Result<(), TailWireError> {
    match frame_type {
        TailFrameType::Event if delivery_sequence == 0 || payload_len == 0 => {
            Err(TailWireError::InvalidFrameFields)
        }
        TailFrameType::Event => Ok(()),
        TailFrameType::CaughtUp if payload_len == 0 => Ok(()),
        TailFrameType::Identity | TailFrameType::Disconnect
            if delivery_sequence == 0 && event_watermark == 0 && payload_len == 0 =>
        {
            Ok(())
        }
        TailFrameType::CaughtUp | TailFrameType::Identity | TailFrameType::Disconnect => {
            Err(TailWireError::InvalidFrameFields)
        }
    }
}

fn validate_incarnation(
    incarnation: &EngineIncarnation,
    limits: TailWireLimits,
) -> Result<(), TailWireError> {
    let valid =
        |value: &str| !value.is_empty() && value.len() <= limits.max_incarnation_component_bytes;
    if !valid(&incarnation.engine_id)
        || !valid(&incarnation.model_revision)
        || !valid(&incarnation.image_digest)
        || incarnation.process_started_unix_ns == 0
        || incarnation.attestation_sha256.len() != SHA256_BYTES
    {
        return Err(TailWireError::InvalidIncarnation);
    }
    Ok(())
}

fn read_u16(frame: &[u8], offset: usize) -> Result<u16, TailWireError> {
    frame
        .get(offset..offset + 2)
        .and_then(|bytes| bytes.try_into().ok())
        .map(u16::from_be_bytes)
        .ok_or(TailWireError::InvalidFrameLength)
}

fn read_u32(frame: &[u8], offset: usize) -> Result<u32, TailWireError> {
    frame
        .get(offset..offset + 4)
        .and_then(|bytes| bytes.try_into().ok())
        .map(u32::from_be_bytes)
        .ok_or(TailWireError::InvalidFrameLength)
}

fn read_u64(frame: &[u8], offset: usize) -> Result<u64, TailWireError> {
    frame
        .get(offset..offset + 8)
        .and_then(|bytes| bytes.try_into().ok())
        .map(u64::from_be_bytes)
        .ok_or(TailWireError::InvalidFrameLength)
}

fn authenticate(key: &TailSessionKey, authenticated_frame: &[u8]) -> [u8; SHA256_BYTES] {
    let mut hmac = HmacSha256::new(&key.0);
    hmac.update(AUTH_DOMAIN);
    hmac.update(
        &u64::try_from(authenticated_frame.len())
            .unwrap_or(u64::MAX)
            .to_be_bytes(),
    );
    hmac.update(authenticated_frame);
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
    fn new(secret: &[u8; TAIL_SESSION_KEY_BYTES]) -> Self {
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

    const SESSION: SnapshotSessionChallenge = SnapshotSessionChallenge::new([0x31; 32]);
    const OTHER_SESSION: SnapshotSessionChallenge = SnapshotSessionChallenge::new([0x32; 32]);
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

    fn binding(
        incarnation: &EngineIncarnation,
        message: u64,
        delivery: u64,
    ) -> TailFrameBinding<'_> {
        TailFrameBinding {
            frame_type: TailFrameType::Event,
            direction: TailDirection::CompanionToRouter,
            session_id: SESSION,
            message_sequence: message,
            delivery_sequence: delivery,
            event_watermark: 9_000 + delivery * 17,
            engine_incarnation: incarnation,
            digest_key_id: &KEY_ID,
            companion_generation: 7,
        }
    }

    fn expectations(incarnation: &EngineIncarnation) -> TailSessionExpectations<'_> {
        TailSessionExpectations {
            direction: TailDirection::CompanionToRouter,
            session_id: SESSION,
            first_message_sequence: 1,
            engine_incarnation: incarnation,
            digest_key_id: &KEY_ID,
            companion_generation: 7,
        }
    }

    fn key() -> TailSessionKey {
        TailSessionKey::derive(
            &SnapshotSessionSecret::new(*b"snapshot-session-secret-32-byte!"),
            SESSION,
            7,
            TailDirection::CompanionToRouter,
        )
    }

    fn frame() -> (Vec<u8>, EngineIncarnation, TailSessionKey) {
        let incarnation = incarnation();
        let key = key();
        let frame = encode_tail_frame(
            b"opaque-event",
            binding(&incarnation, 1, 1),
            &key,
            TailWireLimits::default(),
        )
        .unwrap();
        (frame, incarnation, key)
    }

    #[test]
    fn round_trip_and_replay_are_fail_closed() {
        let (frame, incarnation, key) = frame();
        let mut decoder =
            TailFrameDecoder::new(&key, expectations(&incarnation), TailWireLimits::default())
                .unwrap();
        let verified = decoder.decode(&frame).unwrap();
        assert_eq!(verified.delivery_sequence, 1);
        assert_eq!(verified.event_watermark, 9_017);
        assert_eq!(verified.payload, b"opaque-event");
        assert_eq!(
            decoder.decode(&frame).unwrap_err(),
            TailWireError::InvalidMessageSequence
        );
    }

    #[test]
    fn any_authenticated_region_tamper_fails_mac() {
        let (frame, incarnation, key) = frame();
        for offset in [32, 64, 72, 80, 88, PREFIX_BYTES, frame.len() - 33] {
            let mut tampered = frame.clone();
            tampered[offset] ^= 1;
            let mut decoder =
                TailFrameDecoder::new(&key, expectations(&incarnation), TailWireLimits::default())
                    .unwrap();
            assert_eq!(
                decoder.decode(&tampered).unwrap_err(),
                TailWireError::AuthenticationFailed
            );
        }
    }

    #[test]
    fn wrong_direction_and_session_are_rejected() {
        let (frame, incarnation, key) = frame();
        let mut opposite = expectations(&incarnation);
        opposite.direction = TailDirection::RouterToCompanion;
        let mut decoder = TailFrameDecoder::new(&key, opposite, TailWireLimits::default()).unwrap();
        assert_eq!(
            decoder.decode(&frame).unwrap_err(),
            TailWireError::InvalidDirection
        );

        let mut other_session = expectations(&incarnation);
        other_session.session_id = OTHER_SESSION;
        let mut decoder =
            TailFrameDecoder::new(&key, other_session, TailWireLimits::default()).unwrap();
        assert_eq!(
            decoder.decode(&frame).unwrap_err(),
            TailWireError::SessionMismatch
        );
    }

    #[test]
    fn wrong_first_message_sequence_is_rejected() {
        let (frame, incarnation, key) = frame();
        let mut expected = expectations(&incarnation);
        expected.first_message_sequence = 2;
        let mut decoder = TailFrameDecoder::new(&key, expected, TailWireLimits::default()).unwrap();
        assert_eq!(
            decoder.decode(&frame).unwrap_err(),
            TailWireError::InvalidMessageSequence
        );
    }

    #[test]
    fn sparse_real_watermarks_do_not_look_like_delivery_gaps() {
        let incarnation = incarnation();
        let key = key();
        let mut second = binding(&incarnation, 2, 2);
        second.event_watermark = 90_000;
        let first = encode_tail_frame(
            b"first",
            binding(&incarnation, 1, 1),
            &key,
            TailWireLimits::default(),
        )
        .unwrap();
        let second = encode_tail_frame(b"second", second, &key, TailWireLimits::default()).unwrap();
        let mut decoder =
            TailFrameDecoder::new(&key, expectations(&incarnation), TailWireLimits::default())
                .unwrap();
        assert_eq!(decoder.decode(&first).unwrap().delivery_sequence, 1);
        let verified = decoder.decode(&second).unwrap();
        assert_eq!(verified.delivery_sequence, 2);
        assert_eq!(verified.event_watermark, 90_000);
    }

    #[test]
    fn huge_declared_lengths_and_truncation_fail_before_decode() {
        let (frame, incarnation, key) = frame();
        let mut huge = frame.clone();
        huge[24..32].copy_from_slice(&u64::MAX.to_be_bytes());
        let mut decoder =
            TailFrameDecoder::new(&key, expectations(&incarnation), TailWireLimits::default())
                .unwrap();
        assert_eq!(
            decoder.decode(&huge).unwrap_err(),
            TailWireError::PayloadTooLarge
        );

        let mut decoder =
            TailFrameDecoder::new(&key, expectations(&incarnation), TailWireLimits::default())
                .unwrap();
        assert_eq!(
            decoder.decode(&frame[..frame.len() - 1]).unwrap_err(),
            TailWireError::InvalidFrameLength
        );
    }

    #[test]
    fn debug_output_redacts_keys_identity_and_payload() {
        let (frame, incarnation, key) = frame();
        let mut decoder =
            TailFrameDecoder::new(&key, expectations(&incarnation), TailWireLimits::default())
                .unwrap();
        let verified = decoder.decode(&frame).unwrap();
        let debug = format!("{verified:?}");
        assert!(!debug.contains("engine-a"));
        assert!(!debug.contains("opaque-event"));
        assert!(!debug.contains("107, 107"));
        assert_eq!(format!("{key:?}"), "TailSessionKey([REDACTED])");
        assert!(
            !TailWireError::AuthenticationFailed
                .to_string()
                .contains("opaque")
        );
    }

    #[test]
    fn derived_keys_are_session_generation_and_direction_bound() {
        let secret = SnapshotSessionSecret::new(*b"snapshot-session-secret-32-byte!");
        let baseline =
            TailSessionKey::derive(&secret, SESSION, 7, TailDirection::CompanionToRouter);
        let other_direction =
            TailSessionKey::derive(&secret, SESSION, 7, TailDirection::RouterToCompanion);
        let other_generation =
            TailSessionKey::derive(&secret, SESSION, 8, TailDirection::CompanionToRouter);
        let other_session =
            TailSessionKey::derive(&secret, OTHER_SESSION, 7, TailDirection::CompanionToRouter);
        assert_ne!(baseline.0, other_direction.0);
        assert_ne!(baseline.0, other_generation.0);
        assert_ne!(baseline.0, other_session.0);
    }

    #[test]
    fn control_frames_reject_payload_and_nonzero_identity_progress() {
        let incarnation = incarnation();
        let key = key();
        let mut control = binding(&incarnation, 1, 0);
        control.frame_type = TailFrameType::Identity;
        control.event_watermark = 0;
        assert_eq!(
            encode_tail_frame(b"not-empty", control, &key, TailWireLimits::default()).unwrap_err(),
            TailWireError::InvalidFrameFields
        );
    }
}
