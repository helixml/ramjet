//! Supervised, observation-only KV-event consumers.
//!
//! Each upstream owns an independent fenced inventory and transport task.
//! Nothing in this module participates in route selection: it qualifies real
//! event streams and recovery behavior before exact placement is enabled.

use std::{sync::Arc, time::Duration};

use parking_lot::RwLock;
use tokio::{sync::broadcast, task::JoinHandle, time::sleep};

use crate::{
    config::{Config, KvEventMode},
    exact_index::{ExactIndexLimits, FencedExactKvInventory, LiveBatchOutcome, ReplayBatchOutcome},
    kv_transport::{KvTransportConfig, LiveActivity, SequencedBatch, ZmqKvEventSource},
    kv_wire::KvWireLimits,
    metrics::Metrics,
};

pub type SharedFencedInventory = Arc<RwLock<FencedExactKvInventory>>;

pub struct KvEventConsumers {
    inventories: Arc<[SharedFencedInventory]>,
    tasks: Vec<JoinHandle<()>>,
}

impl KvEventConsumers {
    #[must_use]
    pub fn start(
        config: &Config,
        metrics: &Arc<Metrics>,
        shutdown: &broadcast::Sender<()>,
    ) -> Self {
        if config.kv_event_mode == KvEventMode::Off {
            return Self {
                inventories: Arc::from([]),
                tasks: Vec::new(),
            };
        }

        let replay_limit = u64::try_from(config.kv_event_replay_limit).unwrap_or(u64::MAX);
        let inventories = config
            .kv_event_sources
            .iter()
            .map(|_| {
                Arc::new(RwLock::new(FencedExactKvInventory::new(
                    replay_limit,
                    ExactIndexLimits::default(),
                )))
            })
            .collect::<Arc<[_]>>();
        let tasks = config
            .kv_event_sources
            .iter()
            .cloned()
            .enumerate()
            .map(|(index, source)| {
                let inventory = inventories[index].clone();
                let metrics = Arc::clone(metrics);
                let upstream = config.upstreams[index].as_str().to_owned();
                let consumer_config = ConsumerConfig {
                    upstream_index: index,
                    transport: KvTransportConfig {
                        live_endpoint: source.live_endpoint,
                        replay_endpoint: Some(source.replay_endpoint),
                        topic: source.topic,
                        connect_timeout: Duration::from_millis(config.kv_event_timeout_ms as u64),
                        replay_timeout: Duration::from_millis(config.kv_event_timeout_ms as u64),
                        max_replay_batches: config.kv_event_replay_limit,
                        max_replay_tail_batches: config.kv_event_replay_tail_limit,
                        wire_limits: KvWireLimits::default(),
                    },
                    reconnect_min: Duration::from_millis(config.kv_event_reconnect_min_ms as u64),
                    reconnect_max: Duration::from_millis(config.kv_event_reconnect_max_ms as u64),
                    stagger: Duration::from_millis((index as u64).saturating_mul(53)),
                };
                let shutdown = shutdown.subscribe();
                tokio::spawn(run_consumer(
                    consumer_config,
                    upstream,
                    inventory,
                    metrics,
                    shutdown,
                ))
            })
            .collect();
        Self { inventories, tasks }
    }

    #[must_use]
    pub fn inventories(&self) -> Arc<[SharedFencedInventory]> {
        self.inventories.clone()
    }

    pub async fn shutdown(self) {
        for mut task in self.tasks {
            if tokio::time::timeout(Duration::from_secs(2), &mut task)
                .await
                .is_err()
            {
                task.abort();
            }
        }
    }
}

struct ConsumerConfig {
    upstream_index: usize,
    transport: KvTransportConfig,
    reconnect_min: Duration,
    reconnect_max: Duration,
    stagger: Duration,
}

