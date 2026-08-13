//! Versioned, bounded wire contract for authoritative exact-KV snapshots.
//!
//! The frame is a small `MessagePack` envelope containing an opaque `MessagePack`
//! body and its SHA-256 checksum. Consumers verify the envelope and checksum
//! before decoding or exposing any snapshot records. Errors deliberately carry
//! no source values, token material, hashes, digests, or engine identifiers.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use serde_bytes::ByteBuf;
use sha2::{Digest, Sha256};
use thiserror::Error;

pub const SNAPSHOT_SCHEMA_VERSION: u16 = 2;
const SHA256_BYTES: usize = 32;

#[allow(clippy::struct_field_names)]
#[derive(Clone, Copy, Debug)]
pub struct SnapshotLimits {
    pub max_frame_bytes: usize,
    pub max_payload_bytes: usize,
    pub max_incarnation_component_bytes: usize,
    pub max_key_id_bytes: usize,
    pub max_groups: usize,
    pub max_records: usize,
    pub max_external_hash_bytes: usize,
    pub max_total_external_hash_bytes: usize,
    pub max_prefix_token_ids: u64,
}

impl Default for SnapshotLimits {
    fn default() -> Self {
        Self {
            max_frame_bytes: 64 * 1024 * 1024,
            max_payload_bytes: 64 * 1024 * 1024,
            max_incarnation_component_bytes: 512,
            max_key_id_bytes: 64,
            max_groups: 4_096,
            max_records: 1_048_576,
            max_external_hash_bytes: 256,
            max_total_external_hash_bytes: 32 * 1024 * 1024,
            max_prefix_token_ids: 16_777_216,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EngineIncarnation {
    pub engine_id: String,
    pub model_revision: String,
    pub image_digest: String,
    pub process_started_unix_ns: u64,
    #[serde(with = "serde_bytes")]
    pub attestation_sha256: Vec<u8>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DigestAlgorithm {
    HmacSha256V1,
    #[serde(other)]
    Unsupported,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DigestSpec {
    pub algorithm: DigestAlgorithm,
    #[serde(with = "serde_bytes")]
    pub key_id: Vec<u8>,
    pub digest_bytes: u16,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResetKind {
    FullEngine,
    DataParallelRank,
    CacheGroup,
    #[serde(other)]
    Unsupported,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResetScope {
    pub kind: ResetKind,
    pub data_parallel_rank: Option<u32>,
    pub group_idx: Option<u32>,
}

impl ResetScope {
    #[must_use]
    pub const fn full_engine() -> Self {
        Self {
            kind: ResetKind::FullEngine,
            data_parallel_rank: None,
            group_idx: None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttentionKind {
    FullAttention,
    MlaAttention,
    SinkFullAttention,
    SlidingWindow,
    SlidingWindowMla,
    Mamba,
    ChunkedLocalAttention,
    EncoderOnlyAttention,
    CrossAttention,
    #[serde(other)]
    Unsupported,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GroupDisposition {
    Indexed,
    Filtered,
    #[serde(other)]
    Unsupported,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GroupMetadata {
    pub data_parallel_rank: u32,
    pub group_idx: u32,
    pub attention_kind: AttentionKind,
    pub disposition: GroupDisposition,
    pub block_size: u32,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum SnapshotBlockHash {
    Bytes(ByteBuf),
    Signed(i64),
    Unsigned(u64),
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DigestRecord {
    /// Index into `groups`.
    pub group_slot: u32,
    /// Index of a preceding BFS record. `None` denotes a group root.
    pub parent_record: Option<u32>,
    /// Opaque vLLM identity retained only for parent/removal reconciliation.
    pub external_hash: SnapshotBlockHash,
    /// Keyed digest of this block's token slice. Parent linkage scopes equal
    /// block contents to their path; consumers hash each request block once.
    #[serde(with = "serde_bytes")]
    pub block_digest: Vec<u8>,
    pub block_token_ids: u32,
    pub prefix_token_ids: u64,
    /// Whether the engine still advertises this block as resident. Removed
    /// ancestors remain in snapshots while they have live descendants so a
    /// later reinsert can restore the complete prefix without losing parity
    /// with the live exact index.
    pub present: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotCapacity {
    pub groups: u64,
    pub records: u64,
    pub external_hash_bytes: u64,
    pub digest_bytes: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotBody {
    pub engine_incarnation: EngineIncarnation,
    /// Highest real vLLM event sequence represented by this state.
    pub watermark: u64,
    pub reset_scope: ResetScope,
    pub digest: DigestSpec,
    pub capacity: SnapshotCapacity,
    pub groups: Vec<GroupMetadata>,
    /// Breadth-first records; every parent must precede its children.
    pub records: Vec<DigestRecord>,
}

impl SnapshotBody {
    /// Recalculate the redundant capacity declaration producers put on wire.
    ///
    /// # Errors
    ///
    /// Returns [`SnapshotError::CapacityExceeded`] on an integer overflow.
    pub fn refresh_capacity(&mut self) -> Result<(), SnapshotError> {
        self.capacity = measured_capacity(&self.groups, &self.records)?;
        Ok(())
    }
}

#[derive(Clone, Copy, Debug)]
pub struct SnapshotExpectations<'a> {
    pub engine_incarnation: &'a EngineIncarnation,
    pub reset_scope: ResetScope,
    pub digest: &'a DigestSpec,
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum SnapshotError {
    #[error("KV snapshot frame exceeds configured byte limit")]
    FrameTooLarge,
    #[error("invalid KV snapshot MessagePack")]
    InvalidMessagePack,
    #[error("unsupported KV snapshot schema")]
    UnsupportedSchema,
    #[error("unsupported KV snapshot checksum")]
    UnsupportedChecksum,
    #[error("KV snapshot payload length is invalid")]
    InvalidPayloadLength,
    #[error("KV snapshot checksum is invalid")]
    InvalidChecksum,
    #[error("KV snapshot engine incarnation is invalid")]
    InvalidIncarnation,
    #[error("KV snapshot engine incarnation does not match")]
    IncarnationMismatch,
    #[error("unsupported KV snapshot digest contract")]
    UnsupportedDigest,
    #[error("KV snapshot digest contract does not match")]
    DigestMismatch,
    #[error("unsupported KV snapshot reset scope")]
    UnsupportedResetScope,
    #[error("KV snapshot reset scope does not match")]
    ResetScopeMismatch,
    #[error("KV snapshot exceeds configured group capacity")]
    TooManyGroups,
    #[error("KV snapshot exceeds configured record capacity")]
    TooManyRecords,
    #[error("KV snapshot exceeds configured storage capacity")]
    CapacityExceeded,
    #[error("KV snapshot capacity declaration does not match")]
    CapacityMismatch,
    #[error("KV snapshot contains invalid group metadata")]
    InvalidGroup,
    #[error("KV snapshot contains an invalid digest record")]
    InvalidRecord,
    #[error("KV snapshot operation was cancelled")]
    Cancelled,
    #[error("KV snapshot encoding failed")]
    EncodeFailed,
}

impl SnapshotError {
    #[must_use]
    pub const fn reason(&self) -> &'static str {
        match self {
            Self::FrameTooLarge => "frame_too_large",
            Self::InvalidMessagePack => "invalid_messagepack",
            Self::UnsupportedSchema => "unsupported_schema",
            Self::UnsupportedChecksum => "unsupported_checksum",
            Self::InvalidPayloadLength => "invalid_payload_length",
            Self::InvalidChecksum => "invalid_checksum",
            Self::InvalidIncarnation => "invalid_incarnation",
            Self::IncarnationMismatch => "incarnation_mismatch",
            Self::UnsupportedDigest => "unsupported_digest",
            Self::DigestMismatch => "digest_mismatch",
            Self::UnsupportedResetScope => "unsupported_reset_scope",
            Self::ResetScopeMismatch => "reset_scope_mismatch",
            Self::TooManyGroups => "too_many_groups",
            Self::TooManyRecords => "too_many_records",
            Self::CapacityExceeded => "capacity_exceeded",
            Self::CapacityMismatch => "capacity_mismatch",
            Self::InvalidGroup => "invalid_group",
            Self::InvalidRecord => "invalid_record",
            Self::Cancelled => "cancelled",
            Self::EncodeFailed => "encode_failed",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ChecksumAlgorithm {
    Sha256,
    #[serde(other)]
    Unsupported,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SnapshotEnvelope {
    schema_version: u16,
    checksum_algorithm: ChecksumAlgorithm,
    payload_bytes: u64,
    #[serde(with = "serde_bytes")]
    checksum: Vec<u8>,
    #[serde(with = "serde_bytes")]
    payload: Vec<u8>,
}

/// Encode and validate an authoritative snapshot frame.
///
/// # Errors
///
/// Returns [`SnapshotError`] when the body violates the contract, exceeds a
/// bound, cancellation is requested, or `MessagePack` encoding fails.
pub fn encode_snapshot(
    body: &SnapshotBody,
    limits: SnapshotLimits,
) -> Result<Vec<u8>, SnapshotError> {
    encode_snapshot_with_cancel(body, limits, || false)
}

/// Encode with a cancellation probe checked between bounded validation phases.
///
/// # Errors
///
/// Returns the same errors as [`encode_snapshot`], including
/// [`SnapshotError::Cancelled`].
pub fn encode_snapshot_with_cancel(
    body: &SnapshotBody,
    limits: SnapshotLimits,
    mut cancelled: impl FnMut() -> bool,
) -> Result<Vec<u8>, SnapshotError> {
    check_cancelled(&mut cancelled)?;
    validate_body(body, limits, &mut cancelled)?;
    check_cancelled(&mut cancelled)?;

    let payload = rmp_serde::to_vec_named(body).map_err(|_| SnapshotError::EncodeFailed)?;
    if payload.len() > limits.max_payload_bytes {
        return Err(SnapshotError::CapacityExceeded);
    }
    check_cancelled(&mut cancelled)?;

    let envelope = SnapshotEnvelope {
        schema_version: SNAPSHOT_SCHEMA_VERSION,
        checksum_algorithm: ChecksumAlgorithm::Sha256,
        payload_bytes: u64::try_from(payload.len()).map_err(|_| SnapshotError::CapacityExceeded)?,
        checksum: Sha256::digest(&payload).to_vec(),
        payload,
    };
    let frame = rmp_serde::to_vec_named(&envelope).map_err(|_| SnapshotError::EncodeFailed)?;
    if frame.len() > limits.max_frame_bytes {
        return Err(SnapshotError::FrameTooLarge);
    }
    Ok(frame)
}

/// Decode, integrity-check, bound, and validate one snapshot into private state.
///
/// Callers must drain a strictly-after-watermark live tail before atomically
/// publishing the returned body. This function never mutates a live index.
///
/// # Errors
///
/// Returns [`SnapshotError`] on malformed input, any contract mismatch, a
/// configured capacity breach, or cancellation.
pub fn decode_snapshot(
    frame: &[u8],
    limits: SnapshotLimits,
    expected: SnapshotExpectations<'_>,
) -> Result<SnapshotBody, SnapshotError> {
    decode_snapshot_with_cancel(frame, limits, expected, || false)
}

/// Decode with a cancellation probe checked before and during bounded work.
///
/// # Errors
///
/// Returns the same errors as [`decode_snapshot`], including
/// [`SnapshotError::Cancelled`].
pub fn decode_snapshot_with_cancel(
    frame: &[u8],
    limits: SnapshotLimits,
    expected: SnapshotExpectations<'_>,
    mut cancelled: impl FnMut() -> bool,
) -> Result<SnapshotBody, SnapshotError> {
    check_cancelled(&mut cancelled)?;
    if frame.len() > limits.max_frame_bytes {
        return Err(SnapshotError::FrameTooLarge);
    }
    let envelope: SnapshotEnvelope =
        rmp_serde::from_slice(frame).map_err(|_| SnapshotError::InvalidMessagePack)?;
    check_cancelled(&mut cancelled)?;
    if envelope.schema_version != SNAPSHOT_SCHEMA_VERSION {
        return Err(SnapshotError::UnsupportedSchema);
    }
    if envelope.checksum_algorithm != ChecksumAlgorithm::Sha256 {
        return Err(SnapshotError::UnsupportedChecksum);
    }
    if envelope.payload.len() > limits.max_payload_bytes
        || u64::try_from(envelope.payload.len()).ok() != Some(envelope.payload_bytes)
    {
        return Err(SnapshotError::InvalidPayloadLength);
    }
    if envelope.checksum.len() != SHA256_BYTES
        || Sha256::digest(&envelope.payload).as_slice() != envelope.checksum
    {
        return Err(SnapshotError::InvalidChecksum);
    }
    check_cancelled(&mut cancelled)?;

    let body: SnapshotBody =
        rmp_serde::from_slice(&envelope.payload).map_err(|_| SnapshotError::InvalidMessagePack)?;
    validate_header(&body, limits)?;
    if body.engine_incarnation != *expected.engine_incarnation {
        return Err(SnapshotError::IncarnationMismatch);
    }
    if body.digest != *expected.digest {
        return Err(SnapshotError::DigestMismatch);
    }
    if body.reset_scope != expected.reset_scope {
        return Err(SnapshotError::ResetScopeMismatch);
    }
    check_cancelled(&mut cancelled)?;
    validate_collections(&body, limits, &mut cancelled)?;
    Ok(body)
}

fn validate_body(
    body: &SnapshotBody,
    limits: SnapshotLimits,
    cancelled: &mut impl FnMut() -> bool,
) -> Result<(), SnapshotError> {
    validate_header(body, limits)?;
    validate_collections(body, limits, cancelled)
}

fn validate_header(body: &SnapshotBody, limits: SnapshotLimits) -> Result<(), SnapshotError> {
    validate_incarnation(&body.engine_incarnation, limits)?;
    if body.digest.algorithm != DigestAlgorithm::HmacSha256V1
        || usize::from(body.digest.digest_bytes) != SHA256_BYTES
        || body.digest.key_id.is_empty()
        || body.digest.key_id.len() > limits.max_key_id_bytes
    {
        return Err(SnapshotError::UnsupportedDigest);
    }
    validate_reset_scope(body.reset_scope)?;
    if body.groups.len() > limits.max_groups {
        return Err(SnapshotError::TooManyGroups);
    }
    if body.records.len() > limits.max_records {
        return Err(SnapshotError::TooManyRecords);
    }
    Ok(())
}

fn validate_incarnation(
    incarnation: &EngineIncarnation,
    limits: SnapshotLimits,
) -> Result<(), SnapshotError> {
    let valid_component =
        |value: &str| !value.is_empty() && value.len() <= limits.max_incarnation_component_bytes;
    if !valid_component(&incarnation.engine_id)
        || !valid_component(&incarnation.model_revision)
        || !valid_component(&incarnation.image_digest)
        || incarnation.process_started_unix_ns == 0
        || incarnation.attestation_sha256.len() != SHA256_BYTES
    {
        return Err(SnapshotError::InvalidIncarnation);
    }
    Ok(())
}

fn validate_reset_scope(scope: ResetScope) -> Result<(), SnapshotError> {
    let valid = match scope.kind {
        ResetKind::FullEngine => scope.data_parallel_rank.is_none() && scope.group_idx.is_none(),
        ResetKind::DataParallelRank => {
            scope.data_parallel_rank.is_some() && scope.group_idx.is_none()
        }
        ResetKind::CacheGroup => scope.data_parallel_rank.is_some() && scope.group_idx.is_some(),
        ResetKind::Unsupported => false,
    };
    if valid {
        Ok(())
    } else {
        Err(SnapshotError::UnsupportedResetScope)
    }
}

fn validate_collections(
    body: &SnapshotBody,
    limits: SnapshotLimits,
    cancelled: &mut impl FnMut() -> bool,
) -> Result<(), SnapshotError> {
    let mut group_keys = HashSet::with_capacity(body.groups.len());
    for group in &body.groups {
        check_cancelled(cancelled)?;
        if group.block_size == 0
            || group.attention_kind == AttentionKind::Unsupported
            || group.disposition == GroupDisposition::Unsupported
            || !group_keys.insert((group.data_parallel_rank, group.group_idx))
            || !group_in_scope(group, body.reset_scope)
            || !valid_disposition(group)
        {
            return Err(SnapshotError::InvalidGroup);
        }
    }

    let mut external_hashes = HashSet::with_capacity(body.records.len());
    let mut depths = Vec::with_capacity(body.records.len());
    let mut child_counts = vec![0usize; body.records.len()];
    let mut previous_depths = vec![0u32; body.groups.len()];
    for (index, record) in body.records.iter().enumerate() {
        check_cancelled(cancelled)?;
        let group_slot = usize::try_from(record.group_slot)
            .ok()
            .filter(|slot| *slot < body.groups.len())
            .ok_or(SnapshotError::InvalidRecord)?;
        let group = &body.groups[group_slot];
        if group.disposition != GroupDisposition::Indexed
            || record.block_digest.len() != usize::from(body.digest.digest_bytes)
            || record.block_token_ids == 0
            || record.block_token_ids > group.block_size
            || record.prefix_token_ids > limits.max_prefix_token_ids
            || external_hash_len(&record.external_hash) == 0
            || external_hash_len(&record.external_hash) > limits.max_external_hash_bytes
            || !external_hashes.insert((record.group_slot, &record.external_hash))
        {
            return Err(SnapshotError::InvalidRecord);
        }

        let (depth, expected_prefix) = if let Some(parent) = record.parent_record {
            let parent = usize::try_from(parent)
                .ok()
                .filter(|parent| *parent < index)
                .ok_or(SnapshotError::InvalidRecord)?;
            if body.records[parent].group_slot != record.group_slot {
                return Err(SnapshotError::InvalidRecord);
            }
            child_counts[parent] = child_counts[parent].saturating_add(1);
            (
                depths[parent] + 1,
                body.records[parent]
                    .prefix_token_ids
                    .checked_add(u64::from(record.block_token_ids))
                    .ok_or(SnapshotError::InvalidRecord)?,
            )
        } else {
            (0, u64::from(record.block_token_ids))
        };
        if depth < previous_depths[group_slot] || record.prefix_token_ids != expected_prefix {
            return Err(SnapshotError::InvalidRecord);
        }
        depths.push(depth);
        previous_depths[group_slot] = depth;
    }

    if body
        .records
        .iter()
        .zip(child_counts)
        .any(|(record, children)| !record.present && children == 0)
    {
        // The live exact index prunes absent leaves. Rejecting them keeps the
        // snapshot canonical and prevents dead records consuming capacity.
        return Err(SnapshotError::InvalidRecord);
    }

    let measured = measured_capacity(&body.groups, &body.records)?;
    if measured.external_hash_bytes
        > u64::try_from(limits.max_total_external_hash_bytes).unwrap_or(u64::MAX)
    {
        return Err(SnapshotError::CapacityExceeded);
    }
    if measured != body.capacity {
        return Err(SnapshotError::CapacityMismatch);
    }
    Ok(())
}

fn valid_disposition(group: &GroupMetadata) -> bool {
    let main = matches!(
        group.attention_kind,
        AttentionKind::FullAttention
            | AttentionKind::MlaAttention
            | AttentionKind::SinkFullAttention
    );
    matches!(
        (main, group.disposition),
        (true, GroupDisposition::Indexed) | (false, GroupDisposition::Filtered)
    )
}

fn group_in_scope(group: &GroupMetadata, scope: ResetScope) -> bool {
    match scope.kind {
        ResetKind::FullEngine => true,
        ResetKind::DataParallelRank => scope.data_parallel_rank == Some(group.data_parallel_rank),
        ResetKind::CacheGroup => {
            scope.data_parallel_rank == Some(group.data_parallel_rank)
                && scope.group_idx == Some(group.group_idx)
        }
        ResetKind::Unsupported => false,
    }
}

fn measured_capacity(
    groups: &[GroupMetadata],
    records: &[DigestRecord],
) -> Result<SnapshotCapacity, SnapshotError> {
    let external_hash_bytes = records.iter().try_fold(0u64, |total, record| {
        total.checked_add(u64::try_from(external_hash_len(&record.external_hash)).ok()?)
    });
    let digest_bytes = records.iter().try_fold(0u64, |total, record| {
        total.checked_add(u64::try_from(record.block_digest.len()).ok()?)
    });
    Ok(SnapshotCapacity {
        groups: u64::try_from(groups.len()).map_err(|_| SnapshotError::CapacityExceeded)?,
        records: u64::try_from(records.len()).map_err(|_| SnapshotError::CapacityExceeded)?,
        external_hash_bytes: external_hash_bytes.ok_or(SnapshotError::CapacityExceeded)?,
        digest_bytes: digest_bytes.ok_or(SnapshotError::CapacityExceeded)?,
    })
}

fn external_hash_len(hash: &SnapshotBlockHash) -> usize {
    match hash {
        SnapshotBlockHash::Bytes(bytes) => bytes.len(),
        SnapshotBlockHash::Signed(_) | SnapshotBlockHash::Unsigned(_) => size_of::<u64>(),
    }
}

fn check_cancelled(cancelled: &mut impl FnMut() -> bool) -> Result<(), SnapshotError> {
    if cancelled() {
        Err(SnapshotError::Cancelled)
    } else {
        Ok(())
    }
}
