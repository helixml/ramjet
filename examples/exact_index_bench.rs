use std::{
    fs,
    hint::black_box,
    sync::{Arc, Barrier},
    thread,
    time::Instant,
};

use mini_dynamo::{
    exact_index::{ExactIndexLimits, ExactKvIndex, SharedExactKvInventory},
    kv_wire::{BlockRemoved, BlockStored, ExternalBlockHash, KvEvent},
};
use serde::Serialize;

const BLOCK_SIZE: usize = 256;
const SEQUENCES: usize = 48;
const BLOCKS_PER_SEQUENCE: usize = 316;

#[derive(Clone, Copy, Serialize)]
struct ResultRow<'a> {
    phase: &'a str,
    operations: usize,
    token_ids_per_operation: usize,
    microseconds_per_operation: f64,
    operations_per_second: f64,
    nodes: usize,
    resident_token_ids: usize,
    rss_delta_kib: Option<u64>,
    threads: usize,
}

fn main() {
    let limits = ExactIndexLimits::default();
    let before_rss = rss_kib();
    let started = Instant::now();
    let mut index = ExactKvIndex::new(limits);
    for sequence in 0..SEQUENCES {
        index
            .store(&sequence_event(sequence))
            .expect("capacity build");
    }
    let build_elapsed = started.elapsed().as_secs_f64();
    let stats = index.stats();
    print_row(ResultRow {
        phase: "capacity_build",
        operations: stats.nodes,
        token_ids_per_operation: BLOCK_SIZE,
        microseconds_per_operation: build_elapsed * 1_000_000.0 / usize_f64(stats.nodes),
        operations_per_second: usize_f64(stats.nodes) / build_elapsed,
        nodes: stats.nodes,
        resident_token_ids: stats.token_ids,
        rss_delta_kib: before_rss
            .zip(rss_kib())
            .map(|(before, after)| after.saturating_sub(before)),
        threads: 1,
    });

    let query = sequence_tokens(0);
    for (blocks, iterations) in [(16_usize, 20_000_usize), (74, 5_000), (316, 1_000)] {
        let tokens = &query[..blocks * BLOCK_SIZE];
        for _ in 0..100 {
            black_box(index.find_longest(tokens).expect("warm-up lookup"));
        }
        let started = Instant::now();
        for _ in 0..iterations {
            black_box(index.find_longest(tokens).expect("benchmark lookup"));
        }
        let elapsed = started.elapsed().as_secs_f64();
        print_row(ResultRow {
            phase: "lookup",
            operations: iterations,
            token_ids_per_operation: tokens.len(),
            microseconds_per_operation: elapsed * 1_000_000.0 / usize_f64(iterations),
            operations_per_second: usize_f64(iterations) / elapsed,
            nodes: stats.nodes,
            resident_token_ids: stats.token_ids,
            rss_delta_kib: None,
            threads: 1,
        });
    }

    let parent = block_hash(0, BLOCKS_PER_SEQUENCE - 1);
    let update_tokens = synthetic_block(SEQUENCES + 1, 0);
    let updates = 20_000_usize;
    let started = Instant::now();
    for operation in 0..updates {
        let hash = ExternalBlockHash::Unsigned(
            1_u64 << 63 | u64::try_from(operation).expect("update hash"),
        );
        let stored = BlockStored {
            block_hashes: vec![hash.clone()],
            parent_block_hash: Some(parent.clone()),
            token_ids: update_tokens.clone(),
            block_size: BLOCK_SIZE,
            group_idx: Some(0),
            kv_cache_spec_kind: None,
            kv_cache_spec_sliding_window: None,
            medium: Some("GPU".to_owned()),
            locality: Some("LOCAL".to_owned()),
            lora_name: None,
            cache_namespace: None,
            has_extra_keys: false,
        };
        index.store(&stored).expect("benchmark store");
        index.remove(&BlockRemoved {
            block_hashes: vec![hash],
            group_idx: Some(0),
            medium: Some("GPU".to_owned()),
            locality: Some("LOCAL".to_owned()),
        });
    }
    let elapsed = started.elapsed().as_secs_f64();
    print_row(ResultRow {
        phase: "store_remove_pair",
        operations: updates,
        token_ids_per_operation: BLOCK_SIZE,
        microseconds_per_operation: elapsed * 1_000_000.0 / usize_f64(updates),
        operations_per_second: usize_f64(updates) / elapsed,
        nodes: index.stats().nodes,
        resident_token_ids: index.stats().token_ids,
        rss_delta_kib: None,
        threads: 1,
    });

    concurrent_read_bench(limits, &query);
}

