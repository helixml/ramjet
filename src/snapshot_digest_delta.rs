//! Bounded, fail-closed application of authenticated snapshot tail payloads.
//!
//! The tail-wire layer authenticates the opaque payload before this adapter is
//! called. This module decodes that payload with [`crate::kv_wire`], selects
//! only events belonging to the snapshot's exact indexed main-attention group,
//! and mutates the private digest index. Any decode or index error clears the
//! index before returning, so a caller can never retain or publish a partially
//! applied generation.

use thiserror::Error;

use crate::{
    digest_index::{DigestIndexError, DigestKvIndex, SnapshotGroupKey},
    kv_wire::{
        BlockRemoved, BlockStored, DecodeError, KvEvent, KvEventBatch, KvWireLimits, decode_batch,
    },
};

#[derive(Clone, Copy, Debug)]
pub struct SnapshotDigestDeltaAdapter {
    group: SnapshotGroupKey,
    wire_limits: KvWireLimits,
}

impl SnapshotDigestDeltaAdapter {
    #[must_use]
    pub const fn new(group: SnapshotGroupKey, wire_limits: KvWireLimits) -> Self {
        Self { group, wire_limits }
    }

    /// Decode and apply one authenticated tail payload.
    ///
    /// This method has the callback shape required by
    /// [`crate::snapshot_actor::SnapshotBootstrapActor`]: pass
    /// `|index, payload| adapter.apply(index, payload)`. On any error the index
    /// is empty, and the actor will additionally fence and discard or revoke
    /// the generation.
    ///
    /// # Errors
    ///
    /// Returns a content-free decode or digest-index error. An error always
    /// clears `index`, including malformed input and a failure after earlier
    /// events in the same batch were applied.
    pub fn apply(
        &self,
        index: &mut DigestKvIndex,
        payload: &[u8],
    ) -> Result<DigestDeltaSummary, SnapshotDigestDeltaError> {
        let batch = match decode_batch(payload, self.wire_limits) {
            Ok(batch) => batch,
            Err(error) => {
                index.clear();
                return Err(SnapshotDigestDeltaError::Decode(error));
            }
        };
        self.apply_batch(index, &batch)
    }

    /// Apply one batch already decoded by the qualified vLLM transport.
    ///
    /// This avoids decoding live and replay payloads a second time while
    /// retaining the same conservative group filters and fail-closed index
    /// semantics as [`Self::apply`].
    ///
    /// # Errors
    ///
    /// Returns a content-free digest-index error and clears `index` if any
    /// event cannot be applied atomically.
    pub fn apply_batch(
        &self,
        index: &mut DigestKvIndex,
        batch: &KvEventBatch,
    ) -> Result<DigestDeltaSummary, SnapshotDigestDeltaError> {
        let rank = batch.data_parallel_rank.unwrap_or(0);
        let mut summary = DigestDeltaSummary::default();

        for event in &batch.events {
            let result = match event {
                KvEvent::BlockStored(stored) => {
                    if self.accept_store(rank, stored) {
                        match index.store(stored) {
                            Ok(blocks) => {
                                summary.stored_blocks =
                                    summary.stored_blocks.saturating_add(blocks);
                                Ok(())
                            }
                            // r34 may publish a short partial MLA block whose
                            // internal parent is absent from the public event
                            // stream. The raw exact inventory already treats
                            // every absent-parent store as non-authoritative and
                            // filters it. Preserve that no-overclaim contract in
                            // the compact index instead of fencing the otherwise
                            // complete generation.
                            Err(DigestIndexError::ParentNotFound) => {
                                summary.filtered_events = summary.filtered_events.saturating_add(1);
                                Ok(())
                            }
                            Err(error) => Err(error),
                        }
                    } else {
                        summary.filtered_events = summary.filtered_events.saturating_add(1);
                        Ok(())
                    }
                }
                KvEvent::BlockRemoved(removed) => {
                    if self.accept_remove(rank, removed) {
                        summary.removed_blocks =
                            summary.removed_blocks.saturating_add(index.remove(removed));
                    } else {
                        summary.filtered_events = summary.filtered_events.saturating_add(1);
                    }
                    Ok(())
                }
                KvEvent::AllBlocksCleared => {
                    if rank == self.group.data_parallel_rank {
                        index.clear();
                        summary.clear_events = summary.clear_events.saturating_add(1);
                    } else {
                        summary.filtered_events = summary.filtered_events.saturating_add(1);
                    }
                    Ok(())
                }
            };
            if let Err(error) = result {
                index.clear();
                return Err(SnapshotDigestDeltaError::Index(error));
            }
        }
        Ok(summary)
    }

