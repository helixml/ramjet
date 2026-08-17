use ramjet::{
    digest_index::{DigestIndexLimits, DigestKvIndex},
    exact_index::{ExactIndexLimits, ExactKvIndex},
    kv_wire::{BlockRemoved, BlockStored, ExternalBlockHash},
};

const SECRET: &[u8; 32] = b"0123456789abcdef0123456789abcdef";

fn indexes() -> (ExactKvIndex, DigestKvIndex) {
    (
        ExactKvIndex::new(ExactIndexLimits {
            max_nodes: 4_096,
            max_token_ids: 65_536,
            max_lookup_steps: 4_096,
        }),
        DigestKvIndex::from_secret(
            DigestIndexLimits {
                max_nodes: 4_096,
                max_logical_token_ids: 65_536,
                max_lookup_steps: 4_096,
                max_external_hash_bytes: 256,
                max_total_external_hash_bytes: 1 << 20,
            },
            SECRET,
        )
        .unwrap(),
    )
}

fn store(
    hash: ExternalBlockHash,
    parent: Option<ExternalBlockHash>,
    tokens: &[u32],
) -> BlockStored {
    BlockStored {
        block_hashes: vec![hash],
        parent_block_hash: parent,
        token_ids: tokens.to_vec(),
        block_size: tokens.len(),
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

fn remove(hash: ExternalBlockHash) -> BlockRemoved {
    BlockRemoved {
        block_hashes: vec![hash],
        group_idx: Some(0),
        medium: Some("GPU".to_owned()),
        locality: Some("LOCAL".to_owned()),
    }
}

fn assert_parity(exact: &ExactKvIndex, digest: &DigestKvIndex, query: &[u32]) {
    let exact_match = exact.find_longest(query).unwrap();
    let digest_match = digest.find_longest(query).unwrap();
    assert_eq!(
        (digest_match.blocks, digest_match.token_ids),
        (exact_match.blocks, exact_match.token_ids),
        "digest index must neither overclaim nor underclaim"
    );
    let exact_stats = exact.stats();
    let digest_stats = digest.stats();
    assert_eq!(digest_stats.nodes, exact_stats.nodes);
    assert_eq!(digest_stats.logical_token_ids, exact_stats.token_ids);
    assert_eq!(digest_stats.external_hashes, exact_stats.external_hashes);
}

#[test]
fn exact_parity_for_geometry_hash_identity_and_tombstones() {
    let (mut exact, mut digest) = indexes();
    let cases = [
        (ExternalBlockHash::Unsigned(10), None, vec![1_u32, 2]),
        (ExternalBlockHash::Signed(-10), None, vec![1_u32, 2, 3]),
        (
            ExternalBlockHash::Bytes(vec![0x10, 0x20].into()),
            Some(ExternalBlockHash::Unsigned(10)),
            vec![4_u32, 5, 6, 7],
        ),
    ];
    for (hash, parent, tokens) in &cases {
        let event = store(hash.clone(), parent.clone(), tokens);
        assert_eq!(exact.store(&event).unwrap(), digest.store(&event).unwrap());
    }
    for query in [&[][..], &[1, 2], &[1, 2, 4], &[1, 2, 4, 5, 6, 7, 8]] {
        assert_parity(&exact, &digest, query);
    }

    let removed = remove(ExternalBlockHash::Unsigned(10));
    assert_eq!(exact.remove(&removed), digest.remove(&removed));
    assert_parity(&exact, &digest, &[1, 2, 4, 5, 6, 7]);

    let restored = store(ExternalBlockHash::Unsigned(10), None, &[1, 2]);
    assert_eq!(
        exact.store(&restored).unwrap(),
        digest.store(&restored).unwrap()
    );
    assert_parity(&exact, &digest, &[1, 2, 4, 5, 6, 7]);

    for (hash, _, _) in cases.iter().rev() {
        let event = remove(hash.clone());
        assert_eq!(exact.remove(&event), digest.remove(&event));
    }
    assert_parity(&exact, &digest, &[1, 2, 4, 5, 6, 7]);
}

#[derive(Clone)]
struct ChainNode {
    hash: ExternalBlockHash,
    parent: Option<ExternalBlockHash>,
    tokens: Vec<u32>,
    present: bool,
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

#[test]
fn deterministic_churn_never_diverges_or_overclaims() {
    const SEEDS: u64 = 32;
    const ACTIONS: usize = 1_000;
    const CHAINS: usize = 6;
    const DEPTH: usize = 8;

    for seed in 1..=SEEDS {
        let (mut exact, mut digest) = indexes();
        let mut nodes = Vec::with_capacity(CHAINS * DEPTH);
        let mut queries = Vec::with_capacity(CHAINS);
        for chain in 0..CHAINS {
            let mut parent = None;
            let mut query = Vec::new();
            for depth in 0..DEPTH {
                let id = u64::try_from(chain * DEPTH + depth + 1).unwrap();
                let hash = ExternalBlockHash::Unsigned(seed << 32 | id);
                let length = 2 + (chain + depth) % 3;
                let tokens = (0..length)
                    .map(|offset| u32::try_from(chain * 10_000 + depth * 100 + offset + 1).unwrap())
                    .collect::<Vec<_>>();
                let event = store(hash.clone(), parent.clone(), &tokens);
                exact.store(&event).unwrap();
                digest.store(&event).unwrap();
                query.extend_from_slice(&tokens);
                nodes.push(ChainNode {
                    hash: hash.clone(),
                    parent: parent.clone(),
                    tokens,
                    present: true,
                });
                parent = Some(hash);
            }
            query.push(u32::MAX);
            queries.push(query);
        }

        let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15));
        for action in 0..ACTIONS {
            let index = usize::try_from(rng.next()).unwrap() % nodes.len();
            let can_insert = !nodes[index].present
                && nodes[index].parent.as_ref().is_none_or(|parent| {
                    nodes
                        .iter()
                        .find(|node| &node.hash == parent)
                        .is_some_and(|node| node.present)
                });
            if nodes[index].present {
                let event = remove(nodes[index].hash.clone());
                assert_eq!(exact.remove(&event), digest.remove(&event));
                nodes[index].present = false;
            } else if can_insert {
                let event = store(
                    nodes[index].hash.clone(),
                    nodes[index].parent.clone(),
                    &nodes[index].tokens,
                );
                assert_eq!(exact.store(&event).unwrap(), digest.store(&event).unwrap());
                nodes[index].present = true;
            }

            let first = (index / DEPTH) % CHAINS;
            let second = usize::try_from(rng.next()).unwrap() % CHAINS;
            assert_parity(&exact, &digest, &queries[first]);
            assert_parity(&exact, &digest, &queries[second]);
            if action % 137 == 0 {
                assert_parity(&exact, &digest, &[]);
            }
        }
    }
}
