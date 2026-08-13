//! Compact, bounded exact-KV index backed by keyed block commitments.
//!
//! Unlike [`crate::exact_index`], this index never retains raw token IDs. Each
//! block is represented by a 256-bit HMAC-SHA256 commitment split into a
//! compact map key and an independent collision guard. Engine block hashes are
//! retained only for parent and removal reconciliation.

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use serde_bytes::ByteBuf;
use thiserror::Error;

use crate::{
    block_digest::{BlockCommitment, BlockDigestError, BlockDigester, PrimaryCommitment},
    kv_snapshot::{DigestAlgorithm, GroupDisposition, ResetKind, SnapshotBlockHash, SnapshotBody},
    kv_wire::{BlockRemoved, BlockStored, ExternalBlockHash},
};

const ROOT_NODE: usize = 0;
const DIGEST_BYTES: usize = 32;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DigestIndexLimits {
    pub max_nodes: usize,
    pub max_logical_token_ids: usize,
    pub max_lookup_steps: usize,
    pub max_external_hash_bytes: usize,
    pub max_total_external_hash_bytes: usize,
}

impl Default for DigestIndexLimits {
    fn default() -> Self {
        Self {
            max_nodes: 131_072,
            max_logical_token_ids: 16_777_216,
            max_lookup_steps: 131_072,
            max_external_hash_bytes: 256,
            max_total_external_hash_bytes: 32 * 1024 * 1024,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DigestMatch {
    pub blocks: usize,
    pub token_ids: usize,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DigestIndexStats {
    pub nodes: usize,
    pub logical_token_ids: usize,
    pub external_hashes: usize,
    pub external_hash_bytes: usize,
    pub commitment_bytes: usize,
    pub poisoned_edges: usize,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct SnapshotGroupKey {
    pub data_parallel_rank: u32,
    pub group_idx: u32,
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum DigestIndexError {
    #[error("block digest construction failed")]
    BlockDigest,
    #[error("digest KV store has inconsistent block and token counts")]
    InconsistentBlockShape,
    #[error("digest KV store references an unknown parent")]
    ParentNotFound,
    #[error("digest KV store contains a duplicate or self-referencing hash")]
    DuplicateHash,
    #[error("compact block commitment collided; edge was poisoned")]
    DigestCollision,
    #[error("digest KV index capacity would be exceeded")]
    CapacityExceeded,
    #[error("digest KV lookup work budget was exceeded")]
    LookupBudgetExceeded,
    #[error("snapshot digest contract does not match this index")]
    SnapshotDigestMismatch,
    #[error("snapshot group is absent or is not indexable")]
    SnapshotGroupNotFound,
    #[error("snapshot reset scope is unsupported by this index")]
    InvalidSnapshotScope,
    #[error("snapshot contains an unsupported indexed-group layout")]
    UnsupportedSnapshotGroups,
    #[error("snapshot record is invalid for the selected group")]
    InvalidSnapshotRecord,
    #[error("snapshot index construction was cancelled")]
    Cancelled,
}

impl DigestIndexError {
    #[must_use]
    pub const fn reason(&self) -> &'static str {
        match self {
            Self::BlockDigest => "digest_construction_failed",
            Self::InconsistentBlockShape => "digest_inconsistent_block_shape",
            Self::ParentNotFound => "digest_parent_not_found",
            Self::DuplicateHash => "digest_duplicate_hash",
            Self::DigestCollision => "digest_collision",
            Self::CapacityExceeded => "digest_capacity_exceeded",
            Self::LookupBudgetExceeded => "digest_lookup_budget_exceeded",
            Self::SnapshotDigestMismatch => "digest_snapshot_contract_mismatch",
            Self::SnapshotGroupNotFound => "digest_snapshot_group_not_found",
            Self::InvalidSnapshotScope => "digest_snapshot_scope_invalid",
            Self::UnsupportedSnapshotGroups => "digest_snapshot_groups_unsupported",
            Self::InvalidSnapshotRecord => "digest_snapshot_invalid_record",
            Self::Cancelled => "digest_snapshot_cancelled",
        }
    }
}

impl From<BlockDigestError> for DigestIndexError {
    fn from(_: BlockDigestError) -> Self {
        Self::BlockDigest
    }
}

#[derive(Clone, Copy, Debug)]
enum ChildEntry {
    Unique {
        guard: [u8; 16],
        node: usize,
    },
    /// A collision is generation-fatal. Preserve the original arena edge so
    /// removal cannot silently make the key routeable again.
    Poisoned {
        node: usize,
    },
}

#[derive(Debug)]
struct Node {
    parent: Option<usize>,
    block: Option<PrimaryCommitment>,
    children: HashMap<PrimaryCommitment, ChildEntry>,
    child_lengths: Vec<usize>,
    external_hash: Option<ExternalBlockHash>,
    present: bool,
    logical_token_ids: usize,
}

impl Node {
    fn root() -> Self {
        Self {
            parent: None,
            block: None,
            children: HashMap::new(),
            child_lengths: Vec::new(),
            external_hash: None,
            present: true,
            logical_token_ids: 0,
        }
    }
}

type CommitFn = fn(&BlockDigester, &[u32]) -> Result<BlockCommitment, DigestIndexError>;

#[derive(Debug)]
pub struct DigestKvIndex {
    limits: DigestIndexLimits,
    digester: Arc<BlockDigester>,
    commit: CommitFn,
    nodes: Vec<Option<Node>>,
    free_nodes: Vec<usize>,
    by_external_hash: HashMap<ExternalBlockHash, usize>,
    live_nodes: usize,
    logical_token_ids: usize,
    external_hash_bytes: usize,
    poisoned_edges: usize,
}

impl DigestKvIndex {
    /// Construct an empty index from an exact 32-byte protected secret.
    ///
    /// # Errors
    ///
    /// Returns [`DigestIndexError::BlockDigest`] unless `secret` is exactly 32
    /// bytes. The secret is never exposed by this index's debug output.
    pub fn from_secret(limits: DigestIndexLimits, secret: &[u8]) -> Result<Self, DigestIndexError> {
        let digester = BlockDigester::from_slice(secret)?;
        Ok(Self::new(limits, Arc::new(digester)))
    }

    #[must_use]
    pub(crate) fn new(limits: DigestIndexLimits, digester: Arc<BlockDigester>) -> Self {
        Self::with_commit(limits, digester, commit_block)
    }

    fn with_commit(
        limits: DigestIndexLimits,
        digester: Arc<BlockDigester>,
        commit: CommitFn,
    ) -> Self {
        Self {
            limits,
            digester,
            commit,
            nodes: vec![Some(Node::root())],
            free_nodes: Vec::new(),
            by_external_hash: HashMap::new(),
            live_nodes: 0,
            logical_token_ids: 0,
            external_hash_bytes: 0,
            poisoned_edges: 0,
        }
    }

    pub fn clear(&mut self) {
        self.nodes.clear();
        self.nodes.push(Some(Node::root()));
        self.free_nodes.clear();
        self.by_external_hash.clear();
        self.live_nodes = 0;
        self.logical_token_ids = 0;
        self.external_hash_bytes = 0;
        self.poisoned_edges = 0;
    }

    #[must_use]
    pub fn stats(&self) -> DigestIndexStats {
        DigestIndexStats {
            nodes: self.live_nodes,
            logical_token_ids: self.logical_token_ids,
            external_hashes: self.by_external_hash.len(),
            external_hash_bytes: self.external_hash_bytes,
            commitment_bytes: self.live_nodes.saturating_mul(DIGEST_BYTES),
            poisoned_edges: self.poisoned_edges,
        }
    }

    /// Add one decoded and already-filtered live store event.
    ///
    /// Capacity and shape errors are atomic. A detected commitment collision
    /// deliberately poisons the edge before returning an error, making the
    /// affected generation fail closed.
    ///
    /// # Errors
    ///
    /// Returns [`DigestIndexError`] when the event is inconsistent, conflicts
    /// with existing state, or exceeds a configured resource bound.
    pub fn store(&mut self, event: &BlockStored) -> Result<usize, DigestIndexError> {
        if event.block_size == 0
            || event.token_ids.is_empty()
            || event.block_hashes.is_empty()
            || event.token_ids.len().div_ceil(event.block_size) != event.block_hashes.len()
        {
            return Err(DigestIndexError::InconsistentBlockShape);
        }
        self.validate_external_hashes(&event.block_hashes)?;
        let parent_id = event
            .parent_block_hash
            .as_ref()
            .map_or(Ok(ROOT_NODE), |parent| {
                self.by_external_hash
                    .get(parent)
                    .copied()
                    .ok_or(DigestIndexError::ParentNotFound)
            })?;

        let mut seen = HashSet::with_capacity(event.block_hashes.len() + 1);
        if let Some(parent) = event.parent_block_hash.as_ref() {
            seen.insert(parent);
        }
        if event.block_hashes.iter().any(|hash| !seen.insert(hash)) {
            return Err(DigestIndexError::DuplicateHash);
        }

        let chunks = event.token_ids.chunks(event.block_size).collect::<Vec<_>>();
        let commitments = chunks
            .iter()
            .map(|chunk| (self.commit)(&self.digester, chunk))
            .collect::<Result<Vec<_>, _>>()?;
        let mut cursor = parent_id;
        let mut first_new = commitments.len();
        let mut existing_path = Vec::with_capacity(commitments.len());

        for (position, (commitment, external_hash)) in
            commitments.iter().zip(&event.block_hashes).enumerate()
        {
            let entry = self.node(cursor).children.get(&commitment.primary).copied();
            let child_id = match entry {
                None => {
                    first_new = position;
                    break;
                }
                Some(ChildEntry::Poisoned { .. }) => {
                    return Err(DigestIndexError::DigestCollision);
                }
                Some(ChildEntry::Unique { guard, node }) if guard == commitment.guard => node,
                Some(ChildEntry::Unique { .. }) => {
                    self.poison(cursor, commitment.primary);
                    return Err(DigestIndexError::DigestCollision);
                }
            };
            let child = self.node(child_id);
            if child.external_hash.as_ref() != Some(external_hash) {
                self.poison(cursor, commitment.primary);
                return Err(DigestIndexError::DigestCollision);
            }
            cursor = child_id;
            existing_path.push(child_id);
        }

        for (position, hash) in event.block_hashes.iter().enumerate() {
            if let Some(existing) = self.by_external_hash.get(hash)
                && existing_path.get(position) != Some(existing)
            {
                return Err(DigestIndexError::DigestCollision);
            }
        }

        let new_nodes = commitments.len().saturating_sub(first_new);
        let new_token_ids = chunks[first_new..]
            .iter()
            .try_fold(0_usize, |total, chunk| total.checked_add(chunk.len()))
            .ok_or(DigestIndexError::CapacityExceeded)?;
        let new_hash_bytes = event.block_hashes[first_new..]
            .iter()
            .try_fold(0_usize, |total, hash| {
                total.checked_add(external_hash_len(hash))
            })
            .ok_or(DigestIndexError::CapacityExceeded)?;
        self.ensure_capacity(new_nodes, new_token_ids, new_hash_bytes)?;

        cursor = parent_id;
        for ((commitment, external_hash), chunk) in
            commitments.into_iter().zip(&event.block_hashes).zip(chunks)
        {
            let child_id = match self.node(cursor).children.get(&commitment.primary).copied() {
                Some(ChildEntry::Unique { node, .. }) => node,
                Some(ChildEntry::Poisoned { .. }) => {
                    return Err(DigestIndexError::DigestCollision);
                }
                None => self.insert_child(cursor, commitment, external_hash, chunk.len()),
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
        let mut removed = 0_usize;
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

    /// Return the longest cached prefix without retaining the request tokens.
    ///
    /// # Errors
    ///
    /// Returns [`DigestIndexError::LookupBudgetExceeded`] when ambiguous block
    /// geometries exceed the configured fail-closed work budget.
    pub fn find_longest(&self, token_ids: &[u32]) -> Result<DigestMatch, DigestIndexError> {
        let mut best = DigestMatch::default();
        let mut stack = vec![(ROOT_NODE, 0_usize, 0_usize)];
        let mut steps = 0_usize;
        while let Some((node_id, offset, blocks)) = stack.pop() {
            if offset > best.token_ids || (offset == best.token_ids && blocks > best.blocks) {
                best = DigestMatch {
                    blocks,
                    token_ids: offset,
                };
            }
            let node = self.node(node_id);
            for &length in &node.child_lengths {
                steps = steps.saturating_add(1);
                if steps > self.limits.max_lookup_steps {
                    return Err(DigestIndexError::LookupBudgetExceeded);
                }
                let Some(end) = offset
                    .checked_add(length)
                    .filter(|end| *end <= token_ids.len())
                else {
                    continue;
                };
                let commitment = (self.commit)(&self.digester, &token_ids[offset..end])?;
                let Some(ChildEntry::Unique { guard, node }) =
                    node.children.get(&commitment.primary).copied()
                else {
                    continue;
                };
                if guard == commitment.guard && self.node(node).present {
                    stack.push((node, end, blocks + 1));
                }
            }
        }
        Ok(best)
    }

    /// Transactionally replace this index with one selected group from an
    /// already decoded and validated snapshot.
    ///
    /// The digest contract and all index-owned resource bounds are rechecked.
    /// Existing state is preserved on every error.
    ///
    /// # Errors
    ///
    /// Returns [`DigestIndexError`] if the group or digest contract does not
    /// match, a defensive record check fails, or index capacity is exceeded.
    pub fn replace_from_snapshot(
        &mut self,
        body: &SnapshotBody,
        group: SnapshotGroupKey,
    ) -> Result<usize, DigestIndexError> {
        self.replace_from_snapshot_with_cancel(body, group, || false)
    }

    /// Cancellation-aware variant of [`Self::replace_from_snapshot`].
    ///
    /// Cancellation is checked once per record and preserves the old index.
    ///
    /// # Errors
    ///
    /// Returns [`DigestIndexError::Cancelled`] when `cancelled` requests an
    /// early stop, in addition to the snapshot import errors above.
    pub fn replace_from_snapshot_with_cancel(
        &mut self,
        body: &SnapshotBody,
        group: SnapshotGroupKey,
        mut cancelled: impl FnMut() -> bool,
    ) -> Result<usize, DigestIndexError> {
        if body.digest.algorithm != DigestAlgorithm::HmacSha256V1
            || usize::from(body.digest.digest_bytes) != DIGEST_BYTES
            || !self.digester.key_id().matches_wire(&body.digest.key_id)
        {
            return Err(DigestIndexError::SnapshotDigestMismatch);
        }
        if body.reset_scope.kind != ResetKind::FullEngine
            || body.reset_scope.data_parallel_rank.is_some()
            || body.reset_scope.group_idx.is_some()
        {
            return Err(DigestIndexError::InvalidSnapshotScope);
        }
        if body.records.len() > self.limits.max_nodes {
            return Err(DigestIndexError::CapacityExceeded);
        }
        let mut indexed_groups = body
            .groups
            .iter()
            .enumerate()
            .filter(|(_, candidate)| candidate.disposition == GroupDisposition::Indexed);
        let Some((group_slot, only_group)) = indexed_groups.next() else {
            return Err(DigestIndexError::UnsupportedSnapshotGroups);
        };
        if indexed_groups.next().is_some() {
            return Err(DigestIndexError::UnsupportedSnapshotGroups);
        }
        if only_group.data_parallel_rank != group.data_parallel_rank
            || only_group.group_idx != group.group_idx
        {
            return Err(DigestIndexError::SnapshotGroupNotFound);
        }
        let mut scratch = Self::new(self.limits, self.digester.clone());
        let mut record_nodes = vec![None; body.records.len()];
        let mut external_hashes = HashSet::with_capacity(body.records.len());
        let mut imported = 0_usize;
        for (record_index, record) in body.records.iter().enumerate() {
            if cancelled() {
                return Err(DigestIndexError::Cancelled);
            }
            if usize::try_from(record.group_slot).ok() != Some(group_slot) {
                continue;
            }
            let parent_id = match record.parent_record {
                Some(parent) => {
                    let parent_index = usize::try_from(parent)
                        .ok()
                        .filter(|parent| *parent < record_index)
                        .ok_or(DigestIndexError::InvalidSnapshotRecord)?;
                    record_nodes[parent_index].ok_or(DigestIndexError::InvalidSnapshotRecord)?
                }
                None => ROOT_NODE,
            };
            if record.block_token_ids == 0 || record.block_digest.len() != DIGEST_BYTES {
                return Err(DigestIndexError::InvalidSnapshotRecord);
            }
            let digest: [u8; DIGEST_BYTES] = record
                .block_digest
                .as_slice()
                .try_into()
                .map_err(|_| DigestIndexError::InvalidSnapshotRecord)?;
            let external_hash = snapshot_hash(&record.external_hash)?;
            scratch.validate_external_hashes(std::slice::from_ref(&external_hash))?;
            if !external_hashes.insert(external_hash.clone()) {
                return Err(DigestIndexError::DuplicateHash);
            }
            let commitment = commitment_from_digest(record.block_token_ids, digest);
            let node_id = scratch.insert_snapshot_child(
                parent_id,
                commitment,
                external_hash,
                record.present,
            )?;
            record_nodes[record_index] = Some(node_id);
            imported = imported
                .checked_add(1)
                .ok_or(DigestIndexError::CapacityExceeded)?;
        }
        *self = scratch;
        Ok(imported)
    }

    fn insert_snapshot_child(
        &mut self,
        parent_id: usize,
        commitment: BlockCommitment,
        external_hash: ExternalBlockHash,
        present: bool,
    ) -> Result<usize, DigestIndexError> {
        if self.by_external_hash.contains_key(&external_hash) {
            return Err(DigestIndexError::DuplicateHash);
        }
        match self
            .node(parent_id)
            .children
            .get(&commitment.primary)
            .copied()
        {
            Some(ChildEntry::Poisoned { .. }) => Err(DigestIndexError::DigestCollision),
            Some(ChildEntry::Unique { guard, .. }) if guard != commitment.guard => {
                self.poison(parent_id, commitment.primary);
                Err(DigestIndexError::DigestCollision)
            }
            Some(ChildEntry::Unique { .. }) => {
                self.poison(parent_id, commitment.primary);
                Err(DigestIndexError::DigestCollision)
            }
            None => {
                let logical_token_ids = usize::try_from(commitment.primary.token_count)
                    .map_err(|_| DigestIndexError::CapacityExceeded)?;
                let hash_bytes = external_hash_len(&external_hash);
                self.ensure_capacity(1, logical_token_ids, hash_bytes)?;
                let node_id =
                    self.insert_child(parent_id, commitment, &external_hash, logical_token_ids);
                self.node_mut(node_id).present = present;
                if present {
                    self.by_external_hash.insert(external_hash, node_id);
                }
                Ok(node_id)
            }
        }
    }

    fn validate_external_hashes(
        &self,
        hashes: &[ExternalBlockHash],
    ) -> Result<(), DigestIndexError> {
        if hashes.iter().any(|hash| {
            let bytes = external_hash_len(hash);
            bytes == 0 || bytes > self.limits.max_external_hash_bytes
        }) {
            return Err(DigestIndexError::CapacityExceeded);
        }
        Ok(())
    }

    fn ensure_capacity(
        &self,
        new_nodes: usize,
        new_token_ids: usize,
        new_hash_bytes: usize,
    ) -> Result<(), DigestIndexError> {
        if self.live_nodes.saturating_add(new_nodes) > self.limits.max_nodes
            || self.logical_token_ids.saturating_add(new_token_ids)
                > self.limits.max_logical_token_ids
            || self.external_hash_bytes.saturating_add(new_hash_bytes)
                > self.limits.max_total_external_hash_bytes
        {
            return Err(DigestIndexError::CapacityExceeded);
        }
        Ok(())
    }

    fn poison(&mut self, parent_id: usize, primary: PrimaryCommitment) {
        let entry = self.node(parent_id).children.get(&primary).copied();
        if let Some(ChildEntry::Unique { node, .. }) = entry {
            self.node_mut(parent_id)
                .children
                .insert(primary, ChildEntry::Poisoned { node });
            self.poisoned_edges = self.poisoned_edges.saturating_add(1);
        }
    }

    fn insert_child(
        &mut self,
        parent_id: usize,
        commitment: BlockCommitment,
        external_hash: &ExternalBlockHash,
        logical_token_ids: usize,
    ) -> usize {
        let node_id = self.allocate_node(Node {
            parent: Some(parent_id),
            block: Some(commitment.primary),
            children: HashMap::new(),
            child_lengths: Vec::new(),
            external_hash: Some(external_hash.clone()),
            present: true,
            logical_token_ids,
        });
        let parent = self.node_mut(parent_id);
        parent.children.insert(
            commitment.primary,
            ChildEntry::Unique {
                guard: commitment.guard,
                node: node_id,
            },
        );
        let length = usize::try_from(commitment.primary.token_count).expect("u32 fits usize");
        if !parent.child_lengths.contains(&length) {
            parent.child_lengths.push(length);
            parent
                .child_lengths
                .sort_unstable_by(|left, right| right.cmp(left));
        }
        self.live_nodes += 1;
        self.logical_token_ids += logical_token_ids;
        self.external_hash_bytes += external_hash_len(external_hash);
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
            let node = self.node(node_id);
            if node.present || !node.children.is_empty() {
                break;
            }
            let parent_id = node.parent.expect("non-root digest node has parent");
            let block = node.block.expect("non-root digest node has block key");
            if matches!(
                self.node(parent_id).children.get(&block),
                Some(ChildEntry::Poisoned { node: poisoned }) if *poisoned == node_id
            ) {
                break;
            }
            let node = self.nodes[node_id].take().expect("live digest node");
            self.node_mut(parent_id).children.remove(&block);
            self.live_nodes -= 1;
            self.logical_token_ids -= node.logical_token_ids;
            self.external_hash_bytes = self
                .external_hash_bytes
                .saturating_sub(node.external_hash.as_ref().map_or(0, external_hash_len));
            self.free_nodes.push(node_id);
            node_id = parent_id;
        }
    }

    fn node(&self, node_id: usize) -> &Node {
        self.nodes[node_id].as_ref().expect("live digest node")
    }

    fn node_mut(&mut self, node_id: usize) -> &mut Node {
        self.nodes[node_id].as_mut().expect("live digest node")
    }
}

fn commit_block(
    digester: &BlockDigester,
    token_ids: &[u32],
) -> Result<BlockCommitment, DigestIndexError> {
    digester.commit(token_ids).map_err(Into::into)
}

fn commitment_from_digest(token_count: u32, digest: [u8; DIGEST_BYTES]) -> BlockCommitment {
    let mut primary = [0_u8; 16];
    let mut guard = [0_u8; 16];
    primary.copy_from_slice(&digest[..16]);
    guard.copy_from_slice(&digest[16..]);
    BlockCommitment {
        primary: PrimaryCommitment {
            token_count,
            digest: primary,
        },
        guard,
    }
}

fn snapshot_hash(hash: &SnapshotBlockHash) -> Result<ExternalBlockHash, DigestIndexError> {
    match hash {
        SnapshotBlockHash::Bytes(bytes) if !bytes.is_empty() => {
            Ok(ExternalBlockHash::Bytes(ByteBuf::from(bytes.to_vec())))
        }
        SnapshotBlockHash::Bytes(_) => Err(DigestIndexError::InvalidSnapshotRecord),
        SnapshotBlockHash::Signed(value) => Ok(ExternalBlockHash::Signed(*value)),
        SnapshotBlockHash::Unsigned(value) => Ok(ExternalBlockHash::Unsigned(*value)),
    }
}

fn external_hash_len(hash: &ExternalBlockHash) -> usize {
    match hash {
        ExternalBlockHash::Bytes(bytes) => bytes.len(),
        ExternalBlockHash::Signed(_) | ExternalBlockHash::Unsigned(_) => size_of::<u64>(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kv_snapshot::{
        AttentionKind, DigestRecord, DigestSpec, EngineIncarnation, GroupMetadata, ResetScope,
        SnapshotCapacity,
    };

    const TEST_SECRET: [u8; DIGEST_BYTES] = *b"0123456789abcdef0123456789abcdef";

    fn digester() -> Arc<BlockDigester> {
        Arc::new(BlockDigester::new(TEST_SECRET))
    }

    fn limits() -> DigestIndexLimits {
        DigestIndexLimits {
            max_nodes: 128,
            max_logical_token_ids: 4_096,
            max_lookup_steps: 128,
            max_external_hash_bytes: 256,
            max_total_external_hash_bytes: 4_096,
        }
    }

    fn index() -> DigestKvIndex {
        DigestKvIndex::new(limits(), digester())
    }

    fn store_event(
        hashes: &[u64],
        parent: Option<u64>,
        tokens: &[u32],
        block_size: usize,
    ) -> BlockStored {
        BlockStored {
            block_hashes: hashes
                .iter()
                .copied()
                .map(ExternalBlockHash::Unsigned)
                .collect(),
            parent_block_hash: parent.map(ExternalBlockHash::Unsigned),
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
            block_hashes: hashes
                .iter()
                .copied()
                .map(ExternalBlockHash::Unsigned)
                .collect(),
            group_idx: Some(0),
            medium: Some("GPU".to_owned()),
            locality: Some("LOCAL".to_owned()),
        }
    }

    #[test]
    fn matches_prefix_without_retaining_raw_tokens() {
        let mut index = index();
        index
            .store(&store_event(&[10, 11], None, &[1, 2, 3, 4], 2))
            .unwrap();

        assert_eq!(
            index.find_longest(&[1, 2, 3, 4, 5]).unwrap(),
            DigestMatch {
                blocks: 2,
                token_ids: 4,
            }
        );
        assert_eq!(index.find_longest(&[1, 2, 9]).unwrap().token_ids, 2);
        assert_eq!(index.find_longest(&[1, 7]).unwrap().token_ids, 0);
        assert_eq!(index.stats().commitment_bytes, 64);
    }

    #[test]
    fn supports_variable_geometries_removal_and_reinsertion() {
        let mut index = index();
        index.store(&store_event(&[10], None, &[1, 2], 2)).unwrap();
        index
            .store(&store_event(&[20], None, &[1, 2, 3], 3))
            .unwrap();
        assert_eq!(index.find_longest(&[1, 2, 3]).unwrap().token_ids, 3);
        assert_eq!(index.remove(&remove_event(&[10])), 1);
        assert_eq!(index.find_longest(&[1, 2]).unwrap().token_ids, 0);
        index.store(&store_event(&[10], None, &[1, 2], 2)).unwrap();
        assert_eq!(index.find_longest(&[1, 2]).unwrap().token_ids, 2);
    }

    #[test]
    fn tombstone_hash_memory_is_bounded_until_pruned() {
        let mut index = index();
        index
            .store(&store_event(&[10, 11], None, &[1, 2, 3, 4], 2))
            .unwrap();
        assert_eq!(index.stats().external_hash_bytes, 16);

        index.remove(&remove_event(&[10]));
        assert_eq!(index.stats().external_hashes, 1);
        assert_eq!(index.stats().external_hash_bytes, 16);
        index.store(&store_event(&[10], None, &[1, 2], 2)).unwrap();
        assert_eq!(index.stats().external_hash_bytes, 16);

        index.remove(&remove_event(&[11, 10]));
        assert_eq!(index.stats().external_hash_bytes, 0);
    }

    fn forced_primary_collision(
        digester: &BlockDigester,
        tokens: &[u32],
    ) -> Result<BlockCommitment, DigestIndexError> {
        let mut commitment = digester.commit(tokens)?;
        commitment.primary.digest = [0; 16];
        Ok(commitment)
    }

    #[test]
    fn collision_poison_survives_removal_and_reinsert() {
        let mut index = DigestKvIndex::with_commit(limits(), digester(), forced_primary_collision);
        index.store(&store_event(&[10], None, &[1, 2], 2)).unwrap();
        assert_eq!(
            index.store(&store_event(&[20], None, &[3, 4], 2)),
            Err(DigestIndexError::DigestCollision)
        );
        assert_eq!(index.remove(&remove_event(&[10])), 1);
        assert_eq!(index.stats().poisoned_edges, 1);
        assert_eq!(index.find_longest(&[1, 2]).unwrap().token_ids, 0);
        assert_eq!(
            index.store(&store_event(&[30], None, &[5, 6], 2)),
            Err(DigestIndexError::DigestCollision)
        );
    }

    #[test]
    fn capacity_error_does_not_mutate_existing_state() {
        let mut constrained = limits();
        constrained.max_nodes = 1;
        let mut index = DigestKvIndex::new(constrained, digester());
        index.store(&store_event(&[10], None, &[1, 2], 2)).unwrap();
        let before = index.stats();
        assert_eq!(
            index.store(&store_event(&[20], None, &[3, 4], 2)),
            Err(DigestIndexError::CapacityExceeded)
        );
        assert_eq!(index.stats(), before);
        assert_eq!(index.find_longest(&[1, 2]).unwrap().token_ids, 2);
    }

    fn snapshot_for(tokens: &[&[u32]], hashes: &[u64], key: &BlockDigester) -> SnapshotBody {
        let mut prefix = 0_u64;
        let records = tokens
            .iter()
            .zip(hashes)
            .enumerate()
            .map(|(index, (token_ids, hash))| {
                let commitment = key.commit(token_ids).unwrap();
                let mut digest = Vec::with_capacity(DIGEST_BYTES);
                digest.extend_from_slice(&commitment.primary.digest);
                digest.extend_from_slice(&commitment.guard);
                prefix += u64::try_from(token_ids.len()).unwrap();
                DigestRecord {
                    group_slot: 0,
                    parent_record: index
                        .checked_sub(1)
                        .and_then(|parent| u32::try_from(parent).ok()),
                    external_hash: SnapshotBlockHash::Unsigned(*hash),
                    block_digest: digest,
                    block_token_ids: u32::try_from(token_ids.len()).unwrap(),
                    prefix_token_ids: prefix,
                    present: true,
                }
            })
            .collect::<Vec<_>>();
        SnapshotBody {
            engine_incarnation: EngineIncarnation {
                engine_id: "engine-a".to_owned(),
                model_revision: "revision".to_owned(),
                image_digest: "sha256:image".to_owned(),
                process_started_unix_ns: 1,
                attestation_sha256: vec![1; DIGEST_BYTES],
            },
            watermark: 42,
            reset_scope: ResetScope::full_engine(),
            digest: DigestSpec {
                algorithm: DigestAlgorithm::HmacSha256V1,
                key_id: key.key_id().to_vec(),
                digest_bytes: u16::try_from(DIGEST_BYTES).unwrap(),
            },
            capacity: SnapshotCapacity::default(),
            groups: vec![GroupMetadata {
                data_parallel_rank: 0,
                group_idx: 0,
                attention_kind: AttentionKind::MlaAttention,
                disposition: GroupDisposition::Indexed,
                block_size: 2,
            }],
            records,
        }
    }

    #[test]
    fn snapshot_replacement_builds_lookup_compatible_state() {
        let digester = digester();
        let mut index = DigestKvIndex::new(limits(), digester.clone());
        index.store(&store_event(&[99], None, &[9, 9], 2)).unwrap();
        let snapshot = snapshot_for(&[&[1, 2], &[3, 4]], &[10, 11], &digester);

        assert_eq!(
            index
                .replace_from_snapshot(
                    &snapshot,
                    SnapshotGroupKey {
                        data_parallel_rank: 0,
                        group_idx: 0,
                    },
                )
                .unwrap(),
            2
        );
        assert_eq!(index.find_longest(&[1, 2, 3, 4]).unwrap().token_ids, 4);
        assert_eq!(index.find_longest(&[9, 9]).unwrap().token_ids, 0);
    }

    #[test]
    fn failed_snapshot_replacement_preserves_existing_state() {
        let digester = digester();
        let mut index = DigestKvIndex::new(limits(), digester.clone());
        index.store(&store_event(&[99], None, &[9, 9], 2)).unwrap();
        let mut snapshot = snapshot_for(&[&[1, 2]], &[10], &digester);
        snapshot.digest.key_id[0] ^= 1;

        assert_eq!(
            index.replace_from_snapshot(
                &snapshot,
                SnapshotGroupKey {
                    data_parallel_rank: 0,
                    group_idx: 0,
                },
            ),
            Err(DigestIndexError::SnapshotDigestMismatch)
        );
        assert_eq!(index.find_longest(&[9, 9]).unwrap().token_ids, 2);
    }

    #[test]
    fn snapshot_import_is_cancelled_atomically() {
        let digester = digester();
        let mut index = DigestKvIndex::new(limits(), digester.clone());
        index.store(&store_event(&[99], None, &[9, 9], 2)).unwrap();
        let snapshot = snapshot_for(&[&[1, 2], &[3, 4]], &[10, 11], &digester);
        let mut checks = 0;

        assert_eq!(
            index.replace_from_snapshot_with_cancel(
                &snapshot,
                SnapshotGroupKey {
                    data_parallel_rank: 0,
                    group_idx: 0,
                },
                || {
                    checks += 1;
                    checks > 1
                },
            ),
            Err(DigestIndexError::Cancelled)
        );
        assert_eq!(index.find_longest(&[9, 9]).unwrap().token_ids, 2);
        assert_eq!(index.find_longest(&[1, 2]).unwrap().token_ids, 0);
    }

    #[test]
    fn v1_snapshot_import_rejects_multiple_indexed_groups() {
        let digester = digester();
        let mut index = DigestKvIndex::new(limits(), digester.clone());
        let mut snapshot = snapshot_for(&[&[1, 2]], &[10], &digester);
        snapshot.groups.push(GroupMetadata {
            data_parallel_rank: 0,
            group_idx: 1,
            attention_kind: AttentionKind::MlaAttention,
            disposition: GroupDisposition::Indexed,
            block_size: 2,
        });

        assert_eq!(
            index.replace_from_snapshot(
                &snapshot,
                SnapshotGroupKey {
                    data_parallel_rank: 0,
                    group_idx: 0,
                },
            ),
            Err(DigestIndexError::UnsupportedSnapshotGroups)
        );
    }

    #[test]
    fn removed_snapshot_parent_preserves_but_fences_live_descendant() {
        let digester = digester();
        let mut index = DigestKvIndex::new(limits(), digester.clone());
        let mut snapshot = snapshot_for(&[&[1, 2], &[3, 4]], &[10, 11], &digester);
        snapshot.records[0].present = false;

        index
            .replace_from_snapshot(
                &snapshot,
                SnapshotGroupKey {
                    data_parallel_rank: 0,
                    group_idx: 0,
                },
            )
            .unwrap();
        assert_eq!(index.stats().nodes, 2);
        assert_eq!(index.stats().external_hashes, 1);
        assert_eq!(index.find_longest(&[1, 2, 3, 4]).unwrap().token_ids, 0);

        index.store(&store_event(&[10], None, &[1, 2], 2)).unwrap();
        assert_eq!(index.find_longest(&[1, 2, 3, 4]).unwrap().token_ids, 4);
    }

    #[test]
    fn lookup_budget_fails_closed() {
        let mut constrained = limits();
        constrained.max_lookup_steps = 1;
        let mut index = DigestKvIndex::new(constrained, digester());
        index.store(&store_event(&[10], None, &[1, 2], 2)).unwrap();
        index
            .store(&store_event(&[20], None, &[1, 2, 3], 3))
            .unwrap();
        assert_eq!(
            index.find_longest(&[1, 2, 3]),
            Err(DigestIndexError::LookupBudgetExceeded)
        );
    }

    #[test]
    fn debug_redacts_digest_key() {
        let digester = BlockDigester::new(TEST_SECRET);
        let debug = format!("{digester:?}");
        assert!(debug.contains("REDACTED"));
        assert!(!debug.contains("0123456789abcdef"));
    }
}
