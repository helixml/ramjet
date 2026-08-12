//! Bounded decoder for vLLM's native KV-event `MessagePack` payload.
//!
//! ZMQ framing, replay, and cache indexing deliberately live elsewhere. This
//! module accepts only the payload frame and never formats token IDs or block
//! hashes, keeping malformed-input errors safe to expose as controlled logs.

use serde::Deserialize;
use serde_bytes::ByteBuf;
use thiserror::Error;

#[derive(Clone, Copy, Debug)]
pub struct KvWireLimits {
    pub max_payload_bytes: usize,
    pub max_events: usize,
    pub max_block_hashes: usize,
    pub max_token_ids: usize,
    pub max_block_size: usize,
    pub max_hash_bytes: usize,
}

impl Default for KvWireLimits {
    fn default() -> Self {
        Self {
            max_payload_bytes: 16 * 1024 * 1024,
            max_events: 4_096,
            max_block_hashes: 1_048_576,
            max_token_ids: 4_194_304,
            max_block_size: 1_048_576,
            max_hash_bytes: 256,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct KvEventBatch {
    pub timestamp: f64,
    pub events: Vec<KvEvent>,
    pub data_parallel_rank: Option<u32>,
}

impl KvEventBatch {
    #[must_use]
    pub fn clears_all(&self) -> bool {
        self.events
            .iter()
            .any(|event| matches!(event, KvEvent::AllBlocksCleared))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum KvEvent {
    BlockStored(BlockStored),
    BlockRemoved(BlockRemoved),
    AllBlocksCleared,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BlockStored {
    pub block_hashes: Vec<ExternalBlockHash>,
    pub parent_block_hash: Option<ExternalBlockHash>,
    pub token_ids: Vec<u32>,
    pub block_size: usize,
    pub group_idx: Option<u32>,
    pub kv_cache_spec_kind: Option<String>,
    pub kv_cache_spec_sliding_window: Option<u64>,
    pub medium: Option<String>,
    pub locality: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BlockRemoved {
    pub block_hashes: Vec<ExternalBlockHash>,
    pub group_idx: Option<u32>,
    pub medium: Option<String>,
    pub locality: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ExternalBlockHash {
    Bytes(ByteBuf),
    Signed(i64),
    Unsigned(u64),
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum DecodeError {
    #[error("KV-event payload exceeds configured byte limit")]
    PayloadTooLarge,
    #[error("invalid KV-event MessagePack payload")]
    InvalidMessagePack,
    #[error("KV-event batch timestamp is not finite")]
    InvalidTimestamp,
    #[error("KV-event data-parallel rank is out of range")]
    InvalidDataParallelRank,
    #[error("KV-event batch exceeds configured event limit")]
    TooManyEvents,
    #[error("KV-event batch exceeds configured block-hash limit")]
    TooManyBlockHashes,
    #[error("KV-event batch exceeds configured token-ID limit")]
    TooManyTokenIds,
    #[error("KV-event contains an invalid block hash")]
    InvalidBlockHash,
    #[error("KV-event contains an invalid block size")]
    InvalidBlockSize,
    #[error("KV-event block and token counts are inconsistent")]
    InconsistentBlockShape,
    #[error("KV-event contains an out-of-range cache-group index")]
    InvalidGroupIndex,
}

#[derive(Deserialize)]
struct RawBatch(f64, Vec<RawEvent>, #[serde(default)] Option<i64>);

#[derive(Deserialize)]
#[serde(tag = "type")]
enum RawEvent {
    BlockStored {
        block_hashes: Vec<RawBlockHash>,
        parent_block_hash: Option<RawBlockHash>,
        token_ids: Vec<u32>,
        block_size: usize,
        #[serde(default)]
        group_idx: Option<i64>,
        #[serde(default)]
        kv_cache_spec_kind: Option<String>,
        #[serde(default)]
        kv_cache_spec_sliding_window: Option<u64>,
        medium: Option<String>,
        #[serde(default)]
        locality: Option<String>,
    },
    BlockRemoved {
        block_hashes: Vec<RawBlockHash>,
        #[serde(default)]
        group_idx: Option<i64>,
        medium: Option<String>,
        #[serde(default)]
        locality: Option<String>,
    },
    AllBlocksCleared,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum RawBlockHash {
    Bytes(ByteBuf),
    Signed(i64),
    Unsigned(u64),
}

/// Decode and validate one vLLM `KVEventBatch` payload frame.
///
/// Errors intentionally identify only the violated invariant. They never
/// contain raw token IDs, hashes, or `MessagePack` bytes.
///
/// # Errors
///
/// Returns [`DecodeError`] when the payload is malformed, exceeds a configured
/// bound, or violates an event-shape invariant required by the exact index.
pub fn decode_batch(payload: &[u8], limits: KvWireLimits) -> Result<KvEventBatch, DecodeError> {
    if payload.len() > limits.max_payload_bytes {
        return Err(DecodeError::PayloadTooLarge);
    }
    let RawBatch(timestamp, raw_events, raw_rank) =
        rmp_serde::from_slice(payload).map_err(|_| DecodeError::InvalidMessagePack)?;
    if !timestamp.is_finite() {
        return Err(DecodeError::InvalidTimestamp);
    }
    if raw_events.len() > limits.max_events {
        return Err(DecodeError::TooManyEvents);
    }
    let data_parallel_rank = optional_u32(raw_rank, DecodeError::InvalidDataParallelRank)?;

    let mut hash_count = 0usize;
    let mut token_count = 0usize;
    let mut events = Vec::with_capacity(raw_events.len());
    for raw in raw_events {
        let event = match raw {
            RawEvent::BlockStored {
                block_hashes,
                parent_block_hash,
                token_ids,
                block_size,
                group_idx,
                kv_cache_spec_kind,
                kv_cache_spec_sliding_window,
                medium,
                locality,
            } => {
                validate_block_size(block_size, limits.max_block_size)?;
                if block_hashes.is_empty() || token_ids.is_empty() {
                    return Err(DecodeError::InconsistentBlockShape);
                }
                let expected_blocks = token_ids.len().div_ceil(block_size);
                if expected_blocks != block_hashes.len() {
                    return Err(DecodeError::InconsistentBlockShape);
                }
                hash_count = checked_total(
                    hash_count,
                    block_hashes.len(),
                    limits.max_block_hashes,
                    DecodeError::TooManyBlockHashes,
                )?;
                token_count = checked_total(
                    token_count,
                    token_ids.len(),
                    limits.max_token_ids,
                    DecodeError::TooManyTokenIds,
                )?;
                KvEvent::BlockStored(BlockStored {
                    block_hashes: convert_hashes(block_hashes, limits.max_hash_bytes)?,
                    parent_block_hash: parent_block_hash
                        .map(|hash| convert_hash(hash, limits.max_hash_bytes))
                        .transpose()?,
                    token_ids,
                    block_size,
                    group_idx: optional_u32(group_idx, DecodeError::InvalidGroupIndex)?,
                    kv_cache_spec_kind,
                    kv_cache_spec_sliding_window,
                    medium,
                    locality,
                })
            }
            RawEvent::BlockRemoved {
                block_hashes,
                group_idx,
                medium,
                locality,
            } => {
                if block_hashes.is_empty() {
                    return Err(DecodeError::InvalidBlockHash);
                }
                hash_count = checked_total(
                    hash_count,
                    block_hashes.len(),
                    limits.max_block_hashes,
                    DecodeError::TooManyBlockHashes,
                )?;
                KvEvent::BlockRemoved(BlockRemoved {
                    block_hashes: convert_hashes(block_hashes, limits.max_hash_bytes)?,
                    group_idx: optional_u32(group_idx, DecodeError::InvalidGroupIndex)?,
                    medium,
                    locality,
                })
            }
            RawEvent::AllBlocksCleared => KvEvent::AllBlocksCleared,
        };
        events.push(event);
    }

    Ok(KvEventBatch {
        timestamp,
        events,
        data_parallel_rank,
    })
}

fn validate_block_size(block_size: usize, max: usize) -> Result<(), DecodeError> {
    if block_size == 0 || block_size > max {
        return Err(DecodeError::InvalidBlockSize);
    }
    Ok(())
}

fn optional_u32(value: Option<i64>, error: DecodeError) -> Result<Option<u32>, DecodeError> {
    value
        .map(|number| u32::try_from(number).map_err(|_| error))
        .transpose()
}

fn checked_total(
    total: usize,
    additional: usize,
    maximum: usize,
    error: DecodeError,
) -> Result<usize, DecodeError> {
    total
        .checked_add(additional)
        .filter(|sum| *sum <= maximum)
        .ok_or(error)
}

fn convert_hashes(
    hashes: Vec<RawBlockHash>,
    max_bytes: usize,
) -> Result<Vec<ExternalBlockHash>, DecodeError> {
    hashes
        .into_iter()
        .map(|hash| convert_hash(hash, max_bytes))
        .collect()
}

fn convert_hash(hash: RawBlockHash, max_bytes: usize) -> Result<ExternalBlockHash, DecodeError> {
    match hash {
        RawBlockHash::Bytes(bytes) if bytes.is_empty() || bytes.len() > max_bytes => {
            Err(DecodeError::InvalidBlockHash)
        }
        RawBlockHash::Bytes(bytes) => Ok(ExternalBlockHash::Bytes(bytes)),
        RawBlockHash::Signed(value) => Ok(ExternalBlockHash::Signed(value)),
        RawBlockHash::Unsigned(value) => Ok(ExternalBlockHash::Unsigned(value)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Encoded by msgspec from the exact vLLM r34 classes installed on node06.
    // All hashes/token IDs are synthetic test values.
    const VLLM_R34_FIXTURE: &[u8] = &[
        0x93, 0xcb, 0x3f, 0xf8, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x93, 0x8a, 0xa4, 0x74, 0x79,
        0x70, 0x65, 0xab, 0x42, 0x6c, 0x6f, 0x63, 0x6b, 0x53, 0x74, 0x6f, 0x72, 0x65, 0x64, 0xac,
        0x62, 0x6c, 0x6f, 0x63, 0x6b, 0x5f, 0x68, 0x61, 0x73, 0x68, 0x65, 0x73, 0x92, 0xc4, 0x02,
        0x68, 0x31, 0x07, 0xb1, 0x70, 0x61, 0x72, 0x65, 0x6e, 0x74, 0x5f, 0x62, 0x6c, 0x6f, 0x63,
        0x6b, 0x5f, 0x68, 0x61, 0x73, 0x68, 0xc0, 0xa9, 0x74, 0x6f, 0x6b, 0x65, 0x6e, 0x5f, 0x69,
        0x64, 0x73, 0x94, 0x0a, 0x0b, 0x0c, 0x0d, 0xaa, 0x62, 0x6c, 0x6f, 0x63, 0x6b, 0x5f, 0x73,
        0x69, 0x7a, 0x65, 0x02, 0xa7, 0x6c, 0x6f, 0x72, 0x61, 0x5f, 0x69, 0x64, 0xc0, 0xa6, 0x6d,
        0x65, 0x64, 0x69, 0x75, 0x6d, 0xa3, 0x47, 0x50, 0x55, 0xa9, 0x6c, 0x6f, 0x72, 0x61, 0x5f,
        0x6e, 0x61, 0x6d, 0x65, 0xc0, 0xa9, 0x67, 0x72, 0x6f, 0x75, 0x70, 0x5f, 0x69, 0x64, 0x78,
        0x00, 0xb2, 0x6b, 0x76, 0x5f, 0x63, 0x61, 0x63, 0x68, 0x65, 0x5f, 0x73, 0x70, 0x65, 0x63,
        0x5f, 0x6b, 0x69, 0x6e, 0x64, 0xa4, 0x66, 0x75, 0x6c, 0x6c, 0x84, 0xa4, 0x74, 0x79, 0x70,
        0x65, 0xac, 0x42, 0x6c, 0x6f, 0x63, 0x6b, 0x52, 0x65, 0x6d, 0x6f, 0x76, 0x65, 0x64, 0xac,
        0x62, 0x6c, 0x6f, 0x63, 0x6b, 0x5f, 0x68, 0x61, 0x73, 0x68, 0x65, 0x73, 0x91, 0xc4, 0x02,
        0x68, 0x31, 0xa6, 0x6d, 0x65, 0x64, 0x69, 0x75, 0x6d, 0xa3, 0x47, 0x50, 0x55, 0xa9, 0x67,
        0x72, 0x6f, 0x75, 0x70, 0x5f, 0x69, 0x64, 0x78, 0x00, 0x81, 0xa4, 0x74, 0x79, 0x70, 0x65,
        0xb0, 0x41, 0x6c, 0x6c, 0x42, 0x6c, 0x6f, 0x63, 0x6b, 0x73, 0x43, 0x6c, 0x65, 0x61, 0x72,
        0x65, 0x64, 0x03,
    ];

    #[test]
    fn decodes_exact_node06_vllm_r34_fixture() {
        let batch = decode_batch(VLLM_R34_FIXTURE, KvWireLimits::default()).unwrap();
        assert!((batch.timestamp - 1.5).abs() < f64::EPSILON);
        assert_eq!(batch.data_parallel_rank, Some(3));
        assert_eq!(batch.events.len(), 3);
        assert!(batch.clears_all());

        let KvEvent::BlockStored(stored) = &batch.events[0] else {
            panic!("expected stored event");
        };
        assert_eq!(stored.token_ids.len(), 4);
        assert_eq!(stored.block_hashes.len(), 2);
        assert_eq!(stored.block_size, 2);
        assert_eq!(stored.group_idx, Some(0));
        assert_eq!(stored.kv_cache_spec_kind.as_deref(), Some("full"));
        assert!(matches!(
            stored.block_hashes[0],
            ExternalBlockHash::Bytes(_)
        ));
        assert!(matches!(
            stored.block_hashes[1],
            ExternalBlockHash::Signed(7)
        ));
    }

    #[test]
    fn rejects_oversized_payload_before_decode() {
        let limits = KvWireLimits {
            max_payload_bytes: VLLM_R34_FIXTURE.len() - 1,
            ..KvWireLimits::default()
        };
        assert_eq!(
            decode_batch(VLLM_R34_FIXTURE, limits),
            Err(DecodeError::PayloadTooLarge)
        );
    }

    #[test]
    fn rejects_unknown_event_without_echoing_payload() {
        let fixture =
            rmp_serde::to_vec(&(1.0, vec![serde_json::json!({"type": "FutureEvent"})])).unwrap();
        let error = decode_batch(&fixture, KvWireLimits::default()).unwrap_err();
        assert_eq!(error, DecodeError::InvalidMessagePack);
        assert_eq!(error.to_string(), "invalid KV-event MessagePack payload");
    }

    #[test]
    fn rejects_inconsistent_block_shape() {
        let fixture = rmp_serde::to_vec(&(
            1.0,
            vec![serde_json::json!({
                "type": "BlockStored",
                "block_hashes": [1],
                "parent_block_hash": null,
                "token_ids": [1, 2, 3],
                "block_size": 2,
                "medium": "GPU"
            })],
        ))
        .unwrap();
        assert_eq!(
            decode_batch(&fixture, KvWireLimits::default()),
            Err(DecodeError::InconsistentBlockShape)
        );
    }

    #[test]
    fn enforces_aggregate_batch_limits() {
        let limits = KvWireLimits {
            max_token_ids: 3,
            ..KvWireLimits::default()
        };
        assert_eq!(
            decode_batch(VLLM_R34_FIXTURE, limits),
            Err(DecodeError::TooManyTokenIds)
        );
    }
}