async fn run_consumer(
    config: ConsumerConfig,
    upstream: String,
    inventory: SharedFencedInventory,
    metrics: Arc<Metrics>,
    mut shutdown: broadcast::Receiver<()>,
) {
    let mut backoff = config.reconnect_min;
    let mut first_attempt = true;
    loop {
        if first_attempt
            && !config.stagger.is_zero()
            && wait_or_shutdown(config.stagger, &mut shutdown).await
        {
            return;
        }
        first_attempt = false;
        metrics
            .kv_event_reconnects
            .with_label_values(&[&upstream, "attempt"])
            .inc();
        let connected = tokio::select! {
            _ = shutdown.recv() => return,
            result = ZmqKvEventSource::connect(config.transport.clone()) => result,
        };
        let Ok(mut source) = connected else {
            metrics
                .kv_event_reconnects
                .with_label_values(&[&upstream, "connect_error"])
                .inc();
            if wait_or_shutdown(backoff, &mut shutdown).await {
                return;
            }
            backoff = next_backoff(backoff, config.reconnect_max);
            continue;
        };
        let Some((disconnect_reason, made_progress)) = consume_connection(
            &config,
            &upstream,
            &inventory,
            &metrics,
            &mut shutdown,
            &mut source,
        )
        .await
        else {
            return;
        };

        metrics.kv_event_up.with_label_values(&[&upstream]).set(0.0);
        metrics
            .kv_event_reconnects
            .with_label_values(&[&upstream, disconnect_reason])
            .inc();
        inventory.write().generation_changed();
        update_state_metrics(&inventory, &metrics, &upstream);
        tracing::warn!(
            upstream_index = config.upstream_index,
            reason = disconnect_reason,
            "KV-event shadow consumer disconnected"
        );
        if made_progress {
            backoff = config.reconnect_min;
        }
        if wait_or_shutdown(backoff, &mut shutdown).await {
            return;
        }
        backoff = next_backoff(backoff, config.reconnect_max);
    }
}

async fn consume_connection(
    config: &ConsumerConfig,
    upstream: &str,
    inventory: &SharedFencedInventory,
    metrics: &Metrics,
    shutdown: &mut broadcast::Receiver<()>,
    source: &mut ZmqKvEventSource,
) -> Option<(&'static str, bool)> {
    let mut made_progress = false;
    loop {
        let received = tokio::select! {
            _ = shutdown.recv() => {
                metrics.kv_event_up.with_label_values(&[upstream]).set(0.0);
                return None;
            }
            result = source.recv_live_activity() => result,
        };
        let activity = match received {
            Ok(activity) => activity,
            Err(error) => return Some((error.reason(), made_progress)),
        };
        let live = match activity {
            LiveActivity::Connected => {
                connection_changed(config, upstream, inventory, metrics, true);
                continue;
            }
            LiveActivity::Disconnected => {
                connection_changed(config, upstream, inventory, metrics, false);
                continue;
            }
            LiveActivity::Batch(batch) => batch,
        };
        made_progress = true;
        if let Ok(Some((from, through))) = ingest_live(inventory, metrics, upstream, &live) {
            let replayed = tokio::select! {
                _ = shutdown.recv() => {
                    metrics.kv_event_up.with_label_values(&[upstream]).set(0.0);
                    return None;
                }
                result = source.replay(from, through) => result,
            };
            let replayed = match replayed {
                Ok(replayed) => replayed,
                Err(error) => return Some((error.reason(), made_progress)),
            };
            metrics
                .kv_event_replay_batches
                .with_label_values(&[upstream])
                .observe(metric_usize(replayed.len()));
            ingest_replay(inventory, metrics, upstream, replayed);
        }
    }
}

