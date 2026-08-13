//! Prototype for a compact, token-vector-free exact KV prefix index.
//!
//! Run the focused tests with:
//! `cargo test --locked --example digest_index_prototype`
//! and the small synthetic benchmark with:
//! `cargo run --release --locked --example digest_index_prototype`.

use std::{
    collections::{HashMap, HashSet},
    hint::black_box,
    time::Instant,
};

use mini_dynamo::kv_wire::{BlockRemoved, BlockStored, ExternalBlockHash};
use sha2::{Digest, Sha256};
use thiserror::Error;

const ROOT: usize = 0;
const DIGEST_BYTES_PER_BLOCK: usize = 32;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct PrimaryKey {
    token_count: u32,
    digest: [u8; 16],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct BlockCommitment {
    primary: PrimaryKey,
    guard: [u8; 16],
}

#[derive(Clone, Copy, Debug)]
enum ChildEntry {
    Unique {
        guard: [u8; 16],
        node: usize,
    },
    /// At least two different engine identities claimed this compact key.
    /// Lookups never traverse a poisoned entry.
    Poisoned {
        /// Preserve the original arena edge so reverse removal and accounting
        /// never point at an orphan. The poisoned edge remains unrouteable
        /// until the whole generation is discarded.
        node: usize,
    },
}

#[derive(Debug)]
struct Node {
    parent: Option<usize>,
    block: Option<PrimaryKey>,
    children: HashMap<PrimaryKey, ChildEntry>,
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

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct DigestMatch {
    blocks: usize,
    token_ids: usize,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct DigestStats {
    nodes: usize,
    logical_token_ids: usize,
    commitment_bytes: usize,
    poisoned_edges: usize,
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
enum DigestIndexError {
    #[error("store has inconsistent block and token counts")]
    InconsistentBlockShape,
    #[error("store references an unknown parent")]
    ParentNotFound,
    #[error("store contains a duplicate or self-referencing external hash")]
    DuplicateHash,
    #[error("compact block commitment collided; edge was poisoned")]
    DigestCollision,
    #[error("index capacity would be exceeded")]
    CapacityExceeded,
    #[error("lookup work budget was exceeded")]
    LookupBudgetExceeded,
}

type CommitFn = fn(&[u32]) -> BlockCommitment;

/// Arena-backed prototype. No raw token vector survives `store`.
#[derive(Debug)]
struct DigestKvIndex {
    nodes: Vec<Option<Node>>,
    free_nodes: Vec<usize>,
    by_external_hash: HashMap<ExternalBlockHash, usize>,
    live_nodes: usize,
    logical_token_ids: usize,
    poisoned_edges: usize,
    max_nodes: usize,
    max_logical_token_ids: usize,
    max_lookup_steps: usize,
    commit: CommitFn,
}

impl DigestKvIndex {
    fn new(max_nodes: usize, max_logical_token_ids: usize, max_lookup_steps: usize) -> Self {
        Self::with_commit(
            max_nodes,
            max_logical_token_ids,
            max_lookup_steps,
            sha256_commitment,
        )
    }

    fn with_commit(
        max_nodes: usize,
        max_logical_token_ids: usize,
        max_lookup_steps: usize,
        commit: CommitFn,
    ) -> Self {
        Self {
            nodes: vec![Some(Node::root())],
            free_nodes: Vec::new(),
            by_external_hash: HashMap::new(),
            live_nodes: 0,
            logical_token_ids: 0,
            poisoned_edges: 0,
            max_nodes,
            max_logical_token_ids,
            max_lookup_steps,
            commit,
        }
    }

    fn stats(&self) -> DigestStats {
        DigestStats {
            nodes: self.live_nodes,
            logical_token_ids: self.logical_token_ids,
            commitment_bytes: self.live_nodes.saturating_mul(DIGEST_BYTES_PER_BLOCK),
            poisoned_edges: self.poisoned_edges,
        }
    }

    fn store(&mut self, event: &BlockStored) -> Result<usize, DigestIndexError> {
        if event.block_size == 0
            || event.token_ids.is_empty()
            || event.block_hashes.is_empty()
            || event.token_ids.len().div_ceil(event.block_size) != event.block_hashes.len()
        {
            return Err(DigestIndexError::InconsistentBlockShape);
        }
        let parent_id = event
            .parent_block_hash
            .as_ref()
            .map_or(Ok(ROOT), |parent| {
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
            .map(|chunk| (self.commit)(chunk))
            .collect::<Vec<_>>();
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
            // Without raw tokens, a different external identity for the same
            // full commitment is indistinguishable from a 256-bit collision.
            // Poisoning preserves the fail-closed routing contract.
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
            .try_fold(0usize, |total, chunk| total.checked_add(chunk.len()))
            .ok_or(DigestIndexError::CapacityExceeded)?;
        if self.live_nodes.saturating_add(new_nodes) > self.max_nodes
            || self.logical_token_ids.saturating_add(new_token_ids) > self.max_logical_token_ids
        {
            return Err(DigestIndexError::CapacityExceeded);
        }

        cursor = parent_id;
        for ((commitment, external_hash), chunk) in
            commitments.into_iter().zip(&event.block_hashes).zip(chunks)
        {
            let child_id = match self.node(cursor).children.get(&commitment.primary).copied() {
                Some(ChildEntry::Unique { node, .. }) => node,
                Some(ChildEntry::Poisoned { .. }) => {
                    return Err(DigestIndexError::DigestCollision);
                }
                None => self.insert_child(cursor, commitment, external_hash.clone(), chunk.len()),
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

    fn remove(&mut self, event: &BlockRemoved) -> usize {
        let mut removed = 0;
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

    fn find_longest(&self, token_ids: &[u32]) -> Result<DigestMatch, DigestIndexError> {
        let mut best = DigestMatch::default();
        let mut stack = vec![(ROOT, 0_usize, 0_usize)];
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
                if steps > self.max_lookup_steps {
                    return Err(DigestIndexError::LookupBudgetExceeded);
                }
                let Some(end) = offset
                    .checked_add(length)
                    .filter(|end| *end <= token_ids.len())
                else {
                    continue;
                };
                let commitment = (self.commit)(&token_ids[offset..end]);
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

    fn poison(&mut self, parent_id: usize, primary: PrimaryKey) {
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
        external_hash: ExternalBlockHash,
        logical_token_ids: usize,
    ) -> usize {
        let node_id = self.allocate_node(Node {
            parent: Some(parent_id),
            block: Some(commitment.primary),
            children: HashMap::new(),
            child_lengths: Vec::new(),
            external_hash: Some(external_hash),
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
        while node_id != ROOT {
            let should_prune = {
                let node = self.node(node_id);
                !node.present && node.children.is_empty()
            };
            if !should_prune {
                break;
            }
            let node = self.nodes[node_id].take().expect("live digest node");
            let parent_id = node.parent.expect("non-root digest node has parent");
            let block = node.block.expect("non-root digest node has block key");
            if matches!(
                self.node(parent_id).children.get(&block),
                Some(ChildEntry::Poisoned { node: poisoned }) if *poisoned == node_id
            ) {
                // A detected digest conflict is generation-fatal. Preserve the
                // arena edge and its accounting even after the original
                // external identity is removed, so the compact key cannot
                // silently become routeable again.
                break;
            }
            self.node_mut(parent_id).children.remove(&block);
            self.live_nodes -= 1;
            self.logical_token_ids -= node.logical_token_ids;
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

fn sha256_commitment(tokens: &[u32]) -> BlockCommitment {
    let mut hasher = Sha256::new();
    hasher.update(b"mini-dynamo:block-commitment:v1\0");
    hasher.update(
        u64::try_from(tokens.len())
            .expect("token slice length fits u64")
            .to_le_bytes(),
    );
    for token in tokens {
        hasher.update(token.to_le_bytes());
    }
    let digest: [u8; 32] = hasher.finalize().into();
    let mut primary = [0_u8; 16];
    let mut guard = [0_u8; 16];
    primary.copy_from_slice(&digest[..16]);
    guard.copy_from_slice(&digest[16..]);
    BlockCommitment {
        primary: PrimaryKey {
            token_count: u32::try_from(tokens.len()).expect("KV block length fits u32"),
            digest: primary,
        },
        guard,
    }
}

fn main() {
    const BLOCK_SIZE: usize = 256;
    const BLOCKS: usize = 2_048;
    let tokens = (0..BLOCKS * BLOCK_SIZE)
        .map(|value| u32::try_from(value % 129_280).expect("synthetic token"))
        .collect::<Vec<_>>();
    let event = store_event(
        &(1..=u64::try_from(BLOCKS).expect("block count")).collect::<Vec<_>>(),
        None,
        &tokens,
        BLOCK_SIZE,
    );
    let mut index = DigestKvIndex::new(BLOCKS + 1, tokens.len(), BLOCKS * 2);
    let started = Instant::now();
    index.store(&event).expect("synthetic store");
    let build_ms = started.elapsed().as_secs_f64() * 1_000.0;
    let started = Instant::now();
    for _ in 0..100 {
        black_box(index.find_longest(&tokens).expect("synthetic lookup"));
    }
    let lookup_us = started.elapsed().as_secs_f64() * 10_000.0;
    let stats = index.stats();
    println!(
        "blocks={} logical_token_bytes={} commitment_bytes={} build_ms={build_ms:.2} lookup_us={lookup_us:.2}",
        stats.nodes,
        stats.logical_token_ids * size_of::<u32>(),
        stats.commitment_bytes,
    );
    black_box(index.remove(&remove_event(&[
        u64::try_from(BLOCKS).expect("block count"),
    ])));
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

#[cfg(test)]
mod tests {
    use super::*;

    fn index() -> DigestKvIndex {
        DigestKvIndex::new(128, 4_096, 128)
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
                token_ids: 4
            }
        );
        assert_eq!(index.find_longest(&[1, 2, 9]).unwrap().token_ids, 2);
        assert_eq!(index.find_longest(&[1, 7]).unwrap().token_ids, 0);
        assert!(
            index
                .nodes
                .iter()
                .flatten()
                .all(|node| { node.logical_token_ids == 0 || node.block.is_some() })
        );
        assert_eq!(index.stats().commitment_bytes, 64);
    }

    #[test]
    fn supports_variable_block_geometries_at_one_parent() {
        let mut index = index();
        index.store(&store_event(&[10], None, &[1, 2], 2)).unwrap();
        index
            .store(&store_event(&[20], None, &[1, 2, 3], 3))
            .unwrap();

        assert_eq!(index.find_longest(&[1, 2]).unwrap().token_ids, 2);
        assert_eq!(index.find_longest(&[1, 2, 3]).unwrap().token_ids, 3);
    }

    #[test]
    fn identical_blocks_are_scoped_by_parent_identity() {
        let mut index = index();
        index.store(&store_event(&[10], None, &[1, 2], 2)).unwrap();
        index.store(&store_event(&[20], None, &[9, 9], 2)).unwrap();
        index
            .store(&store_event(&[11], Some(10), &[3, 4], 2))
            .unwrap();
        index
            .store(&store_event(&[21], Some(20), &[3, 4], 2))
            .unwrap();

        assert_eq!(index.find_longest(&[1, 2, 3, 4]).unwrap().token_ids, 4);
        assert_eq!(index.find_longest(&[9, 9, 3, 4]).unwrap().token_ids, 4);
    }

    #[test]
    fn removal_uses_external_identity_and_preserves_descendants() {
        let mut index = index();
        index
            .store(&store_event(&[10, 11], None, &[1, 2, 3, 4], 2))
            .unwrap();
        assert_eq!(index.remove(&remove_event(&[99])), 0);
        assert_eq!(index.remove(&remove_event(&[10])), 1);
        assert_eq!(index.find_longest(&[1, 2, 3, 4]).unwrap().token_ids, 0);

        index.store(&store_event(&[10], None, &[1, 2], 2)).unwrap();
        assert_eq!(index.find_longest(&[1, 2, 3, 4]).unwrap().token_ids, 4);
        assert_eq!(index.remove(&remove_event(&[11, 10])), 2);
        assert_eq!(index.stats(), DigestStats::default());
    }

    fn forced_primary_collision(tokens: &[u32]) -> BlockCommitment {
        let mut commitment = sha256_commitment(tokens);
        commitment.primary.digest = [0; 16];
        commitment
    }

    fn forced_full_collision(tokens: &[u32]) -> BlockCommitment {
        BlockCommitment {
            primary: PrimaryKey {
                token_count: u32::try_from(tokens.len()).unwrap(),
                digest: [0; 16],
            },
            guard: [0; 16],
        }
    }

    #[test]
    fn detected_primary_collision_poisons_edge_and_fails_closed() {
        let mut index = DigestKvIndex::with_commit(128, 4_096, 128, forced_primary_collision);
        index.store(&store_event(&[10], None, &[1, 2], 2)).unwrap();
        assert_eq!(
            index.store(&store_event(&[20], None, &[3, 4], 2)),
            Err(DigestIndexError::DigestCollision)
        );

        assert_eq!(index.stats().poisoned_edges, 1);
        assert_eq!(index.find_longest(&[1, 2]).unwrap().token_ids, 0);
        assert_eq!(index.find_longest(&[3, 4]).unwrap().token_ids, 0);
        assert_eq!(index.remove(&remove_event(&[10])), 1);
        assert_eq!(index.stats().poisoned_edges, 1);
        assert_eq!(index.find_longest(&[1, 2]).unwrap().token_ids, 0);
        assert_eq!(
            index.store(&store_event(&[30], None, &[5, 6], 2)),
            Err(DigestIndexError::DigestCollision)
        );
        assert_eq!(index.stats().poisoned_edges, 1);
    }

    #[test]
    fn conflicting_external_identity_poisons_even_full_digest_match() {
        let mut index = DigestKvIndex::with_commit(128, 4_096, 128, forced_full_collision);
        index.store(&store_event(&[10], None, &[1, 2], 2)).unwrap();
        assert_eq!(
            index.store(&store_event(&[20], None, &[3, 4], 2)),
            Err(DigestIndexError::DigestCollision)
        );
        assert_eq!(index.find_longest(&[1, 2]).unwrap().token_ids, 0);
    }

    #[test]
    fn lookup_budget_remains_fail_closed_for_ambiguous_geometries() {
        let mut index = DigestKvIndex::new(128, 4_096, 1);
        index.store(&store_event(&[10], None, &[1, 2], 2)).unwrap();
        index
            .store(&store_event(&[20], None, &[1, 2, 3], 3))
            .unwrap();

        assert_eq!(
            index.find_longest(&[1, 2, 3]),
            Err(DigestIndexError::LookupBudgetExceeded)
        );
    }
}
