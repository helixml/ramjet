//! Bounded accept-loop supervision for snapshot companion sessions.
//!
//! This module deliberately knows nothing about snapshot framing, peer
//! credentials, or index construction. It owns listener admission and task
//! lifetime only: at most two session futures run at once, every admitted
//! connection gets one absolute deadline, excess connections are closed
//! immediately, and shutdown drops all in-flight session futures.

use std::{future::Future, sync::Arc, time::Duration};

use thiserror::Error;
use tokio::{
    net::{UnixListener, UnixStream},
    sync::{Semaphore, watch},
    task::{JoinError, JoinSet},
    time::{Instant, timeout_at},
};

pub const MAX_ACTIVE_SNAPSHOT_CLIENTS: usize = 2;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SnapshotSupervisorConfig {
    pub session_timeout: Duration,
}

impl SnapshotSupervisorConfig {
    /// Create a supervisor configuration with one end-to-end session timeout.
    ///
    /// # Errors
    ///
    /// Returns [`SnapshotSupervisorError::InvalidTimeout`] for a zero timeout.
    pub const fn new(session_timeout: Duration) -> Result<Self, SnapshotSupervisorError> {
        if session_timeout.is_zero() {
            return Err(SnapshotSupervisorError::InvalidTimeout);
        }
        Ok(Self { session_timeout })
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SnapshotSupervisorReport {
    /// Connections returned by the kernel accept queue.
    pub accepted_connections: u64,
    /// Connections admitted into one of the two session slots.
    pub sessions_started: u64,
    pub sessions_completed: u64,
    pub sessions_failed: u64,
    pub sessions_timed_out: u64,
    pub sessions_cancelled: u64,
    /// Connections closed immediately because both slots were occupied.
    pub connections_rejected_capacity: u64,
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum SnapshotSupervisorError {
    #[error("snapshot supervisor session timeout must be positive")]
    InvalidTimeout,
    #[error("snapshot supervisor deadline is out of range")]
    InvalidDeadline,
    #[error("snapshot supervisor listener failed")]
    Listener,
}

impl SnapshotSupervisorError {
    #[must_use]
    pub const fn reason(self) -> &'static str {
        match self {
            Self::InvalidTimeout => "invalid_timeout",
            Self::InvalidDeadline => "invalid_deadline",
            Self::Listener => "listener_failed",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SessionOutcome {
    Completed,
    Failed,
    TimedOut,
    Cancelled,
}

/// Run a bounded snapshot-session accept loop until shutdown.
///
/// The handler receives the absolute deadline that also encloses it. Protocol
/// layers should reuse that deadline for their own bounded sub-operations and
/// must not create fresh relative deadlines. A handler error is intentionally
/// reduced to a content-free failure count.
///
/// When both slots are occupied, a newly accepted stream is dropped in the
/// accept loop without spawning work or waiting for a permit. Session task
/// completion, error, panic, timeout, and cancellation all release their owned
/// permits.
///
/// # Errors
///
/// Returns [`SnapshotSupervisorError`] for an invalid configuration/deadline
/// or listener failure. Existing sessions are cancelled before returning a
/// listener error.
pub async fn supervise_snapshot_sessions<H, Fut, E>(
    listener: UnixListener,
    config: SnapshotSupervisorConfig,
    mut shutdown: watch::Receiver<bool>,
    handler: H,
) -> Result<SnapshotSupervisorReport, SnapshotSupervisorError>
where
    H: Fn(UnixStream, Instant) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<(), E>> + Send + 'static,
    E: Send + 'static,
{
    if config.session_timeout.is_zero() {
        return Err(SnapshotSupervisorError::InvalidTimeout);
    }

    let slots = Arc::new(Semaphore::new(MAX_ACTIVE_SNAPSHOT_CLIENTS));
    let handler = Arc::new(handler);
    let (cancel_sender, _) = watch::channel(false);
    let mut tasks = JoinSet::new();
    let mut report = SnapshotSupervisorReport::default();
    let mut listener_error = None;

    loop {
        tokio::select! {
            biased;
            () = wait_for_shutdown(&mut shutdown) => break,
            joined = tasks.join_next(), if !tasks.is_empty() => {
                if let Some(joined) = joined {
                    record_joined(&mut report, &joined);
                }
            }
            accepted = listener.accept() => {
                let Ok((stream, _)) = accepted else {
                    listener_error = Some(SnapshotSupervisorError::Listener);
                    break;
                };
                report.accepted_connections = report.accepted_connections.saturating_add(1);
                let Ok(permit) = Arc::clone(&slots).try_acquire_owned() else {
                    report.connections_rejected_capacity =
                        report.connections_rejected_capacity.saturating_add(1);
                    drop(stream);
                    continue;
                };
                let Some(deadline) = Instant::now().checked_add(config.session_timeout) else {
                    drop(permit);
                    drop(stream);
                    listener_error = Some(SnapshotSupervisorError::InvalidDeadline);
                    break;
                };
                report.sessions_started = report.sessions_started.saturating_add(1);
                let handler = Arc::clone(&handler);
                let mut cancelled = cancel_sender.subscribe();
                tasks.spawn(async move {
                    let _permit = permit;
                    let session = async move { handler(stream, deadline).await };
                    tokio::select! {
                        biased;
                        () = wait_for_shutdown(&mut cancelled) => SessionOutcome::Cancelled,
                        result = timeout_at(deadline, session) => match result {
                            Ok(Ok(())) => SessionOutcome::Completed,
                            Ok(Err(_)) => SessionOutcome::Failed,
                            Err(_) => SessionOutcome::TimedOut,
                        },
                    }
                });
            }
        }
    }

    // Wake every session wrapper. The selected cancellation branch drops the
    // handler future and its owned stream before returning the permit.
    let _ = cancel_sender.send(true);
    while let Some(joined) = tasks.join_next().await {
        record_joined(&mut report, &joined);
    }

    if let Some(error) = listener_error {
        Err(error)
    } else {
        Ok(report)
    }
}

async fn wait_for_shutdown(shutdown: &mut watch::Receiver<bool>) {
    loop {
        if *shutdown.borrow() {
            return;
        }
        if shutdown.changed().await.is_err() {
            return;
        }
    }
}

fn record_joined(
    report: &mut SnapshotSupervisorReport,
    joined: &Result<SessionOutcome, JoinError>,
) {
    match joined {
        Ok(SessionOutcome::Completed) => {
            report.sessions_completed = report.sessions_completed.saturating_add(1);
        }
        Ok(SessionOutcome::TimedOut) => {
            report.sessions_timed_out = report.sessions_timed_out.saturating_add(1);
        }
        Ok(SessionOutcome::Cancelled) => {
            report.sessions_cancelled = report.sessions_cancelled.saturating_add(1);
        }
        Ok(SessionOutcome::Failed) | Err(_) => {
            report.sessions_failed = report.sessions_failed.saturating_add(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        convert::Infallible,
        fs,
        os::unix::fs::PermissionsExt,
        path::{Path, PathBuf},
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
    };

    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        sync::{Barrier, Notify, oneshot},
        time::timeout,
    };

    use super::*;

    static TEST_SEQUENCE: AtomicUsize = AtomicUsize::new(1);

    struct TestSocket {
        directory: PathBuf,
        path: PathBuf,
    }

    impl TestSocket {
        fn new() -> (Self, UnixListener) {
            let sequence = TEST_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let directory =
                std::env::temp_dir().join(format!("mdss-{:x}-{sequence:x}", std::process::id()));
            fs::create_dir(&directory).unwrap();
            fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
            let path = directory.join("supervisor.sock");
            let listener = UnixListener::bind(&path).unwrap();
            (Self { directory, path }, listener)
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TestSocket {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.directory);
        }
    }

    fn config(timeout: Duration) -> SnapshotSupervisorConfig {
        SnapshotSupervisorConfig::new(timeout).unwrap()
    }

    async fn connect(path: &Path) -> UnixStream {
        timeout(Duration::from_secs(1), UnixStream::connect(path))
            .await
            .unwrap()
            .unwrap()
    }

    #[tokio::test]
    async fn two_clients_run_concurrently_and_complete_independently() {
        let (socket, listener) = TestSocket::new();
        let (shutdown_sender, shutdown) = watch::channel(false);
        let entered = Arc::new(Barrier::new(3));
        let (release_sender, release) = watch::channel(false);
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));

        let supervisor = {
            let entered = Arc::clone(&entered);
            let release = release.clone();
            let active = Arc::clone(&active);
            let peak = Arc::clone(&peak);
            tokio::spawn(supervise_snapshot_sessions(
                listener,
                config(Duration::from_secs(2)),
                shutdown,
                move |mut stream, _| {
                    let entered = Arc::clone(&entered);
                    let mut release = release.clone();
                    let active = Arc::clone(&active);
                    let peak = Arc::clone(&peak);
                    async move {
                        let now = active.fetch_add(1, Ordering::SeqCst) + 1;
                        peak.fetch_max(now, Ordering::SeqCst);
                        entered.wait().await;
                        wait_for_shutdown(&mut release).await;
                        stream.write_all(b"ok").await.unwrap();
                        active.fetch_sub(1, Ordering::SeqCst);
                        Ok::<_, Infallible>(())
                    }
                },
            ))
        };

        let mut first = connect(socket.path()).await;
        let mut second = connect(socket.path()).await;
        entered.wait().await;
        assert_eq!(active.load(Ordering::SeqCst), 2);
        assert_eq!(peak.load(Ordering::SeqCst), 2);
        release_sender.send(true).unwrap();
        let mut response = [0_u8; 2];
        first.read_exact(&mut response).await.unwrap();
        second.read_exact(&mut response).await.unwrap();
        while active.load(Ordering::SeqCst) != 0 {
            tokio::task::yield_now().await;
        }
        shutdown_sender.send(true).unwrap();
        let report = supervisor.await.unwrap().unwrap();
        assert_eq!(report.sessions_started, 2);
        assert_eq!(report.sessions_completed, 2);
        assert_eq!(report.connections_rejected_capacity, 0);
    }

    #[tokio::test]
    async fn stalled_hello_times_out_and_releases_its_slot() {
        let (socket, listener) = TestSocket::new();
        let (shutdown_sender, shutdown) = watch::channel(false);
        let supervisor = tokio::spawn(supervise_snapshot_sessions(
            listener,
            config(Duration::from_millis(40)),
            shutdown,
            |mut stream, deadline| async move {
                assert!(deadline > Instant::now());
                let mut hello = [0_u8; 1];
                stream.read_exact(&mut hello).await.map_err(|_| ())?;
                Ok::<_, ()>(())
            },
        ));

        let mut stalled = connect(socket.path()).await;
        let mut eof = [0_u8; 1];
        assert_eq!(
            timeout(Duration::from_secs(1), stalled.read(&mut eof))
                .await
                .unwrap()
                .unwrap(),
            0
        );

        // The timed-out task has released its permit, so a later client is
        // admitted rather than capacity-rejected.
        let mut replacement = connect(socket.path()).await;
        replacement.write_all(b"h").await.unwrap();
        assert_eq!(
            timeout(Duration::from_secs(1), replacement.read(&mut eof))
                .await
                .unwrap()
                .unwrap(),
            0
        );
        shutdown_sender.send(true).unwrap();
        let report = supervisor.await.unwrap().unwrap();
        assert_eq!(report.sessions_started, 2);
        assert_eq!(report.sessions_timed_out, 1);
        assert_eq!(report.sessions_completed, 1);
    }

    #[tokio::test]
    async fn third_client_is_closed_without_blocking_accept() {
        let (socket, listener) = TestSocket::new();
        let (shutdown_sender, shutdown) = watch::channel(false);
        let occupied = Arc::new(Barrier::new(3));
        let release = Arc::new(Notify::new());
        let supervisor = {
            let occupied = Arc::clone(&occupied);
            let release = Arc::clone(&release);
            tokio::spawn(supervise_snapshot_sessions(
                listener,
                config(Duration::from_secs(2)),
                shutdown,
                move |_stream, _| {
                    let occupied = Arc::clone(&occupied);
                    let release = Arc::clone(&release);
                    async move {
                        occupied.wait().await;
                        release.notified().await;
                        Ok::<_, Infallible>(())
                    }
                },
            ))
        };

        let _first = connect(socket.path()).await;
        let _second = connect(socket.path()).await;
        occupied.wait().await;
        let mut third = connect(socket.path()).await;
        let mut byte = [0_u8; 1];
        assert_eq!(
            timeout(Duration::from_secs(1), third.read(&mut byte))
                .await
                .unwrap()
                .unwrap(),
            0
        );
        release.notify_waiters();
        shutdown_sender.send(true).unwrap();
        let report = supervisor.await.unwrap().unwrap();
        assert_eq!(report.sessions_started, 2);
        assert_eq!(report.connections_rejected_capacity, 1);
        assert_eq!(report.accepted_connections, 3);
    }

    #[tokio::test]
    async fn shutdown_drops_stalled_sessions_and_releases_all_slots() {
        let (socket, listener) = TestSocket::new();
        let (shutdown_sender, shutdown) = watch::channel(false);
        let (entered_sender, entered_receiver) = oneshot::channel();
        let entered_sender = Arc::new(std::sync::Mutex::new(Some(entered_sender)));
        let supervisor = {
            let entered_sender = Arc::clone(&entered_sender);
            tokio::spawn(supervise_snapshot_sessions(
                listener,
                config(Duration::from_secs(30)),
                shutdown,
                move |mut stream, _| {
                    let entered_sender = Arc::clone(&entered_sender);
                    async move {
                        if let Some(sender) = entered_sender.lock().unwrap().take() {
                            let _ = sender.send(());
                        }
                        let mut byte = [0_u8; 1];
                        stream.read_exact(&mut byte).await.map_err(|_| ())?;
                        Ok::<_, ()>(())
                    }
                },
            ))
        };

        let mut client = connect(socket.path()).await;
        entered_receiver.await.unwrap();
        shutdown_sender.send(true).unwrap();
        let report = timeout(Duration::from_secs(1), supervisor)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(report.sessions_cancelled, 1);
        let mut byte = [0_u8; 1];
        assert_eq!(client.read(&mut byte).await.unwrap(), 0);
    }
}
