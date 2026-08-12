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

struct ConnectionEnd {
    reason: &'static str,
    restored_authority: bool,
    full_replay_through: Option<u64>,
}

enum ReplayAttempt {
    Applied { authoritative: bool },
    Failed(&'static str),
    Shutdown,
}

async fn run_consumer(
    config: ConsumerConfig,
    upstream: String,
    inventory: SharedFencedInventory,
    metrics: Arc<Metrics>,
    mut shutdown: broadcast::Receiver<()>,
) {
    initialize_event_metrics(&metrics, &upstream);
    let mut backoff = config.reconnect_min;
    let mut first_attempt = true;
    let mut full_replay_through = None;
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
        let Some(ended) = consume_connection(
            &config,
            &upstream,
            &inventory,
            &metrics,
            &mut shutdown,
            &mut source,
            full_replay_through.take(),
        )
        .await
        else {
            return;
        };

        metrics.kv_event_up.with_label_values(&[&upstream]).set(0.0);
        metrics
            .kv_event_reconnects
            .with_label_values(&[&upstream, ended.reason])
            .inc();
        full_replay_through = match ended.full_replay_through {
            Some(through) if inventory.write().prepare_full_replay_retry(through) => Some(through),
            Some(_) => None,
            None => {
                inventory.write().generation_changed();
                None
            }
        };
        update_state_metrics(&inventory, &metrics, &upstream);
        tracing::warn!(
            upstream_index = config.upstream_index,
            reason = ended.reason,
            replay_retry_through = full_replay_through,
            "KV-event shadow consumer disconnected"
        );
        let delay = recovery_delay(
            &mut backoff,
            config.reconnect_min,
            config.reconnect_max,
            ended.restored_authority,
            if ended.reason == "replay_timeout_undrained" {
                config.transport.replay_timeout
            } else {
                Duration::ZERO
            },
        );
        if wait_or_shutdown(delay, &mut shutdown).await {
            return;
        }
    }
}

fn initialize_event_metrics(metrics: &Metrics, upstream: &str) {
    for source in ["live", "replay"] {
        for action in ["stored", "removed"] {
            metrics
                .kv_event_blocks
                .with_label_values(&[upstream, source, action]);
        }
        metrics
            .kv_event_clears
            .with_label_values(&[upstream, source]);
    }
}

async fn consume_connection(
    config: &ConsumerConfig,
    upstream: &str,
    inventory: &SharedFencedInventory,
    metrics: &Metrics,
    shutdown: &mut broadcast::Receiver<()>,
    source: &mut ZmqKvEventSource,
    full_replay_through: Option<u64>,
) -> Option<ConnectionEnd> {
    // Merely receiving a live event is not recovery progress: an untrusted
    // startup fence uses that event to request a full replay. Resetting the
    // reconnect delay before that replay becomes authoritative can create a
    // rapid request storm behind one abandoned publisher-side replay.
    let mut restored_authority = false;
    if let Some(through) = full_replay_through {
        match replay_range(source, inventory, metrics, upstream, shutdown, 0, through).await {
            ReplayAttempt::Applied { authoritative } => {
                restored_authority = authoritative;
                if !authoritative {
                    return Some(ConnectionEnd {
                        reason: "replay_not_authoritative",
                        restored_authority,
                        full_replay_through: Some(through),
                    });
                }
            }
            ReplayAttempt::Failed(reason) => {
                return Some(ConnectionEnd {
                    reason,
                    restored_authority,
                    full_replay_through: Some(through),
                });
            }
            ReplayAttempt::Shutdown => return None,
        }
    }
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
            Err(error) => {
                return Some(ConnectionEnd {
                    reason: error.reason(),
                    restored_authority,
                    full_replay_through: None,
                });
            }
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
        let replay = ingest_live(inventory, metrics, upstream, &live);
        restored_authority |= inventory.read().trusted();
        if let Ok(Some((from, through))) = replay {
            match replay_range(
                source, inventory, metrics, upstream, shutdown, from, through,
            )
            .await
            {
                ReplayAttempt::Applied { authoritative } => {
                    if !authoritative {
                        return Some(ConnectionEnd {
                            reason: "replay_not_authoritative",
                            restored_authority,
                            full_replay_through: Some(through),
                        });
                    }
                }
                ReplayAttempt::Failed(reason) => {
                    return Some(ConnectionEnd {
                        reason,
                        restored_authority,
                        full_replay_through: Some(through),
                    });
                }
                ReplayAttempt::Shutdown => return None,
            }
            restored_authority = true;
        }
    }
}

