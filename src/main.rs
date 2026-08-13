use std::{future::IntoFuture, sync::Arc, time::Duration};

use anyhow::Context;
use axum::{
    Router,
    body::Body,
    extract::{Path, State},
    http::{Response, StatusCode},
    routing::{any, get},
};
use mini_dynamo::{
    config::Config,
    kv_consumer::KvEventConsumers,
    metrics::Metrics,
    proxy::Proxy,
    router::{Router as LocalityRouter, RouterConfig},
    snapshot_route::SnapshotRouteConsumers,
};
use prometheus::{Encoder, Registry, TextEncoder};
use tokio::{net::TcpListener, sync::broadcast};
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

#[tokio::main]
#[allow(clippy::too_many_lines)]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with(tracing_subscriber::fmt::layer().json())
        .init();

    let config = Config::from_env().context("invalid mini-dynamo configuration")?;
    let registry = Arc::new(Registry::new());
    let metrics = Arc::new(Metrics::new(&registry).context("register metrics")?);
    let routing = Arc::new(LocalityRouter::new(RouterConfig {
        upstreams: config.upstreams.clone(),
        alpha: config.route_alpha,
        chunk_bytes: config.route_chunk_bytes,
        max_prefix_bytes: config.route_max_prefix_bytes,
        max_overlap_blocks: config.route_max_overlap_blocks,
        index_capacity: config.route_index_capacity,
        load_unit_bytes: config.route_load_unit_bytes,
        max_load_units: config.route_max_load_units,
        affinity: config.affinity,
    }));
    let client = reqwest::Client::builder()
        .pool_max_idle_per_host(256)
        .connect_timeout(Duration::from_secs(30))
        .tcp_keepalive(Duration::from_secs(30))
        .build()
        .context("build upstream client")?;
    let (shutdown_tx, _) = broadcast::channel::<()>(1);
    let snapshot_consumers = SnapshotRouteConsumers::start(&config, &metrics)
        .context("initialize snapshot route consumers")?;
    let kv_consumers = KvEventConsumers::start(&config, &metrics, &shutdown_tx);
    let exact_inventories =
        if config.snapshot_route_mode == mini_dynamo::config::SnapshotRouteMode::Shadow {
            snapshot_consumers.inventories()
        } else {
            kv_consumers
                .inventories()
                .iter()
                .cloned()
                .map(mini_dynamo::exact_route_inventory::ExactRouteInventory::direct)
                .collect()
        };
    let proxy = Proxy::new_with_exact_inventories(
        config.clone(),
        client,
        metrics.clone(),
        routing,
        kv_consumers.inventories(),
        exact_inventories,
    )
    .context("initialize mini-dynamo proxy")?;

    let api = Router::new()
        .route("/health", get(Proxy::health))
        .fallback(any(Proxy::handle))
        .with_state(proxy.clone());
    let metrics_api = Router::new()
        .route("/metrics", get(prometheus_metrics))
        .route("/metrics/upstream/{index}", get(upstream_metrics))
        .with_state(MetricsState {
            proxy: proxy.clone(),
            registry,
        });
    let api_listener = TcpListener::bind("0.0.0.0:8000")
        .await
        .context("bind API listener")?;
    let metrics_listener = TcpListener::bind("0.0.0.0:9090")
        .await
        .context("bind metrics listener")?;

    log_startup(&config);

    let probe = tokio::spawn(proxy.probe_loop());
    let mut api_shutdown = shutdown_tx.subscribe();
    let mut metrics_shutdown = shutdown_tx.subscribe();
    let api_server = axum::serve(api_listener, api).with_graceful_shutdown(async move {
        let _ = api_shutdown.recv().await;
    });
    let metrics_server =
        axum::serve(metrics_listener, metrics_api).with_graceful_shutdown(async move {
            let _ = metrics_shutdown.recv().await;
        });
    let mut api_task = tokio::spawn(api_server.into_future());
    let mut metrics_task = tokio::spawn(metrics_server.into_future());
    let exit = tokio::select! {
        result = &mut api_task => Exit::Api(result),
        result = &mut metrics_task => Exit::Metrics(result),
        () = shutdown_signal() => Exit::Signal,
    };
    let _ = shutdown_tx.send(());
    snapshot_consumers.request_shutdown();
    match exit {
        Exit::Api(result) => {
            result.context("join API server")?.context("API server")?;
            await_shutdown(&mut metrics_task, "metrics").await?;
        }
        Exit::Metrics(result) => {
            result
                .context("join metrics server")?
                .context("metrics server")?;
            await_shutdown(&mut api_task, "API").await?;
        }
        Exit::Signal => {
            await_shutdown(&mut api_task, "API").await?;
            await_shutdown(&mut metrics_task, "metrics").await?;
        }
    }
    probe.abort();
    kv_consumers.shutdown().await;
    snapshot_consumers.shutdown().await;
    Ok(())
}