    fn accept_store(&self, rank: u32, event: &BlockStored) -> bool {
        rank == self.group.data_parallel_rank
            && group_matches(event.group_idx, self.group.group_idx)
            && local_gpu(event.medium.as_deref(), event.locality.as_deref())
            && event
                .kv_cache_spec_kind
                .as_deref()
                .is_none_or(is_main_attention)
            && event.lora_name.is_none()
            && event.cache_namespace.is_none()
            && !event.has_extra_keys
    }

    fn accept_remove(&self, rank: u32, event: &BlockRemoved) -> bool {
        rank == self.group.data_parallel_rank
            && group_matches(event.group_idx, self.group.group_idx)
            && local_gpu(event.medium.as_deref(), event.locality.as_deref())
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DigestDeltaSummary {
    pub stored_blocks: usize,
    pub removed_blocks: usize,
    pub filtered_events: usize,
    pub clear_events: usize,
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum SnapshotDigestDeltaError {
    #[error("snapshot digest delta decoding failed")]
    Decode(#[source] DecodeError),
    #[error("snapshot digest delta index application failed")]
    Index(#[source] DigestIndexError),
}

impl SnapshotDigestDeltaError {
    /// Stable, content-free reason suitable for logs and metrics.
    #[must_use]
    pub const fn reason(&self) -> &'static str {
        match self {
            Self::Decode(error) => error.reason(),
            Self::Index(error) => error.reason(),
        }
    }
}

fn group_matches(event_group: Option<u32>, selected_group: u32) -> bool {
    event_group == Some(selected_group) || (selected_group == 0 && event_group.is_none())
}

fn local_gpu(medium: Option<&str>, locality: Option<&str>) -> bool {
    medium.is_none_or(|value| value == "GPU") && locality.is_none_or(|value| value == "LOCAL")
}

fn is_main_attention(value: &str) -> bool {
    matches!(
        value,
        "full_attention" | "mla_attention" | "sink_full_attention"
    )
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;
    use crate::digest_index::{DigestIndexLimits, DigestMatch};

    const SECRET: [u8; 32] = [0x42; 32];

    fn adapter() -> SnapshotDigestDeltaAdapter {
        SnapshotDigestDeltaAdapter::new(
            SnapshotGroupKey {
                data_parallel_rank: 0,
                group_idx: 0,
            },
            KvWireLimits::default(),
        )
    }

    fn index() -> DigestKvIndex {
        DigestKvIndex::from_secret(DigestIndexLimits::default(), &SECRET).unwrap()
    }

    fn payload(events: Vec<Value>) -> Vec<u8> {
        rmp_serde::to_vec(&(1.5_f64, events, 0_u32)).unwrap()
    }

    fn store(hash: u64, parent: Option<u64>, tokens: &[u32]) -> Value {
        json!({
            "type": "BlockStored",
            "block_hashes": [hash],
            "parent_block_hash": parent,
            "token_ids": tokens,
            "block_size": tokens.len(),
            "group_idx": 0,
            "kv_cache_spec_kind": "mla_attention",
            "medium": "GPU",
            "locality": "LOCAL"
        })
    }

    fn store_with_block_size(
        hash: u64,
        parent: Option<u64>,
        tokens: &[u32],
        block_size: usize,
    ) -> Value {
        let mut event = store(hash, parent, tokens);
        event["block_size"] = json!(block_size);
        event
    }

    fn remove(hash: u64) -> Value {
        json!({
            "type": "BlockRemoved",
            "block_hashes": [hash],
            "group_idx": 0,
            "medium": "GPU",
            "locality": "LOCAL"
        })
    }

    #[test]
    fn vllm_shaped_fixture_stores_and_matches() {
        let mut index = index();
        let summary = adapter()
            .apply(&mut index, &payload(vec![store(11, None, &[10, 20])]))
            .unwrap();
        assert_eq!(summary.stored_blocks, 1);
        assert_eq!(
            index.find_longest(&[10, 20]).unwrap(),
            DigestMatch {
                blocks: 1,
                token_ids: 2
            }
        );
    }

    #[test]
    fn store_remove_and_clear_apply_in_order() {
        let mut index = index();
        let summary = adapter()
            .apply(
                &mut index,
                &payload(vec![
                    store(11, None, &[1, 2]),
                    store(12, Some(11), &[3, 4]),
                    remove(12),
                ]),
            )
            .unwrap();
        assert_eq!(summary.stored_blocks, 2);
        assert_eq!(summary.removed_blocks, 1);
        assert_eq!(index.find_longest(&[1, 2, 3, 4]).unwrap().token_ids, 2);

        let cleared = adapter()
            .apply(
                &mut index,
                &payload(vec![json!({"type": "AllBlocksCleared"})]),
            )
            .unwrap();
        assert_eq!(cleared.clear_events, 1);
        assert_eq!(index.stats().nodes, 0);
    }

    #[test]
    fn unsupported_and_mixed_groups_are_conservatively_filtered() {
        let mut index = index();
        let mut non_main = store(21, None, &[9, 9]);
        non_main["kv_cache_spec_kind"] = json!("sliding_window_mla");
        let mut other_group = store(22, None, &[8, 8]);
        other_group["group_idx"] = json!(1);
        let mut namespaced = store(23, None, &[7, 7]);
        namespaced["cache_salt"] = json!("tenant");
        let summary = adapter()
            .apply(
                &mut index,
                &payload(vec![
                    non_main,
                    other_group,
                    namespaced,
                    store(24, None, &[6, 6]),
                ]),
            )
            .unwrap();
        assert_eq!(summary.filtered_events, 3);
        assert_eq!(summary.stored_blocks, 1);
        assert_eq!(index.find_longest(&[9, 9]).unwrap().token_ids, 0);
        assert_eq!(index.find_longest(&[8, 8]).unwrap().token_ids, 0);
        assert_eq!(index.find_longest(&[6, 6]).unwrap().token_ids, 2);
    }

    #[test]
    fn r34_partial_mla_orphan_is_filtered_without_losing_the_generation() {
        let mut index = index();
        let canonical_tokens = (0..256).collect::<Vec<_>>();
        let summary = adapter()
            .apply(
                &mut index,
                &payload(vec![
                    store_with_block_size(11, None, &canonical_tokens, 256),
                    store_with_block_size(12, Some(999), &[300, 301, 302, 303], 4),
                ]),
            )
            .unwrap();

        assert_eq!(summary.stored_blocks, 1);
        assert_eq!(summary.filtered_events, 1);
        assert_eq!(index.stats().nodes, 1);
        assert_eq!(
            index.find_longest(&canonical_tokens).unwrap().token_ids,
            canonical_tokens.len()
        );
        assert_eq!(
            index.find_longest(&[300, 301, 302, 303]).unwrap().token_ids,
            0
        );
    }

    #[test]
    fn malformed_and_wire_capacity_failures_clear_existing_claims() {
        let mut index = index();
        adapter()
            .apply(&mut index, &payload(vec![store(11, None, &[1, 2])]))
            .unwrap();
        let error = adapter().apply(&mut index, b"not-messagepack").unwrap_err();
        assert!(matches!(error, SnapshotDigestDeltaError::Decode(_)));
        assert_eq!(index.stats().nodes, 0);

        adapter()
            .apply(&mut index, &payload(vec![store(12, None, &[3, 4])]))
            .unwrap();
        let bounded = SnapshotDigestDeltaAdapter::new(
            SnapshotGroupKey {
                data_parallel_rank: 0,
                group_idx: 0,
            },
            KvWireLimits {
                max_events: 0,
                ..KvWireLimits::default()
            },
        );
        assert!(matches!(
            bounded.apply(&mut index, &payload(vec![store(13, None, &[5, 6])])),
            Err(SnapshotDigestDeltaError::Decode(DecodeError::TooManyEvents))
        ));
        assert_eq!(index.stats().nodes, 0);
    }

    #[test]
    fn index_capacity_failure_after_partial_batch_never_overclaims() {
        let mut index = DigestKvIndex::from_secret(
            DigestIndexLimits {
                max_nodes: 1,
                ..DigestIndexLimits::default()
            },
            &SECRET,
        )
        .unwrap();
        let error = adapter()
            .apply(
                &mut index,
                &payload(vec![store(31, None, &[1, 2]), store(32, Some(31), &[3, 4])]),
            )
            .unwrap_err();
        assert!(matches!(error, SnapshotDigestDeltaError::Index(_)));
        assert_eq!(index.stats().nodes, 0);
        assert_eq!(index.find_longest(&[1, 2, 3, 4]).unwrap().token_ids, 0);
    }

    #[test]
    fn wrong_rank_clear_cannot_erase_selected_generation() {
        let mut index = index();
        adapter()
            .apply(&mut index, &payload(vec![store(11, None, &[1, 2])]))
            .unwrap();
        let other_rank =
            rmp_serde::to_vec(&(2.0_f64, vec![json!({"type": "AllBlocksCleared"})], 1_u32))
                .unwrap();
        let summary = adapter().apply(&mut index, &other_rank).unwrap();
        assert_eq!(summary.filtered_events, 1);
        assert_eq!(index.find_longest(&[1, 2]).unwrap().token_ids, 2);
    }

    #[test]
    fn errors_and_debug_are_content_free() {
        let error = SnapshotDigestDeltaError::Decode(DecodeError::InvalidMessagePack);
        assert_eq!(error.to_string(), "snapshot digest delta decoding failed");
        assert_eq!(error.reason(), "invalid_messagepack");
        assert!(!format!("{error:?}").contains("token"));
    }
}
