use std::{env, time::Duration};

use mini_dynamo::{
    kv_transport::{KvTransportConfig, ZmqKvEventSource},
    kv_wire::{KvEvent, KvWireLimits},
};
use serde::Serialize;
use tokio::time::timeout;

#[derive(Serialize)]
struct ProbeResult {
    live_sequence: u64,
    live_events: usize,
    live_stored_blocks: usize,
    live_stored_token_ids: usize,
    replay_from: u64,
    replay_through: u64,
    replay_batches: usize,
    replay_events: usize,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = env::args().collect::<Vec<_>>();
    let live_endpoint = args
        .get(1)
        .cloned()
        .unwrap_or_else(|| "tcp://127.0.0.1:5557".to_owned());
    let replay_endpoint = args
        .get(2)
        .cloned()
        .unwrap_or_else(|| "tcp://127.0.0.1:5558".to_owned());
    let topic = args.get(3).cloned().unwrap_or_default();
    let mut source = ZmqKvEventSource::connect(KvTransportConfig {
        live_endpoint,
        replay_endpoint: Some(replay_endpoint),
        topic,
        connect_timeout: Duration::from_secs(5),
        replay_timeout: Duration::from_secs(5),
        max_replay_batches: 1_024,
        max_replay_tail_batches: 64,
        wire_limits: KvWireLimits::default(),
    })
    .await?;
    let live = timeout(Duration::from_secs(10), source.recv_live())
        .await
        .map_err(|_| anyhow::anyhow!("live receive timed out"))??;
    let replay_from = live.sequence.saturating_sub(2);
    let replay = source.replay(replay_from, live.sequence).await?;
    let (live_stored_blocks, live_stored_token_ids) =
        live.batch
            .events
            .iter()
            .fold((0usize, 0usize), |(blocks, tokens), event| match event {
                KvEvent::BlockStored(stored) => (
                    blocks + stored.block_hashes.len(),
                    tokens + stored.token_ids.len(),
                ),
                KvEvent::BlockRemoved(_) | KvEvent::AllBlocksCleared => (blocks, tokens),
            });
    println!(
        "{}",
        serde_json::to_string(&ProbeResult {
            live_sequence: live.sequence,
            live_events: live.batch.events.len(),
            live_stored_blocks,
            live_stored_token_ids,
            replay_from,
            replay_through: live.sequence,
            replay_batches: replay.len(),
            replay_events: replay.iter().map(|batch| batch.batch.events.len()).sum(),
        })?
    );
    Ok(())
}