async fn replay_range(
    source: &mut ZmqKvEventSource,
    inventory: &SharedFencedInventory,
    metrics: &Metrics,
    upstream: &str,
    shutdown: &mut broadcast::Receiver<()>,
    from: u64,
    through: u64,
) -> ReplayAttempt {
    let replayed = tokio::select! {
        _ = shutdown.recv() => {
            metrics.kv_event_up.with_label_values(&[upstream]).set(0.0);
            return ReplayAttempt::Shutdown;
        }
        result = source.replay(from, through) => result,
    };
    let replayed = match replayed {
        Ok(replayed) => replayed,
        Err(error) => return ReplayAttempt::Failed(error.reason()),
    };
    metrics
        .kv_event_replay_batches
        .with_label_values(&[upstream])
        .observe(metric_usize(replayed.len()));
    ReplayAttempt::Applied {
        authoritative: ingest_replay(inventory, metrics, upstream, replayed),
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
        record_apply_summary(metrics, upstream, "live", summary);
    }
    update_state_metrics(inventory, metrics, upstream);
    if failed { Err(()) } else { Ok(replay) }
}

fn ingest_replay(
    inventory: &SharedFencedInventory,
    metrics: &Metrics,
    upstream: &str,
    batches: Vec<SequencedBatch>,
) -> bool {
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
        record_apply_summary(metrics, upstream, "replay", summary);
    }
    update_state_metrics(inventory, metrics, upstream);
    inventory.read().trusted()
}

