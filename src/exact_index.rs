//! Bounded per-engine exact KV block inventory.
//!
//! The index treats engine-provided block hashes as opaque reverse-lookup keys
//! and uses exact token slices for forward prefix lookup. It intentionally has
//! no transport or routing dependency: a consumer can fence and discard this
//! state without affecting the approximate router.

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use parking_lot::RwLock;
use thiserror::Error;

use crate::kv_wire::{BlockRemoved, BlockStored, ExternalBlockHash, KvEvent};
use crate::{
    kv_fence::{IngestAction, KvEventFence, ReplayAction},
    kv_wire::KvEventBatch,
};

const ROOT_NODE: usize = 0;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExactIndexLimits {
    pub max_nodes: usize,
    pub max_token_ids: usize,
    pub max_lookup_steps: usize,
}

impl Default for ExactIndexLimits {
    fn default() -> Self {
        Self {
            max_nodes: 131_072,
            max_token_ids: 16_777_216,
            max_lookup_steps: 131_072,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ExactMatch {
    pub blocks: usize,
    pub token_ids: usize,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ExactIndexStats {
    pub nodes: usize,
    pub token_ids: usize,
    pub external_hashes: usize,
}

/// Content-free cache-group coverage learned from vLLM KV events.
///
/// Untagged legacy event streams deliberately remain placement-compatible.
/// Once an engine publishes group metadata, however, serving placement is
/// safe only after at least one reusable attention group is known and every
/// observed group has a recognized semantic kind. Shadow lookup remains
/// available while that gate is incomplete.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ExactCacheGroupCoverage {
    pub main_groups: usize,
    pub non_main_groups: usize,
    pub unknown_groups: usize,
    pub unlearned_groups: usize,
}

impl ExactCacheGroupCoverage {
    #[must_use]
    pub const fn placement_ready(self) -> bool {
        let tagged_groups = self
            .main_groups
            .saturating_add(self.non_main_groups)
            .saturating_add(self.unknown_groups)
            .saturating_add(self.unlearned_groups);
        tagged_groups == 0
            || (self.main_groups > 0 && self.unknown_groups == 0 && self.unlearned_groups == 0)
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct BatchApplySummary {
    pub stored_blocks: usize,
    pub removed_blocks: usize,
    pub filtered_events: usize,
    pub clear_events: usize,
    filtered_reasons: FilterCounts,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LiveBatchOutcome {
    Applied(BatchApplySummary),
    ObserveOnly,
    Duplicate,
    Replay { from: u64, through: u64 },
    Fenced,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReplayBatchOutcome {
    Applied(BatchApplySummary),
    ObserveOnly,
    Invalid,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FilterReason {
    NonLocal,
    UnsupportedMedium,
    Namespaced,
    NonMainAttention,
    UnknownAttentionKind,
    UnlearnedAttentionGroup,
    UnsupportedPartialBlock,
    OrphanedParent,
}

impl FilterReason {
    pub const ALL: [Self; 8] = [
        Self::NonLocal,
        Self::UnsupportedMedium,
        Self::Namespaced,
        Self::NonMainAttention,
        Self::UnknownAttentionKind,
        Self::UnlearnedAttentionGroup,
        Self::UnsupportedPartialBlock,
        Self::OrphanedParent,
    ];

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::NonLocal => "non_local",
            Self::UnsupportedMedium => "unsupported_medium",
            Self::Namespaced => "namespaced",
            Self::NonMainAttention => "non_main_attention",
            Self::UnknownAttentionKind => "unknown_attention_kind",
            Self::UnlearnedAttentionGroup => "unlearned_attention_group",
            Self::UnsupportedPartialBlock => "unsupported_partial_block",
            Self::OrphanedParent => "orphaned_parent",
        }
    }

    const fn index(self) -> usize {
        match self {
            Self::NonLocal => 0,
            Self::UnsupportedMedium => 1,
            Self::Namespaced => 2,
            Self::NonMainAttention => 3,
            Self::UnknownAttentionKind => 4,
            Self::UnlearnedAttentionGroup => 5,
            Self::UnsupportedPartialBlock => 6,
            Self::OrphanedParent => 7,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct FilterCounts([usize; FilterReason::ALL.len()]);

impl FilterCounts {
    fn increment(&mut self, reason: FilterReason) {
        self.0[reason.index()] = self.0[reason.index()].saturating_add(1);
    }

    fn merge(&mut self, other: Self) {
        for (total, additional) in self.0.iter_mut().zip(other.0) {
            *total = total.saturating_add(additional);
        }
    }

    fn iter(self) -> impl Iterator<Item = (FilterReason, usize)> {
        FilterReason::ALL.into_iter().zip(self.0)
    }
}

impl BatchApplySummary {
    pub fn filtered_by_reason(self) -> impl Iterator<Item = (FilterReason, usize)> {
        self.filtered_reasons.iter()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ApplyOutcome {
    Stored { blocks: usize },
    Removed { blocks: usize },
    Cleared,
    Filtered(FilterReason),
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum ExactIndexError {
    #[error("exact KV store has inconsistent block and token counts")]
    InconsistentBlockShape,
    #[error("exact KV store references an unknown parent")]
    ParentNotFound,
    #[error("exact KV store contains a duplicate or self-referencing hash")]
    DuplicateHash,
    #[error("exact KV store conflicts with an existing token path")]
    ConflictingPath,
    #[error("exact KV cache group changed semantic kind within one generation")]
    ConflictingGroupKind,
    #[error("exact KV index capacity would be exceeded")]
    CapacityExceeded,
    #[error("exact KV lookup work budget was exceeded")]
    LookupBudgetExceeded,
}

impl ExactIndexError {
    #[must_use]
    pub const fn reason(&self) -> &'static str {
        match self {
            Self::InconsistentBlockShape => "index_inconsistent_block_shape",
            Self::ParentNotFound => "index_parent_not_found",
            Self::DuplicateHash => "index_duplicate_hash",
            Self::ConflictingPath => "index_conflicting_path",
            Self::ConflictingGroupKind => "index_conflicting_group_kind",
            Self::CapacityExceeded => "index_capacity_exceeded",
            Self::LookupBudgetExceeded => "index_lookup_budget_exceeded",
        }
    }
}

#[derive(Debug)]
struct Node {
    parent: Option<usize>,
    block: Arc<[u32]>,
    children: HashMap<Arc<[u32]>, usize>,
    child_lengths: Vec<usize>,
    external_hash: Option<ExternalBlockHash>,
    present: bool,
}

impl Node {
    fn root() -> Self {
        Self {
            parent: None,
            block: Arc::from([]),
            children: HashMap::new(),
            child_lengths: Vec::new(),
            external_hash: None,
            present: true,
        }
    }
}

#[derive(Debug)]
pub struct ExactKvIndex {
    limits: ExactIndexLimits,
    nodes: Vec<Option<Node>>,
    free_nodes: Vec<usize>,
    by_external_hash: HashMap<ExternalBlockHash, usize>,
    live_nodes: usize,
    resident_token_ids: usize,
}

impl ExactKvIndex {
    #[must_use]
    pub fn new(limits: ExactIndexLimits) -> Self {
        Self {
            limits,
            nodes: vec![Some(Node::root())],
            free_nodes: Vec::new(),
            by_external_hash: HashMap::new(),
            live_nodes: 0,
            resident_token_ids: 0,
        }
    }

    pub fn clear(&mut self) {
        self.nodes.clear();
        self.nodes.push(Some(Node::root()));
        self.free_nodes.clear();
        self.by_external_hash.clear();
        self.live_nodes = 0;
        self.resident_token_ids = 0;
    }

    #[must_use]
    pub fn stats(&self) -> ExactIndexStats {
        ExactIndexStats {
            nodes: self.live_nodes,
            token_ids: self.resident_token_ids,
            external_hashes: self.by_external_hash.len(),
        }
    }

    /// Add one already-decoded and already-filtered store event atomically.
    ///
    /// # Errors
    ///
    /// Returns [`ExactIndexError`] when the parent/path is inconsistent or the
    /// bounded resident capacity would be exceeded. No mutation occurs on an
    /// error.
    pub fn store(&mut self, event: &BlockStored) -> Result<usize, ExactIndexError> {
        if event.block_size == 0
            || event.token_ids.is_empty()
            || event.block_hashes.is_empty()
            || event.token_ids.len().div_ceil(event.block_size) != event.block_hashes.len()
        {
            return Err(ExactIndexError::InconsistentBlockShape);
        }
        let parent_id = match event.parent_block_hash.as_ref() {
            Some(parent) => self
                .by_external_hash
                .get(parent)
                .copied()
                .ok_or(ExactIndexError::ParentNotFound)?,
            None => ROOT_NODE,
        };

        let mut seen = HashSet::with_capacity(event.block_hashes.len() + 1);
        if let Some(parent) = event.parent_block_hash.as_ref() {
            seen.insert(parent);
        }
        if event.block_hashes.iter().any(|hash| !seen.insert(hash)) {
            return Err(ExactIndexError::DuplicateHash);
        }

        let chunks = event.token_ids.chunks(event.block_size).collect::<Vec<_>>();
        let mut cursor = parent_id;
        let mut first_new = chunks.len();
        let mut existing_path = Vec::with_capacity(chunks.len());
        for (position, (chunk, external_hash)) in chunks.iter().zip(&event.block_hashes).enumerate()
        {
            let Some(child_id) = self.node(cursor).children.get(*chunk).copied() else {
                first_new = position;
                break;
            };
            let child = self.node(child_id);
            if child.external_hash.as_ref() != Some(external_hash) {
                return Err(ExactIndexError::ConflictingPath);
            }
            if self
                .by_external_hash
                .get(external_hash)
                .is_some_and(|existing| *existing != child_id)
            {
                return Err(ExactIndexError::ConflictingPath);
            }
            cursor = child_id;
            existing_path.push(child_id);
        }

        for (position, hash) in event.block_hashes.iter().enumerate() {
            if let Some(existing) = self.by_external_hash.get(hash) {
                let expected = existing_path.get(position).copied();
                if expected != Some(*existing) {
                    return Err(ExactIndexError::ConflictingPath);
                }
            }
        }

        let new_nodes = chunks.len().saturating_sub(first_new);
        let new_token_ids = chunks[first_new..]
            .iter()
            .try_fold(0usize, |total, chunk| total.checked_add(chunk.len()))
            .ok_or(ExactIndexError::CapacityExceeded)?;
        if self.live_nodes.saturating_add(new_nodes) > self.limits.max_nodes
            || self.resident_token_ids.saturating_add(new_token_ids) > self.limits.max_token_ids
        {
            return Err(ExactIndexError::CapacityExceeded);
        }

        cursor = parent_id;
        for (chunk, external_hash) in chunks.into_iter().zip(&event.block_hashes) {
            let child_id = if let Some(existing) = self.node(cursor).children.get(chunk).copied() {
                existing
            } else {
                self.insert_child(cursor, chunk, external_hash.clone())
            };
            let child = self.node_mut(child_id);
            child.present = true;
            child.external_hash = Some(external_hash.clone());
            self.by_external_hash
                .insert(external_hash.clone(), child_id);
            cursor = child_id;
        }
        Ok(event.block_hashes.len())
    }

    pub fn remove(&mut self, event: &BlockRemoved) -> usize {
        let mut removed = 0usize;
        for external_hash in &event.block_hashes {
            let Some(node_id) = self.by_external_hash.remove(external_hash) else {
                continue;
            };
            let node = self.node_mut(node_id);
            if !node.present || node.external_hash.as_ref() != Some(external_hash) {
                continue;
            }
            node.present = false;
            removed += 1;
            self.prune_from(node_id);
        }
        removed
    }

    /// Return the longest exact cached prefix for `token_ids`.
    ///
    /// # Errors
    ///
    /// Returns [`ExactIndexError::LookupBudgetExceeded`] if ambiguous block
    /// geometries require more work than the configured fail-closed budget.
    pub fn find_longest(&self, token_ids: &[u32]) -> Result<ExactMatch, ExactIndexError> {
        let mut best = ExactMatch::default();
        let mut stack = vec![(ROOT_NODE, 0usize, 0usize)];
        let mut steps = 0usize;
        while let Some((node_id, offset, blocks)) = stack.pop() {
            if offset > best.token_ids || (offset == best.token_ids && blocks > best.blocks) {
                best = ExactMatch {
                    blocks,
                    token_ids: offset,
                };
            }
            let node = self.node(node_id);
            for &length in &node.child_lengths {
                steps = steps.saturating_add(1);
                if steps > self.limits.max_lookup_steps {
                    return Err(ExactIndexError::LookupBudgetExceeded);
                }
                let Some(end) = offset
                    .checked_add(length)
                    .filter(|end| *end <= token_ids.len())
                else {
                    continue;
                };
                let Some(child_id) = node.children.get(&token_ids[offset..end]).copied() else {
                    continue;
                };
                if self.node(child_id).present {
                    stack.push((child_id, end, blocks + 1));
                }
            }
        }
        Ok(best)
    }

    fn insert_child(
        &mut self,
        parent_id: usize,
        chunk: &[u32],
        external_hash: ExternalBlockHash,
    ) -> usize {
        let block: Arc<[u32]> = Arc::from(chunk);
        let node_id = self.allocate_node(Node {
            parent: Some(parent_id),
            block: block.clone(),
            children: HashMap::new(),
            child_lengths: Vec::new(),
            external_hash: Some(external_hash),
            present: true,
        });
        let parent = self.node_mut(parent_id);
        parent.children.insert(block, node_id);
        if !parent.child_lengths.contains(&chunk.len()) {
            parent.child_lengths.push(chunk.len());
            parent
                .child_lengths
                .sort_unstable_by(|left, right| right.cmp(left));
        }
        self.live_nodes += 1;
        self.resident_token_ids += chunk.len();
        node_id
    }

    fn allocate_node(&mut self, node: Node) -> usize {
        if let Some(node_id) = self.free_nodes.pop() {
            self.nodes[node_id] = Some(node);
            node_id
        } else {
            self.nodes.push(Some(node));
            self.nodes.len() - 1
        }
    }

    fn prune_from(&mut self, mut node_id: usize) {
        while node_id != ROOT_NODE {
            let should_prune = {
                let node = self.node(node_id);
                !node.present && node.children.is_empty()
            };
            if !should_prune {
                break;
            }
            let node = self.nodes[node_id].take().expect("live exact index node");
            let parent_id = node.parent.expect("non-root exact index node has parent");
            self.node_mut(parent_id)
                .children
                .remove(node.block.as_ref());
            self.live_nodes -= 1;
            self.resident_token_ids -= node.block.len();
            self.free_nodes.push(node_id);
            node_id = parent_id;
        }
    }

    fn node(&self, node_id: usize) -> &Node {
        self.nodes[node_id].as_ref().expect("live exact index node")
    }

    fn node_mut(&mut self, node_id: usize) -> &mut Node {
        self.nodes[node_id].as_mut().expect("live exact index node")
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AttentionKind {
    Main,
    NonMain,
    Unknown,
}

impl AttentionKind {
    fn from_wire(value: &str) -> Self {
        match value {
            "full_attention" | "mla_attention" | "sink_full_attention" => Self::Main,
            "sliding_window"
            | "sliding_window_mla"
            | "mamba"
            | "chunked_local_attention"
            | "encoder_only_attention"
            | "cross_attention" => Self::NonMain,
            _ => Self::Unknown,
        }
    }
}

#[derive(Debug)]
pub struct ExactKvInventory {
    index: ExactKvIndex,
    group_kinds: HashMap<(u32, u32), AttentionKind>,
    unlearned_groups: HashSet<(u32, u32)>,
    group_block_sizes: HashMap<(u32, u32), usize>,
}

impl ExactKvInventory {
    #[must_use]
    pub fn new(limits: ExactIndexLimits) -> Self {
        Self {
            index: ExactKvIndex::new(limits),
            group_kinds: HashMap::new(),
            unlearned_groups: HashSet::new(),
            group_block_sizes: HashMap::new(),
        }
    }

    pub fn clear(&mut self) {
        self.index.clear();
    }

    pub fn reset_generation(&mut self) {
        self.index.clear();
        self.group_kinds.clear();
        self.unlearned_groups.clear();
        self.group_block_sizes.clear();
    }

    #[must_use]
    pub fn stats(&self) -> ExactIndexStats {
        self.index.stats()
    }

    #[must_use]
    pub fn group_coverage(&self) -> ExactCacheGroupCoverage {
        let mut coverage = ExactCacheGroupCoverage {
            unlearned_groups: self.unlearned_groups.len(),
            ..ExactCacheGroupCoverage::default()
        };
        for kind in self.group_kinds.values() {
            match kind {
                AttentionKind::Main => {
                    coverage.main_groups = coverage.main_groups.saturating_add(1);
                }
                AttentionKind::NonMain => {
                    coverage.non_main_groups = coverage.non_main_groups.saturating_add(1);
                }
                AttentionKind::Unknown => {
                    coverage.unknown_groups = coverage.unknown_groups.saturating_add(1);
                }
            }
        }
        coverage
    }

    /// Apply one decoded event after conservative cache-tier/group filtering.
    ///
    /// # Errors
    ///
    /// Returns [`ExactIndexError`] when an accepted event would make the exact
    /// inventory incomplete or internally inconsistent. Callers must fence and
    /// clear the generation rather than continue routing from partial state.
    pub fn apply_event(
        &mut self,
        data_parallel_rank: u32,
        event: &KvEvent,
    ) -> Result<ApplyOutcome, ExactIndexError> {
        match event {
            KvEvent::BlockStored(stored) => {
                self.learn_group(data_parallel_rank, stored)?;
                if let Some(reason) = filter_store(stored, data_parallel_rank, &self.group_kinds) {
                    return Ok(ApplyOutcome::Filtered(reason));
                }
                match self.index.store(stored) {
                    Ok(blocks) => Ok(ApplyOutcome::Stored { blocks }),
                    Err(ExactIndexError::ParentNotFound) => Ok(ApplyOutcome::Filtered(
                        if self.is_unsupported_partial(data_parallel_rank, stored) {
                            FilterReason::UnsupportedPartialBlock
                        } else {
                            FilterReason::OrphanedParent
                        },
                    )),
                    Err(error) => Err(error),
                }
            }
            KvEvent::BlockRemoved(removed) => {
                self.observe_unlearned_group(data_parallel_rank, removed.group_idx)?;
                if let Some(reason) = filter_remove(removed, data_parallel_rank, &self.group_kinds)
                {
                    return Ok(ApplyOutcome::Filtered(reason));
                }
                Ok(ApplyOutcome::Removed {
                    blocks: self.index.remove(removed),
                })
            }
            KvEvent::AllBlocksCleared => {
                self.index.clear();
                Ok(ApplyOutcome::Cleared)
            }
        }
    }

    /// Find the longest exact prefix under a shared read lock.
    ///
    /// # Errors
    ///
    /// Returns [`ExactIndexError::LookupBudgetExceeded`] when the configured
    /// fail-closed work budget is exhausted.
    pub fn find_longest(&self, token_ids: &[u32]) -> Result<ExactMatch, ExactIndexError> {
        self.index.find_longest(token_ids)
    }

    fn apply_batch(&mut self, batch: &KvEventBatch) -> Result<BatchApplySummary, ExactIndexError> {
        let rank = batch.data_parallel_rank.unwrap_or(0);
        let mut summary = BatchApplySummary::default();
        for event in &batch.events {
            match self.apply_event(rank, event)? {
                ApplyOutcome::Stored { blocks } => summary.stored_blocks += blocks,
                ApplyOutcome::Removed { blocks } => summary.removed_blocks += blocks,
                ApplyOutcome::Cleared => summary.clear_events += 1,
                ApplyOutcome::Filtered(reason) => {
                    summary.filtered_events += 1;
                    summary.filtered_reasons.increment(reason);
                }
            }
        }
        Ok(summary)
    }

    fn learn_group(&mut self, rank: u32, stored: &BlockStored) -> Result<(), ExactIndexError> {
        let Some(group) = stored.group_idx else {
            return Ok(());
        };
        let key = (rank, group);
        let Some(kind) = stored.kv_cache_spec_kind.as_deref() else {
            return self.observe_unlearned_group(rank, Some(group));
        };
        let observed = AttentionKind::from_wire(kind);
        if let Some(existing) = self.group_kinds.get(&key) {
            if *existing != observed {
                return Err(ExactIndexError::ConflictingGroupKind);
            }
        } else {
            self.ensure_group_capacity(key)?;
        }
        self.group_kinds.insert(key, observed);
        self.unlearned_groups.remove(&key);
        if stored.parent_block_hash.is_none() {
            self.group_block_sizes
                .entry(key)
                .and_modify(|size| *size = (*size).max(stored.block_size))
                .or_insert(stored.block_size);
        }
        Ok(())
    }

    fn observe_unlearned_group(
        &mut self,
        rank: u32,
        group: Option<u32>,
    ) -> Result<(), ExactIndexError> {
        let Some(group) = group else {
            return Ok(());
        };
        let key = (rank, group);
        if self.group_kinds.contains_key(&key) || self.unlearned_groups.contains(&key) {
            return Ok(());
        }
        self.ensure_group_capacity(key)?;
        self.unlearned_groups.insert(key);
        Ok(())
    }

    fn ensure_group_capacity(&self, key: (u32, u32)) -> Result<(), ExactIndexError> {
        if !self.group_kinds.contains_key(&key)
            && !self.unlearned_groups.contains(&key)
            && self
                .group_kinds
                .len()
                .saturating_add(self.unlearned_groups.len())
                >= self.index.limits.max_nodes
        {
            return Err(ExactIndexError::CapacityExceeded);
        }
        Ok(())
    }

    fn is_unsupported_partial(&self, rank: u32, stored: &BlockStored) -> bool {
        let Some(group) = stored.group_idx else {
            return false;
        };
        self.group_block_sizes
            .get(&(rank, group))
            .is_some_and(|canonical| stored.block_size < *canonical)
    }
}

#[derive(Debug)]
pub struct SharedExactKvInventory {
    inner: RwLock<ExactKvInventory>,
}

impl SharedExactKvInventory {
    #[must_use]
    pub fn new(limits: ExactIndexLimits) -> Self {
        Self {
            inner: RwLock::new(ExactKvInventory::new(limits)),
        }
    }

    /// Apply one event under the per-engine write lock.
    ///
    /// # Errors
    ///
    /// Returns [`ExactIndexError`] for the same fail-closed invariants as
    /// [`ExactKvInventory::apply_event`].
    pub fn apply_event(
        &self,
        data_parallel_rank: u32,
        event: &KvEvent,
    ) -> Result<ApplyOutcome, ExactIndexError> {
        self.inner.write().apply_event(data_parallel_rank, event)
    }

    /// Find the longest prefix under the per-engine read lock.
    ///
    /// # Errors
    ///
    /// Returns [`ExactIndexError::LookupBudgetExceeded`] when the configured
    /// fail-closed work budget is exhausted.
    pub fn find_longest(&self, token_ids: &[u32]) -> Result<ExactMatch, ExactIndexError> {
        self.inner.read().find_longest(token_ids)
    }

    #[must_use]
    pub fn stats(&self) -> ExactIndexStats {
        self.inner.read().stats()
    }

    #[must_use]
    pub fn group_coverage(&self) -> ExactCacheGroupCoverage {
        self.inner.read().group_coverage()
    }

    pub fn clear(&self) {
        self.inner.write().clear();
    }
}

/// Transactional accumulator for a full replay beginning at sequence zero.
///
/// Decoded event batches are applied directly to this private scratch index and
/// then dropped. Only the bounded sequence vector and final exact inventory
/// survive until commit, so a large retained publisher history never becomes a
/// `Vec<KvEventBatch>` alongside the index it constructs.
#[derive(Debug)]
pub(crate) struct FullReplayStage {
    inventory: ExactKvInventory,
    sequences: Vec<u64>,
    summary: BatchApplySummary,
    establishes_boundary: bool,
    error: Option<ExactIndexError>,
}

impl FullReplayStage {
    fn new(limits: ExactIndexLimits) -> Self {
        Self {
            inventory: ExactKvInventory::new(limits),
            sequences: Vec::new(),
            summary: BatchApplySummary::default(),
            establishes_boundary: false,
            error: None,
        }
    }

    pub(crate) fn ingest(&mut self, sequence: u64, batch: &KvEventBatch) {
        self.sequences.push(sequence);
        self.establishes_boundary |= batch.clears_all();
        if self.error.is_some() {
            return;
        }
        match self.inventory.apply_batch(batch) {
            Ok(applied) => merge_summary(&mut self.summary, applied),
            Err(error) => self.error = Some(error),
        }
    }

    #[must_use]
    pub(crate) fn batch_count(&self) -> usize {
        self.sequences.len()
    }
}

#[derive(Debug)]
pub struct FencedExactKvInventory {
    fence: KvEventFence,
    inventory: ExactKvInventory,
    revision: u64,
}

impl FencedExactKvInventory {
    #[must_use]
    pub fn new(replay_limit: u64, index_limits: ExactIndexLimits) -> Self {
        Self {
            fence: KvEventFence::new(replay_limit),
            inventory: ExactKvInventory::new(index_limits),
            revision: 0,
        }
    }

    #[must_use]
    pub const fn trusted(&self) -> bool {
        self.fence.trusted()
    }

    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.fence.generation()
    }

    /// Monotonic process-local version of the observable inventory state.
    /// Shadow comparisons use this to reject an alternative cache that changed
    /// between the approximate route decision and the later exact lookup.
    #[must_use]
    pub const fn revision(&self) -> u64 {
        self.revision
    }

    #[must_use]
    pub fn stats(&self) -> ExactIndexStats {
        self.inventory.stats()
    }

    #[must_use]
    pub fn group_coverage(&self) -> ExactCacheGroupCoverage {
        self.inventory.group_coverage()
    }

    /// Allocate an isolated accumulator for a full generation replay.
    #[must_use]
    pub(crate) fn begin_full_replay(&self) -> FullReplayStage {
        FullReplayStage::new(self.inventory.index.limits)
    }

    /// Process one live sequence without ever exposing partial exact state.
    ///
    /// # Errors
    ///
    /// Returns [`ExactIndexError`] when an authoritative batch violates index
    /// invariants. The generation is fenced and cleared before returning.
    pub fn ingest_live(
        &mut self,
        sequence: u64,
        batch: &KvEventBatch,
    ) -> Result<LiveBatchOutcome, ExactIndexError> {
        let outcome = match self.fence.ingest(sequence, batch.clears_all()) {
            IngestAction::Apply => self
                .apply_authoritative(batch)
                .map(LiveBatchOutcome::Applied),
            IngestAction::ResetAndApply => {
                self.inventory.clear();
                self.apply_authoritative(batch)
                    .map(LiveBatchOutcome::Applied)
            }
            IngestAction::Duplicate => Ok(LiveBatchOutcome::Duplicate),
            IngestAction::ObserveOnly => Ok(LiveBatchOutcome::ObserveOnly),
            IngestAction::Replay { from, through } => {
                Ok(LiveBatchOutcome::Replay { from, through })
            }
            IngestAction::UnrecoverableGap => {
                self.inventory.reset_generation();
                Ok(LiveBatchOutcome::Fenced)
            }
        };
        if matches!(
            &outcome,
            Ok(LiveBatchOutcome::Applied(_) | LiveBatchOutcome::Fenced) | Err(_)
        ) {
            self.revision = self.revision.wrapping_add(1);
        }
        outcome
    }

    /// Validate and apply a complete inclusive replay response.
    ///
    /// # Errors
    ///
    /// Returns [`ExactIndexError`] when an authoritative replay violates index
    /// invariants. The generation is fenced and cleared before returning.
    pub fn ingest_replay(
        &mut self,
        batches: &[(u64, KvEventBatch)],
    ) -> Result<ReplayBatchOutcome, ExactIndexError> {
        let sequences = batches
            .iter()
            .map(|(sequence, _)| *sequence)
            .collect::<Vec<_>>();
        let establishes_boundary = batches.iter().any(|(_, batch)| batch.clears_all());
        let outcome = match self.fence.accept_replay(&sequences, establishes_boundary) {
            ReplayAction::Invalid => {
                self.inventory.reset_generation();
                Ok(ReplayBatchOutcome::Invalid)
            }
            ReplayAction::RecoveredObserveOnly => Ok(ReplayBatchOutcome::ObserveOnly),
            ReplayAction::Recovered => {
                let mut summary = BatchApplySummary::default();
                for (_, batch) in batches {
                    match self.inventory.apply_batch(batch) {
                        Ok(applied) => merge_summary(&mut summary, applied),
                        Err(error) => {
                            self.fence.generation_changed();
                            self.inventory.reset_generation();
                            self.revision = self.revision.wrapping_add(1);
                            return Err(error);
                        }
                    }
                }
                Ok(ReplayBatchOutcome::Applied(summary))
            }
        };
        if matches!(
            &outcome,
            Ok(ReplayBatchOutcome::Applied(_) | ReplayBatchOutcome::Invalid) | Err(_)
        ) {
            self.revision = self.revision.wrapping_add(1);
        }
        outcome
    }

    /// Atomically replace exact state after a streamed full replay validates.
    ///
    /// The live inventory is never mutated during staging. An index error or
    /// invalid sequence boundary discards the scratch state, clears the old
    /// generation, and leaves exact routing fenced.
    ///
    /// # Errors
    ///
    /// Returns the first staged index error after fencing and clearing the
    /// current generation.
    pub(crate) fn commit_full_replay(
        &mut self,
        mut stage: FullReplayStage,
    ) -> Result<ReplayBatchOutcome, ExactIndexError> {
        if let Some(error) = stage.error.take() {
            self.fence.generation_changed();
            self.inventory.reset_generation();
            self.revision = self.revision.wrapping_add(1);
            return Err(error);
        }
        let outcome = match self
            .fence
            .accept_replay(&stage.sequences, stage.establishes_boundary)
        {
            ReplayAction::Invalid => {
                self.inventory.reset_generation();
                ReplayBatchOutcome::Invalid
            }
            ReplayAction::RecoveredObserveOnly => ReplayBatchOutcome::ObserveOnly,
            ReplayAction::Recovered => {
                self.inventory = stage.inventory;
                ReplayBatchOutcome::Applied(stage.summary)
            }
        };
        if matches!(
            outcome,
            ReplayBatchOutcome::Applied(_) | ReplayBatchOutcome::Invalid
        ) {
            self.revision = self.revision.wrapping_add(1);
        }
        Ok(outcome)
    }

    /// Fence and clear state when the engine process/cache generation changes.
    pub fn generation_changed(&mut self) {
        self.fence.generation_changed();
        self.inventory.reset_generation();
        self.revision = self.revision.wrapping_add(1);
    }

    /// Clear the disconnected generation and prepare a bounded replay from
    /// sequence zero through the last known live sequence.
    #[must_use]
    pub fn prepare_full_replay_retry(&mut self, through: u64) -> bool {
        self.generation_changed();
        self.fence.prepare_full_replay(through)
    }

    /// Query only while the sequence fence declares the inventory complete.
    ///
    /// # Errors
    ///
    /// Returns [`ExactIndexError::LookupBudgetExceeded`] when the configured
    /// fail-closed work budget is exhausted.
    pub fn find_longest(&self, token_ids: &[u32]) -> Result<Option<ExactMatch>, ExactIndexError> {
        if !self.fence.trusted() {
            return Ok(None);
        }
        self.inventory.find_longest(token_ids).map(Some)
    }

    fn apply_authoritative(
        &mut self,
        batch: &KvEventBatch,
    ) -> Result<BatchApplySummary, ExactIndexError> {
        match self.inventory.apply_batch(batch) {
            Ok(summary) => Ok(summary),
            Err(error) => {
                self.fence.generation_changed();
                self.inventory.reset_generation();
                Err(error)
            }
        }
    }
}

fn merge_summary(total: &mut BatchApplySummary, next: BatchApplySummary) {
    total.stored_blocks += next.stored_blocks;
    total.removed_blocks += next.removed_blocks;
    total.filtered_events += next.filtered_events;
    total.clear_events += next.clear_events;
    total.filtered_reasons.merge(next.filtered_reasons);
}

fn filter_store(
    event: &BlockStored,
    rank: u32,
    groups: &HashMap<(u32, u32), AttentionKind>,
) -> Option<FilterReason> {
    common_filter(
        event.medium.as_deref(),
        event.locality.as_deref(),
        event.group_idx,
        event.kv_cache_spec_kind.as_deref(),
        rank,
        groups,
    )
    .or_else(|| {
        (event.lora_name.is_some() || event.cache_namespace.is_some() || event.has_extra_keys)
            .then_some(FilterReason::Namespaced)
    })
}

fn filter_remove(
    event: &BlockRemoved,
    rank: u32,
    groups: &HashMap<(u32, u32), AttentionKind>,
) -> Option<FilterReason> {
    common_filter(
        event.medium.as_deref(),
        event.locality.as_deref(),
        event.group_idx,
        None,
        rank,
        groups,
    )
}

fn common_filter(
    medium: Option<&str>,
    locality: Option<&str>,
    group_idx: Option<u32>,
    explicit_kind: Option<&str>,
    rank: u32,
    groups: &HashMap<(u32, u32), AttentionKind>,
) -> Option<FilterReason> {
    if locality.is_some_and(|value| value != "LOCAL") {
        return Some(FilterReason::NonLocal);
    }
    if medium.is_some_and(|value| value != "GPU") {
        return Some(FilterReason::UnsupportedMedium);
    }

    let kind = explicit_kind
        .map(AttentionKind::from_wire)
        .or_else(|| group_idx.and_then(|group| groups.get(&(rank, group)).copied()));
    match kind {
        Some(AttentionKind::Main) => None,
        Some(AttentionKind::NonMain) => Some(FilterReason::NonMainAttention),
        Some(AttentionKind::Unknown) => Some(FilterReason::UnknownAttentionKind),
        None if group_idx.is_none() || group_idx == Some(0) => None,
        None => Some(FilterReason::UnlearnedAttentionGroup),
    }
}

#[cfg(test)]
mod tests {
    use std::thread;

    use super::*;

    fn hash(value: u64) -> ExternalBlockHash {
        ExternalBlockHash::Unsigned(value)
    }

    fn store_event(
        hashes: &[u64],
        parent: Option<u64>,
        tokens: &[u32],
        block_size: usize,
    ) -> BlockStored {
        BlockStored {
            block_hashes: hashes.iter().copied().map(hash).collect(),
            parent_block_hash: parent.map(hash),
            token_ids: tokens.to_vec(),
            block_size,
            group_idx: Some(0),
            kv_cache_spec_kind: None,
            kv_cache_spec_sliding_window: None,
            medium: Some("GPU".to_owned()),
            locality: Some("LOCAL".to_owned()),
            lora_name: None,
            cache_namespace: None,
            has_extra_keys: false,
        }
    }

    fn remove_event(hashes: &[u64]) -> BlockRemoved {
        BlockRemoved {
            block_hashes: hashes.iter().copied().map(hash).collect(),
            group_idx: Some(0),
            medium: Some("GPU".to_owned()),
            locality: Some("LOCAL".to_owned()),
        }
    }

    fn batch(events: Vec<KvEvent>) -> KvEventBatch {
        KvEventBatch {
            timestamp: 1.0,
            events,
            data_parallel_rank: Some(0),
        }
    }

    #[test]
    fn stores_branches_and_returns_longest_exact_prefix() {
        let mut index = ExactKvIndex::new(ExactIndexLimits::default());
        let trunk = store_event(&[10, 11], None, &[1, 2, 3, 4], 2);
        index.store(&trunk).unwrap();
        let trunk_stats = index.stats();
        assert_eq!(index.store(&trunk), Ok(2));
        assert_eq!(index.stats(), trunk_stats);
        index
            .store(&store_event(&[12], Some(10), &[8, 9], 2))
            .unwrap();

        assert_eq!(
            index.find_longest(&[1, 2, 3, 4, 5]).unwrap(),
            ExactMatch {
                blocks: 2,
                token_ids: 4
            }
        );
        assert_eq!(
            index.find_longest(&[1, 2, 8, 9, 5]).unwrap(),
            ExactMatch {
                blocks: 2,
                token_ids: 4
            }
        );
        assert_eq!(index.find_longest(&[1, 7]).unwrap(), ExactMatch::default());
    }

    #[test]
    fn removal_prunes_leaves_and_readding_restores_descendants() {
        let mut index = ExactKvIndex::new(ExactIndexLimits::default());
        let event = store_event(&[10, 11], None, &[1, 2, 3, 4], 2);
        index.store(&event).unwrap();
        assert_eq!(index.remove(&remove_event(&[10])), 1);
        assert_eq!(index.find_longest(&[1, 2, 3, 4]).unwrap().token_ids, 0);
        assert_eq!(index.store(&store_event(&[10], None, &[1, 2], 2)), Ok(1));
        assert_eq!(index.find_longest(&[1, 2, 3, 4]).unwrap().token_ids, 4);

        assert_eq!(index.remove(&remove_event(&[11, 10])), 2);
        assert_eq!(index.stats(), ExactIndexStats::default());
    }

    #[test]
    fn rejects_unknown_parent_conflicts_and_self_reference_atomically() {
        let mut index = ExactKvIndex::new(ExactIndexLimits::default());
        assert_eq!(
            index.store(&store_event(&[2], Some(1), &[3, 4], 2)),
            Err(ExactIndexError::ParentNotFound)
        );
        index.store(&store_event(&[1], None, &[1, 2], 2)).unwrap();
        let before = index.stats();
        assert_eq!(
            index.store(&store_event(&[1], None, &[9, 9], 2)),
            Err(ExactIndexError::ConflictingPath)
        );
        assert_eq!(
            index.store(&store_event(&[1], Some(1), &[3, 4], 2)),
            Err(ExactIndexError::DuplicateHash)
        );
        assert_eq!(index.stats(), before);
    }

    #[test]
    fn rejects_masked_shape_if_it_reaches_main_attention_index() {
        let mut index = ExactKvIndex::new(ExactIndexLimits::default());
        let malformed = store_event(&[1], None, &[1, 2, 3], 2);
        assert_eq!(
            index.store(&malformed),
            Err(ExactIndexError::InconsistentBlockShape)
        );
        assert_eq!(index.stats(), ExactIndexStats::default());
    }

    #[test]
    fn capacity_failure_does_not_partially_apply_event() {
        let limits = ExactIndexLimits {
            max_nodes: 2,
            max_token_ids: 4,
            max_lookup_steps: 16,
        };
        let mut index = ExactKvIndex::new(limits);
        assert_eq!(
            index.store(&store_event(&[1, 2, 3], None, &[1, 2, 3, 4, 5, 6], 2)),
            Err(ExactIndexError::CapacityExceeded)
        );
        assert_eq!(index.stats(), ExactIndexStats::default());
    }

    #[test]
    fn variable_block_geometries_choose_the_longest_valid_path() {
        let mut index = ExactKvIndex::new(ExactIndexLimits::default());
        index.store(&store_event(&[1], None, &[1, 2], 2)).unwrap();
        index
            .store(&store_event(&[2], None, &[1, 2, 3, 4], 4))
            .unwrap();
        index
            .store(&store_event(&[3], Some(1), &[3, 4, 5], 3))
            .unwrap();
        assert_eq!(
            index.find_longest(&[1, 2, 3, 4, 5, 6]).unwrap(),
            ExactMatch {
                blocks: 2,
                token_ids: 5
            }
        );
    }

    #[test]
    fn inventory_filters_non_main_and_namespaced_state() {
        let mut inventory = ExactKvInventory::new(ExactIndexLimits::default());
        let mut mamba = store_event(&[1], None, &[1, 2], 2);
        mamba.group_idx = Some(2);
        mamba.kv_cache_spec_kind = Some("mamba".to_owned());
        assert_eq!(
            inventory
                .apply_event(0, &KvEvent::BlockStored(mamba))
                .unwrap(),
            ApplyOutcome::Filtered(FilterReason::NonMainAttention)
        );
        let mut removal = remove_event(&[1]);
        removal.group_idx = Some(2);
        assert_eq!(
            inventory
                .apply_event(0, &KvEvent::BlockRemoved(removal))
                .unwrap(),
            ApplyOutcome::Filtered(FilterReason::NonMainAttention)
        );

        let mut namespaced = store_event(&[2], None, &[3, 4], 2);
        namespaced.kv_cache_spec_kind = Some("full_attention".to_owned());
        namespaced.has_extra_keys = true;
        assert_eq!(
            inventory
                .apply_event(0, &KvEvent::BlockStored(namespaced))
                .unwrap(),
            ApplyOutcome::Filtered(FilterReason::Namespaced)
        );
        assert_eq!(inventory.stats(), ExactIndexStats::default());
    }

    #[test]
    fn hybrid_group_coverage_keeps_shadow_available_until_semantics_are_complete() {
        let mut inventory = ExactKvInventory::new(ExactIndexLimits::default());
        assert!(inventory.group_coverage().placement_ready());

        let mut unlearned = store_event(&[1], None, &[1, 2], 2);
        unlearned.group_idx = Some(3);
        assert_eq!(
            inventory
                .apply_event(0, &KvEvent::BlockStored(unlearned))
                .unwrap(),
            ApplyOutcome::Filtered(FilterReason::UnlearnedAttentionGroup)
        );
        assert_eq!(
            inventory.group_coverage(),
            ExactCacheGroupCoverage {
                unlearned_groups: 1,
                ..ExactCacheGroupCoverage::default()
            }
        );
        assert!(!inventory.group_coverage().placement_ready());

        let mut main = store_event(&[2], None, &[3, 4], 2);
        main.group_idx = Some(3);
        main.kv_cache_spec_kind = Some("full_attention".to_owned());
        inventory
            .apply_event(0, &KvEvent::BlockStored(main))
            .unwrap();
        let mut mamba = store_event(&[3], None, &[5, 6], 2);
        mamba.group_idx = Some(4);
        mamba.kv_cache_spec_kind = Some("mamba".to_owned());
        inventory
            .apply_event(0, &KvEvent::BlockStored(mamba))
            .unwrap();
        assert_eq!(
            inventory.group_coverage(),
            ExactCacheGroupCoverage {
                main_groups: 1,
                non_main_groups: 1,
                unknown_groups: 0,
                unlearned_groups: 0,
            }
        );
        assert!(inventory.group_coverage().placement_ready());

        let mut future = store_event(&[4], None, &[7, 8], 2);
        future.group_idx = Some(5);
        future.kv_cache_spec_kind = Some("future_attention".to_owned());
        inventory
            .apply_event(0, &KvEvent::BlockStored(future))
            .unwrap();
        assert_eq!(inventory.group_coverage().unknown_groups, 1);
        assert!(!inventory.group_coverage().placement_ready());

        let mut conflicting = store_event(&[5], None, &[9, 10], 2);
        conflicting.group_idx = Some(3);
        conflicting.kv_cache_spec_kind = Some("mamba".to_owned());
        assert_eq!(
            inventory.apply_event(0, &KvEvent::BlockStored(conflicting)),
            Err(ExactIndexError::ConflictingGroupKind)
        );
    }

    #[test]
    fn inventory_filters_masked_non_main_shape_before_exact_index() {
        let mut inventory = ExactKvInventory::new(ExactIndexLimits::default());
        let mut sliding = store_event(&[1], None, &[1, 2, 3], 2);
        sliding.group_idx = Some(1);
        sliding.kv_cache_spec_kind = Some("sliding_window_mla".to_owned());
        assert_eq!(
            inventory
                .apply_event(0, &KvEvent::BlockStored(sliding))
                .unwrap(),
            ApplyOutcome::Filtered(FilterReason::NonMainAttention)
        );
        assert_eq!(inventory.stats(), ExactIndexStats::default());
    }

    #[test]
    fn inventory_safely_underestimates_orphaned_state() {
        let mut inventory = ExactKvInventory::new(ExactIndexLimits::default());
        let mut root = store_event(&[1], None, &[1, 2, 3, 4], 4);
        root.kv_cache_spec_kind = Some("mla_attention".to_owned());
        inventory
            .apply_event(0, &KvEvent::BlockStored(root))
            .unwrap();

        let mut partial = store_event(&[2], Some(99), &[5, 6], 2);
        partial.kv_cache_spec_kind = Some("mla_attention".to_owned());
        assert_eq!(
            inventory
                .apply_event(0, &KvEvent::BlockStored(partial))
                .unwrap(),
            ApplyOutcome::Filtered(FilterReason::UnsupportedPartialBlock)
        );

        let mut missing_full_parent = store_event(&[3], Some(99), &[5, 6, 7, 8], 4);
        missing_full_parent.kv_cache_spec_kind = Some("mla_attention".to_owned());
        assert_eq!(
            inventory
                .apply_event(0, &KvEvent::BlockStored(missing_full_parent))
                .unwrap(),
            ApplyOutcome::Filtered(FilterReason::OrphanedParent)
        );
    }

    #[test]
    fn shared_inventory_allows_concurrent_readers() {
        let inventory = Arc::new(SharedExactKvInventory::new(ExactIndexLimits::default()));
        inventory
            .apply_event(
                0,
                &KvEvent::BlockStored(store_event(&[1, 2], None, &[1, 2, 3, 4], 2)),
            )
            .unwrap();
        let readers = (0..8)
            .map(|_| {
                let inventory = inventory.clone();
                thread::spawn(move || {
                    for _ in 0..1_000 {
                        assert_eq!(
                            inventory.find_longest(&[1, 2, 3, 4, 5]).unwrap().token_ids,
                            4
                        );
                    }
                })
            })
            .collect::<Vec<_>>();
        for reader in readers {
            reader.join().unwrap();
        }
    }

    #[test]
    fn fenced_inventory_never_queries_startup_observations() {
        let mut state = FencedExactKvInventory::new(8, ExactIndexLimits::default());
        let stored = batch(vec![KvEvent::BlockStored(store_event(
            &[1],
            None,
            &[1, 2],
            2,
        ))]);
        assert_eq!(
            state.ingest_live(10, &stored).unwrap(),
            LiveBatchOutcome::ObserveOnly
        );
        assert_eq!(state.stats(), ExactIndexStats::default());
        assert_eq!(state.find_longest(&[1, 2]).unwrap(), None);

        let cleared = batch(vec![KvEvent::AllBlocksCleared]);
        assert!(matches!(
            state.ingest_live(11, &cleared).unwrap(),
            LiveBatchOutcome::Applied(_)
        ));
        assert!(state.trusted());
        assert!(matches!(
            state.ingest_live(12, &stored).unwrap(),
            LiveBatchOutcome::Applied(_)
        ));
        assert_eq!(
            state.find_longest(&[1, 2, 3]).unwrap(),
            Some(ExactMatch {
                blocks: 1,
                token_ids: 2
            })
        );
    }

    #[test]
    fn fenced_inventory_applies_contiguous_replay_before_resuming() {
        let mut state = FencedExactKvInventory::new(8, ExactIndexLimits::default());
        state
            .ingest_live(0, &batch(vec![KvEvent::AllBlocksCleared]))
            .unwrap();
        state
            .ingest_live(
                1,
                &batch(vec![KvEvent::BlockStored(store_event(
                    &[1],
                    None,
                    &[1, 2],
                    2,
                ))]),
            )
            .unwrap();
        let third = batch(vec![KvEvent::BlockStored(store_event(
            &[3],
            Some(2),
            &[5, 6],
            2,
        ))]);
        assert_eq!(
            state.ingest_live(3, &third).unwrap(),
            LiveBatchOutcome::Replay {
                from: 2,
                through: 3
            }
        );
        assert_eq!(state.find_longest(&[1, 2, 3, 4, 5, 6]).unwrap(), None);

        let second = batch(vec![KvEvent::BlockStored(store_event(
            &[2],
            Some(1),
            &[3, 4],
            2,
        ))]);
        assert!(matches!(
            state.ingest_replay(&[(2, second), (3, third)]).unwrap(),
            ReplayBatchOutcome::Applied(_)
        ));
        assert!(state.trusted());
        assert_eq!(
            state.find_longest(&[1, 2, 3, 4, 5, 6]).unwrap(),
            Some(ExactMatch {
                blocks: 3,
                token_ids: 6
            })
        );
    }

    #[test]
    fn clear_inside_startup_replay_can_establish_trust() {
        let mut state = FencedExactKvInventory::new(8, ExactIndexLimits::default());
        state.ingest_live(10, &batch(Vec::new())).unwrap();
        assert!(matches!(
            state.ingest_live(12, &batch(Vec::new())).unwrap(),
            LiveBatchOutcome::Replay { .. }
        ));
        let replay = vec![
            (11, batch(vec![KvEvent::AllBlocksCleared])),
            (
                12,
                batch(vec![KvEvent::BlockStored(store_event(
                    &[1],
                    None,
                    &[1, 2],
                    2,
                ))]),
            ),
        ];
        assert!(matches!(
            state.ingest_replay(&replay).unwrap(),
            ReplayBatchOutcome::Applied(_)
        ));
        assert!(state.trusted());
        assert_eq!(state.find_longest(&[1, 2]).unwrap().unwrap().token_ids, 2);
    }

    #[test]
    fn invalid_replay_or_index_error_clears_and_fences_state() {
        let mut state = FencedExactKvInventory::new(8, ExactIndexLimits::default());
        state
            .ingest_live(0, &batch(vec![KvEvent::AllBlocksCleared]))
            .unwrap();
        assert!(matches!(
            state.ingest_live(2, &batch(Vec::new())).unwrap(),
            LiveBatchOutcome::Replay { .. }
        ));
        assert_eq!(
            state.ingest_replay(&[(3, batch(Vec::new()))]).unwrap(),
            ReplayBatchOutcome::Invalid
        );
        assert!(!state.trusted());
        assert_eq!(state.stats(), ExactIndexStats::default());

        state
            .ingest_live(3, &batch(vec![KvEvent::AllBlocksCleared]))
            .unwrap();
        let invalid = batch(vec![KvEvent::BlockStored(store_event(
            &[9, 10],
            None,
            &[1],
            2,
        ))]);
        assert_eq!(
            state.ingest_live(4, &invalid),
            Err(ExactIndexError::InconsistentBlockShape)
        );
        assert!(!state.trusted());
        assert_eq!(state.stats(), ExactIndexStats::default());
    }

    #[test]
    fn reconnect_retry_rebuilds_only_from_a_complete_generation() {
        let mut state = FencedExactKvInventory::new(4, ExactIndexLimits::default());
        assert!(state.prepare_full_replay_retry(1));
        let replay = vec![
            (0, batch(vec![KvEvent::AllBlocksCleared])),
            (
                1,
                batch(vec![KvEvent::BlockStored(store_event(
                    &[1],
                    None,
                    &[1, 2],
                    2,
                ))]),
            ),
        ];
        assert!(matches!(
            state.ingest_replay(&replay).unwrap(),
            ReplayBatchOutcome::Applied(_)
        ));
        assert!(state.trusted());
        assert_eq!(state.stats().token_ids, 2);

        assert!(!state.prepare_full_replay_retry(4));
        assert!(!state.trusted());
        assert_eq!(state.stats(), ExactIndexStats::default());
    }

    #[test]
    fn streamed_full_replay_is_invisible_until_atomic_commit() {
        let mut state = FencedExactKvInventory::new(8, ExactIndexLimits::default());
        assert_eq!(
            state.ingest_live(3, &batch(Vec::new())).unwrap(),
            LiveBatchOutcome::Replay {
                from: 0,
                through: 3
            }
        );
        let mut scratch = state.begin_full_replay();
        scratch.ingest(
            0,
            &batch(vec![KvEvent::BlockStored(store_event(
                &[1],
                None,
                &[1, 2],
                2,
            ))]),
        );
        scratch.ingest(
            3,
            &batch(vec![KvEvent::BlockStored(store_event(
                &[2],
                Some(1),
                &[3, 4],
                2,
            ))]),
        );
        assert_eq!(scratch.batch_count(), 2);
        assert_eq!(state.stats(), ExactIndexStats::default());
        assert!(!state.trusted());

        assert!(matches!(
            state.commit_full_replay(scratch).unwrap(),
            ReplayBatchOutcome::Applied(_)
        ));
        assert!(state.trusted());
        assert_eq!(state.stats().token_ids, 4);
        assert_eq!(
            state.find_longest(&[1, 2, 3, 4]).unwrap(),
            Some(ExactMatch {
                blocks: 2,
                token_ids: 4
            })
        );
    }

    #[test]
    fn invalid_or_failed_streamed_full_replay_discards_every_staged_block() {
        let mut state = FencedExactKvInventory::new(8, ExactIndexLimits::default());
        state.ingest_live(3, &batch(Vec::new())).unwrap();
        let mut incomplete = state.begin_full_replay();
        incomplete.ingest(
            0,
            &batch(vec![KvEvent::BlockStored(store_event(
                &[1],
                None,
                &[1, 2],
                2,
            ))]),
        );
        assert_eq!(
            state.commit_full_replay(incomplete).unwrap(),
            ReplayBatchOutcome::Invalid
        );
        assert!(!state.trusted());
        assert_eq!(state.stats(), ExactIndexStats::default());

        assert!(state.prepare_full_replay_retry(1));
        let mut malformed = state.begin_full_replay();
        malformed.ingest(
            0,
            &batch(vec![KvEvent::BlockStored(store_event(
                &[9, 10],
                None,
                &[1],
                2,
            ))]),
        );
        malformed.ingest(1, &batch(Vec::new()));
        assert_eq!(
            state.commit_full_replay(malformed),
            Err(ExactIndexError::InconsistentBlockShape)
        );
        assert!(!state.trusted());
        assert_eq!(state.stats(), ExactIndexStats::default());
    }

    #[test]
    fn streamed_full_replay_bounds_filtered_group_metadata() {
        let limits = ExactIndexLimits {
            max_nodes: 2,
            max_token_ids: 16,
            max_lookup_steps: 16,
        };
        let mut state = FencedExactKvInventory::new(8, limits);
        assert!(matches!(
            state.ingest_live(2, &batch(Vec::new())).unwrap(),
            LiveBatchOutcome::Replay {
                from: 0,
                through: 2
            }
        ));
        let mut scratch = state.begin_full_replay();
        for sequence in 0..=2 {
            let mut filtered = store_event(&[sequence + 1], None, &[1, 2], 2);
            filtered.group_idx = Some(u32::try_from(sequence + 1).unwrap());
            filtered.kv_cache_spec_kind = Some("mamba".to_owned());
            scratch.ingest(sequence, &batch(vec![KvEvent::BlockStored(filtered)]));
        }

        assert_eq!(
            state.commit_full_replay(scratch),
            Err(ExactIndexError::CapacityExceeded)
        );
        assert!(!state.trusted());
        assert_eq!(state.stats(), ExactIndexStats::default());
    }
}