fn log_startup(config: &Config) {
    tracing::info!(
        upstreams = ?config.upstreams,
        affinity = ?config.affinity,
        alpha = config.route_alpha,
        max_prefix_bytes = config.route_max_prefix_bytes,
        max_overlap_blocks = config.route_max_overlap_blocks,
        load_unit_bytes = config.route_load_unit_bytes,
        max_load_units = config.route_max_load_units,
        journal = config.route_journal,
        tokenizer_mode = ?config.tokenizer_mode,
        tokenizer_profile = ?config.tokenizer_profile,
        tokenizer_min_bytes = config.tokenizer_min_bytes,
        tokenizer_max_bytes = config.tokenizer_max_bytes,
        tokenizer_workers = config.tokenizer_workers,
        tokenizer_queue_capacity = config.tokenizer_queue_capacity,
        exact_route_mode = ?config.exact_route_mode,
        exact_route_workers = config.exact_route_workers,
        exact_route_timeout_ms = config.exact_route_timeout_ms,
        exact_route_min_gain_tokens = config.exact_route_min_gain_tokens,
        exact_route_max_load_delta = config.exact_route_max_load_delta,
        exact_route_canary_bps = config.exact_route_canary_bps,
        exact_route_manifest = config.exact_route_manifest_path.is_some(),
        kv_event_mode = ?config.kv_event_mode,
        kv_event_sources = config.kv_event_sources.len(),
        kv_event_replay_limit = config.kv_event_replay_limit,
        snapshot_route_mode = ?config.snapshot_route_mode,
        snapshot_route_sources = config.snapshot_route_sources.len(),
        "mini-dynamo up: API :8000, metrics :9090"
    );
}

enum Exit {
    Api(Result<Result<(), std::io::Error>, tokio::task::JoinError>),
    Metrics(Result<Result<(), std::io::Error>, tokio::task::JoinError>),
    Signal,
}

async fn await_shutdown(
    task: &mut tokio::task::JoinHandle<Result<(), std::io::Error>>,
    name: &str,
) -> anyhow::Result<()> {
    if let Ok(result) = tokio::time::timeout(Duration::from_secs(15), task).await {
        result
            .with_context(|| format!("join {name} server"))?
            .with_context(|| format!("{name} server"))
    } else {
        tracing::warn!(server = name, "graceful shutdown timed out");
        Ok(())
    }
}

#[derive(Clone)]
struct MetricsState {
    proxy: Proxy,
    registry: Arc<Registry>,
}

async fn prometheus_metrics(State(state): State<MetricsState>) -> Response<Body> {
    let mut output = Vec::new();
    let encoder = TextEncoder::new();
    if encoder
        .encode(&state.registry.gather(), &mut output)
        .is_err()
    {
        return Response::builder()
            .status(StatusCode::INTERNAL_SERVER_ERROR)
            .body(Body::from("metrics encoding failed"))
            .expect("valid metrics error");
    }
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", encoder.format_type())
        .body(Body::from(output))
        .expect("valid metrics response")
}

async fn upstream_metrics(
    State(state): State<MetricsState>,
    Path(index): Path<usize>,
) -> Response<Body> {
    Proxy::upstream_metrics(State(state.proxy), Path(index)).await
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("install SIGTERM handler");
        tokio::select! {
            result = tokio::signal::ctrl_c() => { let _ = result; },
            _ = terminate.recv() => {},
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