fn record_apply_summary(
    metrics: &Metrics,
    upstream: &str,
    source: &'static str,
    summary: crate::exact_index::BatchApplySummary,
) {
    for (action, count) in [
        ("stored", summary.stored_blocks),
        ("removed", summary.removed_blocks),
    ] {
        if count > 0 {
            metrics
                .kv_event_blocks
                .with_label_values(&[upstream, source, action])
                .inc_by(metric_usize(count));
        }
    }
    if summary.clear_events > 0 {
        metrics
            .kv_event_clears
            .with_label_values(&[upstream, source])
            .inc_by(metric_usize(summary.clear_events));
    }
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

fn recovery_delay(
    backoff: &mut Duration,
    minimum: Duration,
    maximum: Duration,
    restored_authority: bool,
    retry_floor: Duration,
) -> Duration {
    if restored_authority {
        *backoff = minimum;
    }
    let delay = (*backoff).max(retry_floor);
    *backoff = next_backoff(*backoff, maximum);
    delay
}

fn metric_usize(value: usize) -> f64 {
    f64::from(u32::try_from(value).unwrap_or(u32::MAX))
}

fn metric_u64(value: u64) -> f64 {
    f64::from(u32::try_from(value).unwrap_or(u32::MAX))
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use prometheus::Registry;
    use zeromq::{PubSocket, RouterSocket, Socket, SocketRecv, SocketSend, ZmqMessage};

    use super::*;
    use crate::kv_wire::{BlockRemoved, BlockStored, ExternalBlockHash, KvEvent, KvEventBatch};

    const EMPTY_BATCH: &[u8] = &[
        0x93, 0xcb, 0x3f, 0xf0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x90, 0x00,
    ];

    fn message(frames: Vec<Bytes>) -> ZmqMessage {
        ZmqMessage::try_from(frames).unwrap()
    }

    async fn serve_incomplete_then_complete_replay(
        mut replay_server: RouterSocket,
        first_request: tokio::sync::oneshot::Sender<()>,
        release: tokio::sync::oneshot::Receiver<()>,
    ) {
        let mut first_request = Some(first_request);
        for attempt in 0..2 {
            let request = replay_server.recv().await.unwrap();
            if let Some(first_request) = first_request.take() {
                first_request.send(()).unwrap();
            }
            assert_eq!(request.get(2).unwrap().as_ref(), 0_u64.to_be_bytes());
            let identity = request.get(0).unwrap().clone();
            let sequences: &[u64] = if attempt == 0 { &[1] } else { &[0, 1] };
            for sequence in sequences {
                replay_server
                    .send(message(vec![
                        identity.clone(),
                        Bytes::new(),
                        Bytes::from_static(b"kv"),
                        Bytes::copy_from_slice(&sequence.to_be_bytes()),
                        Bytes::from_static(EMPTY_BATCH),
                    ]))
                    .await
                    .unwrap();
            }
            replay_server
                .send(message(vec![
                    identity,
                    Bytes::new(),
                    Bytes::new(),
                    Bytes::from_static(&[u8::MAX; 8]),
                    Bytes::new(),
                ]))
                .await
                .unwrap();
        }
        release.await.unwrap();
    }

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
        assert!(
            (metrics
                .kv_event_clears
                .with_label_values(&["engine", "live"])
                .get()
                - 1.0)
                .abs()
                < f64::EPSILON
        );
    }

    #[test]
    fn accepted_store_and_remove_record_content_free_block_churn() {
        let metrics = Metrics::new(&Registry::new()).unwrap();
        let inventory = Arc::new(RwLock::new(FencedExactKvInventory::new(
            8,
            ExactIndexLimits::default(),
        )));
        let stored = SequencedBatch {
            sequence: 0,
            batch: KvEventBatch {
                timestamp: 1.0,
                events: vec![KvEvent::BlockStored(BlockStored {
                    block_hashes: vec![ExternalBlockHash::Unsigned(7)],
                    parent_block_hash: None,
                    token_ids: vec![1, 2],
                    block_size: 2,
                    group_idx: Some(0),
                    kv_cache_spec_kind: Some("mla_attention".to_owned()),
                    kv_cache_spec_sliding_window: None,
                    medium: Some("GPU".to_owned()),
                    locality: Some("LOCAL".to_owned()),
                    lora_name: None,
                    cache_namespace: None,
                    has_extra_keys: false,
                })],
                data_parallel_rank: Some(0),
            },
        };
        let removed = SequencedBatch {
            sequence: 1,
            batch: KvEventBatch {
                timestamp: 2.0,
                events: vec![KvEvent::BlockRemoved(BlockRemoved {
                    block_hashes: vec![ExternalBlockHash::Unsigned(7)],
                    group_idx: Some(0),
                    medium: Some("GPU".to_owned()),
                    locality: Some("LOCAL".to_owned()),
                })],
                data_parallel_rank: Some(0),
            },
        };

        assert_eq!(
            ingest_live(&inventory, &metrics, "engine", &stored),
            Ok(None)
        );
        assert_eq!(
            ingest_live(&inventory, &metrics, "engine", &removed),
            Ok(None)
        );
        for action in ["stored", "removed"] {
            let blocks = metrics
                .kv_event_blocks
                .with_label_values(&["engine", "live", action])
                .get();
            assert!((blocks - 1.0).abs() < f64::EPSILON);
        }
    }

    #[test]
    fn block_churn_series_exist_before_the_first_event() {
        let registry = Registry::new();
        let metrics = Metrics::new(&registry).unwrap();
        initialize_event_metrics(&metrics, "engine");
        let text = prometheus::TextEncoder::new()
            .encode_to_string(&registry.gather())
            .unwrap();
        for labels in [
            r#"action="stored",source="live""#,
            r#"action="removed",source="replay""#,
        ] {
            assert!(text.contains(labels));
        }
        assert!(text.contains("ds4proxy_kv_event_clears_total"));
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
    fn replay_recovery_backoff_resets_only_after_authority_is_restored() {
        let minimum = Duration::from_millis(250);
        let maximum = Duration::from_secs(1);
        let mut backoff = minimum;

        assert_eq!(
            recovery_delay(&mut backoff, minimum, maximum, false, Duration::ZERO),
            Duration::from_millis(250)
        );
        assert_eq!(
            recovery_delay(&mut backoff, minimum, maximum, false, Duration::ZERO),
            Duration::from_millis(500)
        );
        assert_eq!(
            recovery_delay(&mut backoff, minimum, maximum, false, Duration::ZERO),
            Duration::from_secs(1)
        );
        assert_eq!(
            recovery_delay(&mut backoff, minimum, maximum, true, Duration::ZERO),
            Duration::from_millis(250)
        );
        assert_eq!(
            recovery_delay(&mut backoff, minimum, maximum, false, Duration::ZERO),
            Duration::from_millis(500)
        );
    }

    #[test]
    fn undrained_replay_waits_a_full_replay_window_before_retrying() {
        let minimum = Duration::from_millis(250);
        let maximum = Duration::from_secs(10);
        let mut backoff = minimum;

        assert_eq!(
            recovery_delay(
                &mut backoff,
                minimum,
                maximum,
                false,
                Duration::from_secs(20),
            ),
            Duration::from_secs(20)
        );
        // The ordinary exponential state still advances independently, so a
        // later non-replay transport error does not inherit a 20s penalty.
        assert_eq!(
            recovery_delay(&mut backoff, minimum, maximum, false, Duration::ZERO),
            Duration::from_millis(500)
        );
    }

    #[test]
    fn incomplete_replay_does_not_claim_authoritative_progress() {
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

        assert!(!ingest_replay(&inventory, &metrics, "engine", Vec::new()));
        assert!(!inventory.read().trusted());
    }

    #[tokio::test]
    async fn failed_startup_replay_retries_after_reconnect_without_new_live_event() {
        let mut publisher = PubSocket::new();
        let live_endpoint = publisher.bind("tcp://127.0.0.1:0").await.unwrap();
        let mut replay_server = RouterSocket::new();
        let replay_endpoint = replay_server.bind("tcp://127.0.0.1:0").await.unwrap();
        let (shutdown_tx, shutdown_rx) = broadcast::channel(1);
        let metrics = Arc::new(Metrics::new(&Registry::new()).unwrap());
        let inventory = Arc::new(RwLock::new(FencedExactKvInventory::new(
            8,
            ExactIndexLimits::default(),
        )));
        let consumer = tokio::spawn(run_consumer(
            ConsumerConfig {
                upstream_index: 0,
                transport: KvTransportConfig {
                    live_endpoint: live_endpoint.to_string(),
                    replay_endpoint: Some(replay_endpoint.to_string()),
                    topic: "kv".to_owned(),
                    connect_timeout: Duration::from_secs(2),
                    replay_timeout: Duration::from_secs(2),
                    max_replay_batches: 8,
                    max_replay_tail_batches: 2,
                    wire_limits: KvWireLimits::default(),
                },
                reconnect_min: Duration::from_millis(10),
                reconnect_max: Duration::from_millis(50),
                stagger: Duration::ZERO,
            },
            "engine".to_owned(),
            Arc::clone(&inventory),
            Arc::clone(&metrics),
            shutdown_rx,
        ));

        let (first_request_tx, first_request_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let replay_server = tokio::spawn(serve_incomplete_then_complete_replay(
            replay_server,
            first_request_tx,
            release_rx,
        ));

        tokio::time::timeout(Duration::from_secs(2), async {
            while metrics.kv_event_up.with_label_values(&["engine"]).get() == 0.0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let mut first_request_rx = Box::pin(first_request_rx);
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                publisher
                    .send(message(vec![
                        Bytes::from_static(b"kv"),
                        Bytes::copy_from_slice(&1_u64.to_be_bytes()),
                        Bytes::from_static(EMPTY_BATCH),
                    ]))
                    .await
                    .unwrap();
                tokio::select! {
                    result = &mut first_request_rx => {
                        result.unwrap();
                        break;
                    }
                    () = tokio::time::sleep(Duration::from_millis(20)) => {}
                }
            }
        })
        .await
        .expect("the first live batch should trigger replay");

        tokio::time::timeout(Duration::from_secs(3), async {
            while !inventory.read().trusted() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("full replay should retry without a second live batch");
        release_tx.send(()).unwrap();
        replay_server.await.unwrap();
        shutdown_tx.send(()).unwrap();
        consumer.await.unwrap();
        let invalid_replays = metrics
            .kv_event_reconnects
            .with_label_values(&["engine", "invalid_replay"])
            .get();
        assert!((invalid_replays - 1.0).abs() < f64::EPSILON);
        assert_eq!(
            metrics
                .kv_event_replay_batches
                .with_label_values(&["engine"])
                .get_sample_count(),
            1
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