fn connection_changed(
    config: &ConsumerConfig,
    upstream: &str,
    inventory: &SharedFencedInventory,
    metrics: &Metrics,
    connected: bool,
) {
    let label = if connected {
        "connected"
    } else {
        "disconnected"
    };
    metrics
        .kv_event_up
        .with_label_values(&[upstream])
        .set(if connected { 1.0 } else { 0.0 });
    metrics
        .kv_event_reconnects
        .with_label_values(&[upstream, label])
        .inc();
    if connected {
        tracing::info!(
            upstream_index = config.upstream_index,
            "KV-event shadow consumer connected"
        );
    } else {
        inventory.write().generation_changed();
        update_state_metrics(inventory, metrics, upstream);
        tracing::warn!(
            upstream_index = config.upstream_index,
            "KV-event shadow consumer transport disconnected"
        );
    }
}

fn ingest_live(
    inventory: &SharedFencedInventory,
    metrics: &Metrics,
    upstream: &str,
    live: &SequencedBatch,
) -> Result<Option<(u64, u64)>, ()> {
    let outcome = inventory.write().ingest_live(live.sequence, &live.batch);
    let failed = outcome.is_err();
    let (label, replay, summary) = match outcome {
        Ok(LiveBatchOutcome::Applied(summary)) => ("applied", None, Some(summary)),
        Ok(LiveBatchOutcome::ObserveOnly) => ("observe_only", None, None),
        Ok(LiveBatchOutcome::Duplicate) => ("duplicate", None, None),
        Ok(LiveBatchOutcome::Replay { from, through }) => ("replay", Some((from, through)), None),
        Ok(LiveBatchOutcome::Fenced) => ("fenced", None, None),
        Err(error) => (error.reason(), None, None),
    };
    metrics
        .kv_event_batches
        .with_label_values(&[upstream, "live", label])
        .inc();
    if let Some(summary) = summary {
        record_filtered(metrics, upstream, "live", summary);
    }
    update_state_metrics(inventory, metrics, upstream);
    if failed { Err(()) } else { Ok(replay) }
}

fn ingest_replay(
    inventory: &SharedFencedInventory,
    metrics: &Metrics,
    upstream: &str,
    batches: Vec<SequencedBatch>,
) {
    let batches = batches
        .into_iter()
        .map(|batch| (batch.sequence, batch.batch))
        .collect::<Vec<_>>();
    let outcome = inventory.write().ingest_replay(&batches);
    let (label, summary) = match outcome {
        Ok(ReplayBatchOutcome::Applied(summary)) => ("applied", Some(summary)),
        Ok(ReplayBatchOutcome::ObserveOnly) => ("observe_only", None),
        Ok(ReplayBatchOutcome::Invalid) => ("invalid", None),
        Err(error) => (error.reason(), None),
    };
    metrics
        .kv_event_batches
        .with_label_values(&[upstream, "replay", label])
        .inc();
    if let Some(summary) = summary {
        record_filtered(metrics, upstream, "replay", summary);
    }
    update_state_metrics(inventory, metrics, upstream);
}

fn record_filtered(
    metrics: &Metrics,
    upstream: &str,
    source: &'static str,
    summary: crate::exact_index::BatchApplySummary,
) {
    for (reason, count) in summary.filtered_by_reason().filter(|(_, count)| *count > 0) {
        metrics
            .kv_event_filtered
            .with_label_values(&[upstream, source, reason.label()])
            .inc_by(metric_usize(count));
    }
}

fn update_state_metrics(inventory: &SharedFencedInventory, metrics: &Metrics, upstream: &str) {
    let inventory = inventory.read();
    let stats = inventory.stats();
    metrics
        .kv_event_trusted
        .with_label_values(&[upstream])
        .set(if inventory.trusted() { 1.0 } else { 0.0 });
    metrics
        .kv_event_generation
        .with_label_values(&[upstream])
        .set(metric_u64(inventory.generation()));
    for (kind, value) in [
        ("nodes", stats.nodes),
        ("token_ids", stats.token_ids),
        ("external_hashes", stats.external_hashes),
    ] {
        metrics
            .kv_event_index_entries
            .with_label_values(&[upstream, kind])
            .set(metric_usize(value));
    }
}

