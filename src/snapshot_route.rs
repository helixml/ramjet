//! Off-by-default LB ownership of standalone snapshot companion sessions.
//!
//! Every protected file and socket parent is validated for every upstream
//! before the first reconnect task is spawned. A failure therefore cannot
//! leave a partially configured exact inventory set. Approximate serving and
//! upstream health are deliberately outside this runtime.

use std::{sync::Arc, time::Duration};

use thiserror::Error;
use tokio::{sync::watch, task::JoinHandle};

use crate::{
    companion_attestation::{
        CompanionAttestationError, load_authenticated_engine_incarnation,
        load_companion_digest_secret,
    },
    config::{Config, SnapshotRouteMode},
    digest_index::{DigestIndexLimits, SnapshotGroupKey},
    exact_route_inventory::ExactRouteInventory,
    kv_snapshot::SnapshotLimits,
    kv_wire::KvWireLimits,
    snapshot_actor::SnapshotActorLimits,
    snapshot_consumer::{SnapshotConsumer, SnapshotConsumerConfig},
    snapshot_reconnect::{
        SnapshotReconnectConfig, SnapshotReconnectError, SnapshotReconnectOwner,
        SnapshotReconnectReport,
    },
    snapshot_secret_file::{
        SnapshotSecretFileError, SnapshotSecretFilePolicy, load_snapshot_session_secret,
    },
    snapshot_session::SnapshotSessionLimits,
    snapshot_socket_path::SocketParentPolicy,
    snapshot_tail_wire::TailWireLimits,
};

const CHALLENGE_LEDGER_CAPACITY: usize = 4_096;
const SHUTDOWN_DEADLINE: Duration = Duration::from_secs(2);

pub struct SnapshotRouteConsumers {
    inventories: Arc<[ExactRouteInventory]>,
    shutdown: Option<watch::Sender<bool>>,
    tasks: Vec<JoinHandle<SnapshotReconnectReport>>,
}

