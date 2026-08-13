use std::{ffi::OsString, path::PathBuf};

use anyhow::{Context, bail};
use mini_dynamo::companion_service::{SingleEngineCompanionConfig, run_single_engine_companion};
use mini_dynamo::snapshot_socket_path::validate_published_socket;
use tokio::sync::watch;
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    match command(std::env::args_os().skip(1))? {
        Command::Run => run().await,
        Command::Healthcheck(path) => {
            validate_published_socket(&path).context("snapshot companion healthcheck failed")?;
            Ok(())
        }
    }
}

enum Command {
    Run,
    Healthcheck(PathBuf),
}

fn command(mut arguments: impl Iterator<Item = OsString>) -> anyhow::Result<Command> {
    let Some(first) = arguments.next() else {
        return Ok(Command::Run);
    };
    if first != "healthcheck" {
        bail!("invalid snapshot companion command");
    }
    let Some(path) = arguments.next() else {
        bail!("snapshot companion healthcheck requires one socket path");
    };
    if arguments.next().is_some() {
        bail!("snapshot companion healthcheck accepts exactly one socket path");
    }
    Ok(Command::Healthcheck(PathBuf::from(path)))
}

async fn run() -> anyhow::Result<()> {
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with(tracing_subscriber::fmt::layer().json())
        .init();

    let config = SingleEngineCompanionConfig::from_env()
        .context("invalid standalone snapshot companion configuration")?;
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        enabled = config.enabled(),
        "snapshot companion starting"
    );
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