async fn wait_or_shutdown(duration: Duration, shutdown: &mut broadcast::Receiver<()>) -> bool {
    tokio::select! {
        () = sleep(duration) => false,
        _ = shutdown.recv() => true,
    }
}

fn next_backoff(current: Duration, maximum: Duration) -> Duration {
    current.saturating_mul(2).min(maximum)
}

fn metric_usize(value: usize) -> f64 {
    f64::from(u32::try_from(value).unwrap_or(u32::MAX))
}

fn metric_u64(value: u64) -> f64 {
    f64::from(u32::try_from(value).unwrap_or(u32::MAX))
}

#[cfg(test)]
mod tests {
    use prometheus::Registry;

    use super::*;
    use crate::kv_wire::{BlockStored, KvEvent, KvEventBatch};

    #[test]
    fn live_shadow_requests_startup_replay_and_clear_establishes_generation() {
        let registry = Registry::new();
        let metrics = Metrics::new(&registry).unwrap();
        let inventory = Arc::new(RwLock::new(FencedExactKvInventory::new(
            8,
            ExactIndexLimits::default(),
        )));
        let observed = SequencedBatch {
            sequence: 4,
            batch: KvEventBatch {
                timestamp: 1.0,
                events: Vec::new(),
                data_parallel_rank: Some(0),
            },
        };
        assert_eq!(
            ingest_live(&inventory, &metrics, "engine", &observed),
            Ok(Some((0, 4)))
        );
        assert!(!inventory.read().trusted());

        let cleared = SequencedBatch {
            sequence: 5,
            batch: KvEventBatch {
                timestamp: 2.0,
                events: vec![KvEvent::AllBlocksCleared],
                data_parallel_rank: Some(0),
            },
        };
        assert_eq!(
            ingest_live(&inventory, &metrics, "engine", &cleared),
            Ok(None)
        );
        assert!(inventory.read().trusted());
        assert!(
            (metrics
                .kv_event_trusted
                .with_label_values(&["engine"])
                .get()
                - 1.0)
                .abs()
                < f64::EPSILON
        );
    }

    #[test]
    fn reconnect_backoff_is_bounded() {
        assert_eq!(
            next_backoff(Duration::from_millis(250), Duration::from_secs(1)),
            Duration::from_millis(500)
        );
        assert_eq!(
            next_backoff(Duration::from_millis(800), Duration::from_secs(1)),
            Duration::from_secs(1)
        );
    }

    #[test]
    fn filtered_event_metrics_use_bounded_reason_labels() {
        let registry = Registry::new();
        let metrics = Metrics::new(&registry).unwrap();
        let inventory = Arc::new(RwLock::new(FencedExactKvInventory::new(
            8,
            ExactIndexLimits::default(),
        )));
        let filtered = SequencedBatch {
            sequence: 0,
            batch: KvEventBatch {
                timestamp: 1.0,
                events: vec![KvEvent::BlockStored(BlockStored {
                    block_hashes: Vec::new(),
                    parent_block_hash: None,
                    token_ids: vec![1, 2, 3],
                    block_size: 2,
                    group_idx: Some(1),
                    kv_cache_spec_kind: Some("sliding_window_mla".to_owned()),
                    kv_cache_spec_sliding_window: Some(256),
                    medium: Some("GPU".to_owned()),
                    locality: Some("LOCAL".to_owned()),
                    lora_name: None,
                    cache_namespace: None,
                    has_extra_keys: false,
                })],
                data_parallel_rank: Some(0),
            },
        };

        assert_eq!(
            ingest_live(&inventory, &metrics, "engine", &filtered),
            Ok(None)
        );
        assert!(inventory.read().trusted());
        assert!(
            (metrics
                .kv_event_filtered
                .with_label_values(&["engine", "live", "non_main_attention"])
                .get()
                - 1.0)
                .abs()
                < f64::EPSILON
        );
    }
}
