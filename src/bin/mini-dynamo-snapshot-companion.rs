use anyhow::Context;
use mini_dynamo::companion_service::{SingleEngineCompanionConfig, run_single_engine_companion};
use tokio::sync::watch;
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with(tracing_subscriber::fmt::layer().json())
        .init();

    let config = SingleEngineCompanionConfig::from_env()
        .context("invalid standalone snapshot companion configuration")?;
    tracing::info!(enabled = config.enabled(), "snapshot companion starting");
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let service = run_single_engine_companion(config, shutdown_rx);
    tokio::pin!(service);
    let result = tokio::select! {
        biased;
        result = &mut service => result,
        () = shutdown_signal() => {
            let _ = shutdown_tx.send(true);
            service.await
        }
    };
    match result {
        Ok(report) => {
            tracing::info!(report = ?report, "snapshot companion stopped");
            Ok(())
        }
        Err(error) => {
            tracing::error!(reason = error.reason(), "snapshot companion failed");
            Err(anyhow::anyhow!(
                "snapshot companion failed: {}",
                error.reason()
            ))
        }
    }
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