fn concurrent_read_bench(limits: ExactIndexLimits, query: &[u32]) {
    let inventory = Arc::new(SharedExactKvInventory::new(limits));
    for sequence in 0..SEQUENCES {
        inventory
            .apply_event(0, &KvEvent::BlockStored(sequence_event(sequence)))
            .expect("shared capacity build");
    }
    let threads = 8_usize;
    let iterations = 1_000_usize;
    let query: Arc<[u32]> = Arc::from(query);
    let barrier = Arc::new(Barrier::new(threads + 1));
    let readers = (0..threads)
        .map(|_| {
            let inventory = inventory.clone();
            let query = query.clone();
            let barrier = barrier.clone();
            thread::spawn(move || {
                barrier.wait();
                for _ in 0..iterations {
                    black_box(inventory.find_longest(&query).expect("concurrent lookup"));
                }
            })
        })
        .collect::<Vec<_>>();
    barrier.wait();
    let started = Instant::now();
    for reader in readers {
        reader.join().expect("benchmark reader");
    }
    let elapsed = started.elapsed().as_secs_f64();
    let operations = threads * iterations;
    let stats = inventory.stats();
    print_row(ResultRow {
        phase: "concurrent_lookup",
        operations,
        token_ids_per_operation: query.len(),
        microseconds_per_operation: elapsed * 1_000_000.0 / usize_f64(operations),
        operations_per_second: usize_f64(operations) / elapsed,
        nodes: stats.nodes,
        resident_token_ids: stats.token_ids,
        rss_delta_kib: None,
        threads,
    });
}

fn sequence_event(sequence: usize) -> BlockStored {
    BlockStored {
        block_hashes: (0..BLOCKS_PER_SEQUENCE)
            .map(|block| block_hash(sequence, block))
            .collect(),
        parent_block_hash: None,
        token_ids: sequence_tokens(sequence),
        block_size: BLOCK_SIZE,
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

fn sequence_tokens(sequence: usize) -> Vec<u32> {
    (0..BLOCKS_PER_SEQUENCE)
        .flat_map(|block| synthetic_block(sequence, block))
        .collect()
}

fn synthetic_block(sequence: usize, block: usize) -> Vec<u32> {
    (0..BLOCK_SIZE)
        .map(|offset| {
            u32::try_from((sequence * 4_099 + block * 257 + offset) % 129_280)
                .expect("synthetic token ID")
        })
        .collect()
}

fn block_hash(sequence: usize, block: usize) -> ExternalBlockHash {
    ExternalBlockHash::Unsigned(
        (u64::try_from(sequence + 1).expect("sequence hash") << 32)
            | u64::try_from(block + 1).expect("block hash"),
    )
}

fn rss_kib() -> Option<u64> {
    fs::read_to_string("/proc/self/status")
        .ok()?
        .lines()
        .find_map(|line| line.strip_prefix("VmRSS:"))?
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

fn usize_f64(value: usize) -> f64 {
    f64::from(u32::try_from(value).expect("benchmark count fits u32"))
}

fn print_row(row: ResultRow<'_>) {
    println!("{}", serde_json::to_string(&row).expect("serialize result"));
}