#[derive(Debug, Error)]
pub enum SnapshotRouteStartError {
    #[error("snapshot route protected file validation failed")]
    Secret(#[from] SnapshotSecretFileError),
    #[error("snapshot route incarnation validation failed")]
    Attestation(#[from] CompanionAttestationError),
    #[error("snapshot route consumer initialization failed")]
    Consumer,
    #[error("snapshot route reconnect initialization failed")]
    Reconnect(#[from] SnapshotReconnectError),
}

impl SnapshotRouteStartError {
    #[must_use]
    pub const fn reason(&self) -> &'static str {
        match self {
            Self::Secret(error) => error.reason(),
            Self::Attestation(error) => error.reason(),
            Self::Consumer => "consumer_initialization",
            Self::Reconnect(error) => error.reason(),
        }
    }
}

impl SnapshotRouteConsumers {
    /// Validate all configured authorities, then start one bounded reconnect
    /// owner per upstream. Off mode performs no filesystem access or task spawn.
    ///
    /// # Errors
    ///
    /// Fails closed before spawning any owner when any protected file, socket
    /// parent, actor limit, or reconnect policy is invalid.
    pub fn start(config: &Config) -> Result<Self, SnapshotRouteStartError> {
        if config.snapshot_route_mode == SnapshotRouteMode::Off {
            return Ok(Self {
                inventories: Arc::from([]),
                shutdown: None,
                tasks: Vec::new(),
            });
        }

        let policy = SnapshotSecretFilePolicy {
            expected_owner_uid: config.snapshot_route_secret_owner_uid,
        };
        let mut prepared = Vec::with_capacity(config.snapshot_route_sources.len());
        let mut inventories = Vec::with_capacity(config.snapshot_route_sources.len());
        for source in &config.snapshot_route_sources {
            let session_secret = load_snapshot_session_secret(&source.session_secret_path, policy)?;
            let digest_secret = load_companion_digest_secret(&source.digest_secret_path, policy)?;
            let incarnation = load_authenticated_engine_incarnation(
                &source.attestation_path,
                policy,
                &digest_secret,
            )?;
            let consumer = Arc::new(
                SnapshotConsumer::new(
                    SnapshotConsumerConfig {
                        expected_peer_uid: source.companion_uid,
                        expected_engine_incarnation: incarnation,
                        minimum_snapshot_watermark: 0,
                        minimum_companion_generation: 1,
                        group: SnapshotGroupKey {
                            data_parallel_rank: source.data_parallel_rank,
                            group_idx: source.group_idx,
                        },
                        session_limits: SnapshotSessionLimits::default(),
                        snapshot_limits: SnapshotLimits::default(),
                        index_limits: DigestIndexLimits::default(),
                        tail_limits: TailWireLimits::default(),
                        event_limits: KvWireLimits::default(),
                    },
                    session_secret,
                    *digest_secret.as_bytes(),
                    SnapshotActorLimits::default(),
                )
                .map_err(|_| SnapshotRouteStartError::Consumer)?,
            );
            let reconnect = SnapshotReconnectConfig::new(
                source.socket_path.clone(),
                SocketParentPolicy {
                    owner_uid: source.companion_uid,
                },
                Duration::from_millis(config.snapshot_route_attempt_timeout_ms as u64),
                Duration::from_millis(config.snapshot_route_reconnect_min_ms as u64),
                Duration::from_millis(config.snapshot_route_reconnect_max_ms as u64),
                CHALLENGE_LEDGER_CAPACITY,
            )?;
            let publication = Arc::clone(consumer.publication());
            let (owner, _replacement) = SnapshotReconnectOwner::new(reconnect, consumer)?;
            inventories.push(ExactRouteInventory::snapshot(publication));
            prepared.push(owner);
        }

        let (shutdown, receiver) = watch::channel(false);
        let tasks = prepared
            .into_iter()
            .map(|owner| tokio::spawn(owner.run(receiver.clone())))
            .collect();
        Ok(Self {
            inventories: inventories.into(),
            shutdown: Some(shutdown),
            tasks,
        })
    }

    #[must_use]
    pub fn inventories(&self) -> Arc<[ExactRouteInventory]> {
        Arc::clone(&self.inventories)
    }

    /// Signal every owner immediately; joining remains separately bounded.
    pub fn request_shutdown(&self) {
        if let Some(shutdown) = &self.shutdown {
            let _ = shutdown.send(true);
        }
    }

    pub async fn shutdown(mut self) {
        self.request_shutdown();
        self.shutdown.take();
        for mut task in self.tasks {
            if tokio::time::timeout(SHUTDOWN_DEADLINE, &mut task)
                .await
                .is_err()
            {
                task.abort();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashMap,
        fs,
        os::unix::fs::{MetadataExt, PermissionsExt, chown},
        sync::atomic::{AtomicU64, Ordering},
    };

    use crate::{
        companion_attestation::{
            encode_authenticated_engine_incarnation, load_companion_digest_secret,
        },
        kv_snapshot::EngineIncarnation,
    };

    use super::*;

    static TEST_ID: AtomicU64 = AtomicU64::new(1);

    struct TestAuthority {
        directory: std::path::PathBuf,
        session: std::path::PathBuf,
        digest: std::path::PathBuf,
        attestation: std::path::PathBuf,
        socket: std::path::PathBuf,
        owner: u32,
    }

    impl TestAuthority {
        fn new() -> Self {
            let id = TEST_ID.fetch_add(1, Ordering::Relaxed);
            let directory = std::env::temp_dir().join(format!("mdsr-{}-{id}", std::process::id()));
            fs::create_dir(&directory).unwrap();
            fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
            let current_uid = fs::metadata(&directory).unwrap().uid();
            let owner = if current_uid == 0 {
                12_001
            } else {
                current_uid
            };
            let session = directory.join("session");
            let digest = directory.join("digest");
            let attestation = directory.join("attestation");
            let socket = directory.join("snapshot.sock");
            fs::write(&session, [0x31; 32]).unwrap();
            fs::write(&digest, [0x41; 32]).unwrap();
            for path in [&session, &digest] {
                fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
            }
            if current_uid == 0 {
                for path in [&directory, &session, &digest] {
                    chown(path, Some(owner), None).unwrap();
                }
            }
            let policy = SnapshotSecretFilePolicy {
                expected_owner_uid: owner,
            };
            let secret = load_companion_digest_secret(&digest, policy).unwrap();
            let encoded = encode_authenticated_engine_incarnation(
                &EngineIncarnation {
                    engine_id: "engine-a".into(),
                    model_revision: "revision-a".into(),
                    image_digest: "sha256:image-a".into(),
                    process_started_unix_ns: 42,
                    attestation_sha256: vec![7; 32],
                },
                &secret,
            )
            .unwrap();
            fs::write(&attestation, encoded).unwrap();
            fs::set_permissions(&attestation, fs::Permissions::from_mode(0o600)).unwrap();
            if current_uid == 0 {
                chown(&attestation, Some(owner), None).unwrap();
            }
            Self {
                directory,
                session,
                digest,
                attestation,
                socket,
                owner,
            }
        }

        fn config(&self) -> Config {
            let values = HashMap::from([
                ("DS4_UPSTREAM".to_owned(), "http://a:1".to_owned()),
                ("DS4_TOKENIZER_MODE".to_owned(), "local-shadow".to_owned()),
                (
                    "DS4_TOKENIZER_PATH".to_owned(),
                    "/models/tokenizer.json".to_owned(),
                ),
                ("DS4_TOKENIZER_SHA256".to_owned(), "a".repeat(64)),
                ("DS4_EXACT_ROUTE_MODE".to_owned(), "shadow".to_owned()),
                (
                    "DS4_EXACT_ROUTE_MANIFEST_PATH".to_owned(),
                    "/compat/manifest.json".to_owned(),
                ),
                ("DS4_EXACT_ROUTE_MANIFEST_SHA256".to_owned(), "b".repeat(64)),
                ("DS4_SNAPSHOT_ROUTE_MODE".to_owned(), "shadow".to_owned()),
                (
                    "DS4_SNAPSHOT_ROUTE_SOCKET_PATHS".to_owned(),
                    self.socket.display().to_string(),
                ),
                (
                    "DS4_SNAPSHOT_ROUTE_COMPANION_UIDS".to_owned(),
                    self.owner.to_string(),
                ),
                (
                    "DS4_SNAPSHOT_ROUTE_SESSION_SECRET_PATHS".to_owned(),
                    self.session.display().to_string(),
                ),
                (
                    "DS4_SNAPSHOT_ROUTE_DIGEST_SECRET_PATHS".to_owned(),
                    self.digest.display().to_string(),
                ),
                (
                    "DS4_SNAPSHOT_ROUTE_ATTESTATION_PATHS".to_owned(),
                    self.attestation.display().to_string(),
                ),
                ("DS4_SNAPSHOT_ROUTE_GROUPS".to_owned(), "0:0".to_owned()),
                (
                    "DS4_SNAPSHOT_ROUTE_SECRET_OWNER_UID".to_owned(),
                    self.owner.to_string(),
                ),
                (
                    "DS4_SNAPSHOT_ROUTE_RECONNECT_MIN_MS".to_owned(),
                    "1".to_owned(),
                ),
                (
                    "DS4_SNAPSHOT_ROUTE_RECONNECT_MAX_MS".to_owned(),
                    "2".to_owned(),
                ),
            ]);
            Config::from_lookup(|key| values.get(key).cloned()).unwrap()
        }
    }

    impl Drop for TestAuthority {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.directory);
        }
    }

    #[test]
    fn off_mode_does_not_touch_files_or_spawn_tasks() {
        let config = Config::from_lookup(|_| None).unwrap();
        let consumers = SnapshotRouteConsumers::start(&config).unwrap();
        assert!(consumers.inventories().is_empty());
        assert!(consumers.tasks.is_empty());
        assert!(consumers.shutdown.is_none());
    }

    #[tokio::test]
    async fn valid_authority_starts_one_bounded_unpublished_owner() {
        let authority = TestAuthority::new();
        let consumers = SnapshotRouteConsumers::start(&authority.config()).unwrap();
        assert_eq!(consumers.inventories().len(), 1);
        assert!(!consumers.inventories()[0].ready());
        assert_eq!(consumers.tasks.len(), 1);
        consumers.shutdown().await;
    }

    #[tokio::test]
    async fn invalid_authority_fails_before_any_owner_is_returned() {
        let authority = TestAuthority::new();
        fs::set_permissions(&authority.attestation, fs::Permissions::from_mode(0o666)).unwrap();
        let error = SnapshotRouteConsumers::start(&authority.config())
            .err()
            .unwrap();
        assert_eq!(error.reason(), "unsafe_permissions");
        assert!(!authority.socket.exists());
    }
}
