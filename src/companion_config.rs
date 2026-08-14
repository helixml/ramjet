//! Typed, content-free runtime configuration for the snapshot companion.
//!
//! This module performs syntactic startup validation only. Filesystem ownership,
//! permissions, symlinks, and inode identity remain the responsibility of the
//! hardened socket and secret-file modules at open/bind time.

use std::path::{Component, Path, PathBuf};
use std::time::Duration;
use std::{env, fmt};

use thiserror::Error;
use url::Url;

const MAX_CLIENTS: usize = 2;
const MAX_SOURCES: usize = 2;
const MAX_QUEUE_CAPACITY: usize = 65_536;
const MAX_QUEUE_BYTES: usize = 64 * 1024 * 1024;
const MAX_DEADLINE_MS: u64 = 15 * 60 * 1_000;
const MAX_TOPIC_BYTES: usize = 256;
const MAX_SOCKET_PATH_BYTES: usize = 64;
const MAX_SECRET_PATH_BYTES: usize = 4_096;
const MAX_ENDPOINT_BYTES: usize = 512;
const MAX_SNAPSHOT_FRAME_BYTES: usize = 32 * 1024 * 1024;
const MAX_TAIL_FRAME_BYTES: usize = 16 * 1024 * 1024;
const MAX_BATCH_PAYLOAD_BYTES: usize = 16 * 1024 * 1024;
const MAX_BATCH_EVENTS: usize = 4_096;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SnapshotCompanionMode {
    #[default]
    Off,
    Serve,
}

#[derive(Clone, Eq, PartialEq)]
pub struct SnapshotCompanionSource {
    pub live_endpoint: String,
    pub replay_endpoint: String,
}

impl fmt::Debug for SnapshotCompanionSource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SnapshotCompanionSource")
            .field("live_endpoint", &"[REDACTED]")
            .field("replay_endpoint", &"[REDACTED]")
            .finish()
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct SnapshotCompanionConfig {
    pub mode: SnapshotCompanionMode,
    pub socket_path: Option<PathBuf>,
    pub companion_uid: Option<u32>,
    pub client_uid: Option<u32>,
    pub secret_path: Option<PathBuf>,
    pub secret_owner_uid: u32,
    pub max_clients: usize,
    pub tail_queue_capacity: usize,
    pub tail_queue_max_bytes: usize,
    pub snapshot_deadline: Duration,
    pub tail_idle_deadline: Duration,
    pub shutdown_deadline: Duration,
    pub max_snapshot_frame_bytes: usize,
    pub max_tail_frame_bytes: usize,
    pub max_batch_payload_bytes: usize,
    pub max_batch_events: usize,
    pub event_topic: String,
    pub sources: Vec<SnapshotCompanionSource>,
}

impl fmt::Debug for SnapshotCompanionConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SnapshotCompanionConfig")
            .field("mode", &self.mode)
            .field(
                "socket_path",
                &self.socket_path.as_ref().map(|_| "[REDACTED]"),
            )
            .field("companion_uid", &self.companion_uid.map(|_| "[REDACTED]"))
            .field("client_uid", &self.client_uid.map(|_| "[REDACTED]"))
            .field(
                "secret_path",
                &self.secret_path.as_ref().map(|_| "[REDACTED]"),
            )
            .field("secret_owner_uid", &"[REDACTED]")
            .field("max_clients", &self.max_clients)
            .field("tail_queue_capacity", &self.tail_queue_capacity)
            .field("tail_queue_max_bytes", &self.tail_queue_max_bytes)
            .field("snapshot_deadline", &self.snapshot_deadline)
            .field("tail_idle_deadline", &self.tail_idle_deadline)
            .field("shutdown_deadline", &self.shutdown_deadline)
            .field("max_snapshot_frame_bytes", &self.max_snapshot_frame_bytes)
            .field("max_tail_frame_bytes", &self.max_tail_frame_bytes)
            .field("max_batch_payload_bytes", &self.max_batch_payload_bytes)
            .field("max_batch_events", &self.max_batch_events)
            .field("event_topic_bytes", &self.event_topic.len())
            .field("source_count", &self.sources.len())
            .finish()
    }
}

impl SnapshotCompanionConfig {
    /// Load the companion's public environment contract.
    ///
    /// # Errors
    ///
    /// Returns a content-free error naming only the invalid setting and its
    /// required shape. Secret and socket paths are never included in errors.
    pub fn from_env() -> Result<Self, SnapshotCompanionConfigError> {
        Self::from_lookup(|key| env::var(key).ok())
    }

    /// Deterministic lookup-based constructor used by tests and embedders.
    ///
    /// # Errors
    ///
    /// Returns [`SnapshotCompanionConfigError`] for an unknown mode, missing
    /// serve-mode setting, invalid path/UID/bound/deadline, unsafe credential
    /// relationship, malformed endpoint, or mismatched endpoint cardinality.
    #[allow(clippy::too_many_lines)]
    pub fn from_lookup(
        mut get: impl FnMut(&str) -> Option<String>,
    ) -> Result<Self, SnapshotCompanionConfigError> {
        let mode = match get("MD_SNAPSHOT_COMPANION_MODE")
            .as_deref()
            .unwrap_or("off")
        {
            "off" => SnapshotCompanionMode::Off,
            "serve" => SnapshotCompanionMode::Serve,
            _ => return Err(invalid("MD_SNAPSHOT_COMPANION_MODE", "off or serve")),
        };

        let max_clients = bounded_usize(&mut get, "MD_SNAPSHOT_MAX_CLIENTS", 2, 1, MAX_CLIENTS)?;
        let tail_queue_capacity = bounded_usize(
            &mut get,
            "MD_SNAPSHOT_TAIL_QUEUE_CAPACITY",
            1_024,
            1,
            MAX_QUEUE_CAPACITY,
        )?;
        let tail_queue_max_bytes = bounded_usize(
            &mut get,
            "MD_SNAPSHOT_TAIL_QUEUE_MAX_BYTES",
            16 * 1024 * 1024,
            1,
            MAX_QUEUE_BYTES,
        )?;
        let snapshot_deadline = duration_ms(&mut get, "MD_SNAPSHOT_DEADLINE_MS", 3_000)?;
        let tail_idle_deadline =
            duration_ms(&mut get, "MD_SNAPSHOT_TAIL_IDLE_DEADLINE_MS", 30_000)?;
        let shutdown_deadline = duration_ms(&mut get, "MD_SNAPSHOT_SHUTDOWN_DEADLINE_MS", 5_000)?;
        let max_snapshot_frame_bytes = bounded_usize(
            &mut get,
            "MD_SNAPSHOT_MAX_FRAME_BYTES",
            MAX_SNAPSHOT_FRAME_BYTES,
            1,
            MAX_SNAPSHOT_FRAME_BYTES,
        )?;
        let max_tail_frame_bytes = bounded_usize(
            &mut get,
            "MD_SNAPSHOT_MAX_TAIL_FRAME_BYTES",
            8 * 1024 * 1024 + 4 * 1024,
            1,
            MAX_TAIL_FRAME_BYTES,
        )?;
        let max_batch_payload_bytes = bounded_usize(
            &mut get,
            "MD_SNAPSHOT_MAX_BATCH_PAYLOAD_BYTES",
            8 * 1024 * 1024,
            1,
            MAX_BATCH_PAYLOAD_BYTES,
        )?;
        let max_batch_events = bounded_usize(
            &mut get,
            "MD_SNAPSHOT_MAX_BATCH_EVENTS",
            MAX_BATCH_EVENTS,
            1,
            MAX_BATCH_EVENTS,
        )?;
        if max_batch_payload_bytes >= max_tail_frame_bytes {
            return Err(invalid(
                "MD_SNAPSHOT_MAX_BATCH_PAYLOAD_BYTES",
                "a positive byte bound smaller than the tail frame bound",
            ));
        }
        if tail_queue_max_bytes < max_batch_payload_bytes {
            return Err(invalid(
                "MD_SNAPSHOT_TAIL_QUEUE_MAX_BYTES",
                "a byte bound at least as large as one batch payload",
            ));
        }
        let secret_owner_uid =
            parse_u32(&mut get, "MD_SNAPSHOT_SECRET_OWNER_UID", Some(0))?.unwrap_or(0);

        if mode == SnapshotCompanionMode::Off {
            return Ok(Self {
                mode,
                socket_path: None,
                companion_uid: None,
                client_uid: None,
                secret_path: None,
                secret_owner_uid,
                max_clients,
                tail_queue_capacity,
                tail_queue_max_bytes,
                snapshot_deadline,
                tail_idle_deadline,
                shutdown_deadline,
                max_snapshot_frame_bytes,
                max_tail_frame_bytes,
                max_batch_payload_bytes,
                max_batch_events,
                event_topic: String::new(),
                sources: Vec::new(),
            });
        }

        let socket_path =
            required_path(&mut get, "MD_SNAPSHOT_SOCKET_PATH", MAX_SOCKET_PATH_BYTES)?;
        let secret_path =
            required_path(&mut get, "MD_SNAPSHOT_SECRET_PATH", MAX_SECRET_PATH_BYTES)?;
        if socket_path == secret_path {
            return Err(invalid(
                "MD_SNAPSHOT_SECRET_PATH",
                "an absolute normalized path distinct from the socket",
            ));
        }
        let companion_uid = required_non_root_uid(&mut get, "MD_SNAPSHOT_COMPANION_UID")?;
        let client_uid = required_non_root_uid(&mut get, "MD_SNAPSHOT_CLIENT_UID")?;
        if companion_uid == client_uid {
            return Err(invalid(
                "MD_SNAPSHOT_CLIENT_UID",
                "a non-root UID distinct from the companion UID",
            ));
        }

        let live = endpoint_list(&mut get, "MD_SNAPSHOT_LIVE_ENDPOINTS")?;
        let replay = endpoint_list(&mut get, "MD_SNAPSHOT_REPLAY_ENDPOINTS")?;
        if live.is_empty() || live.len() > MAX_SOURCES || live.len() != replay.len() {
            return Err(invalid(
                "MD_SNAPSHOT_LIVE_ENDPOINTS",
                "one or two live endpoints with one replay endpoint each",
            ));
        }
        let event_topic = get("MD_SNAPSHOT_EVENT_TOPIC").unwrap_or_default();
        if event_topic.len() > MAX_TOPIC_BYTES {
            return Err(invalid("MD_SNAPSHOT_EVENT_TOPIC", "at most 256 bytes"));
        }
        let sources = live
            .into_iter()
            .zip(replay)
            .map(|(live_endpoint, replay_endpoint)| SnapshotCompanionSource {
                live_endpoint,
                replay_endpoint,
            })
            .collect();

        Ok(Self {
            mode,
            socket_path: Some(socket_path),
            companion_uid: Some(companion_uid),
            client_uid: Some(client_uid),
            secret_path: Some(secret_path),
            secret_owner_uid,
            max_clients,
            tail_queue_capacity,
            tail_queue_max_bytes,
            snapshot_deadline,
            tail_idle_deadline,
            shutdown_deadline,
            max_snapshot_frame_bytes,
            max_tail_frame_bytes,
            max_batch_payload_bytes,
            max_batch_events,
            event_topic,
            sources,
        })
    }

    #[must_use]
    pub fn enabled(&self) -> bool {
        self.mode == SnapshotCompanionMode::Serve
    }
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum SnapshotCompanionConfigError {
    #[error("missing snapshot companion setting {key}: expected {reason}")]
    Missing {
        key: &'static str,
        reason: &'static str,
    },
    #[error("invalid snapshot companion setting {key}: expected {reason}")]
    Invalid {
        key: &'static str,
        reason: &'static str,
    },
}

impl SnapshotCompanionConfigError {
    #[must_use]
    pub const fn reason(self) -> &'static str {
        match self {
            Self::Missing { .. } => "missing_setting",
            Self::Invalid { .. } => "invalid_setting",
        }
    }
}

fn required_path(
    get: &mut impl FnMut(&str) -> Option<String>,
    key: &'static str,
    max_bytes: usize,
) -> Result<PathBuf, SnapshotCompanionConfigError> {
    let raw = get(key).filter(|value| !value.is_empty()).ok_or(
        SnapshotCompanionConfigError::Missing {
            key,
            reason: "an absolute normalized file path",
        },
    )?;
    let normalized_text = raw.starts_with('/')
        && !raw.ends_with('/')
        && raw[1..]
            .split('/')
            .all(|component| !component.is_empty() && !matches!(component, "." | ".."));
    let within_limit = raw.len() <= max_bytes;
    let path = PathBuf::from(raw);
    if !within_limit || !normalized_text || !valid_absolute_file_path(&path) {
        return Err(invalid(key, "an absolute normalized file path"));
    }
    Ok(path)
}

fn valid_absolute_file_path(path: &Path) -> bool {
    path.is_absolute()
        && path.file_name().is_some()
        && path
            .components()
            .all(|component| matches!(component, Component::RootDir | Component::Normal(_)))
}

fn required_non_root_uid(
    get: &mut impl FnMut(&str) -> Option<String>,
    key: &'static str,
) -> Result<u32, SnapshotCompanionConfigError> {
    let uid = parse_u32(get, key, None)?.ok_or(SnapshotCompanionConfigError::Missing {
        key,
        reason: "a non-zero numeric UID",
    })?;
    if uid == 0 {
        return Err(invalid(key, "a non-zero numeric UID"));
    }
    Ok(uid)
}

fn parse_u32(
    get: &mut impl FnMut(&str) -> Option<String>,
    key: &'static str,
    fallback: Option<u32>,
) -> Result<Option<u32>, SnapshotCompanionConfigError> {
    let Some(raw) = get(key).filter(|value| !value.is_empty()) else {
        return Ok(fallback);
    };
    raw.parse::<u32>()
        .map(Some)
        .map_err(|_| invalid(key, "a numeric UID"))
}

fn bounded_usize(
    get: &mut impl FnMut(&str) -> Option<String>,
    key: &'static str,
    fallback: usize,
    minimum: usize,
    maximum: usize,
) -> Result<usize, SnapshotCompanionConfigError> {
    let Some(raw) = get(key).filter(|value| !value.is_empty()) else {
        return Ok(fallback);
    };
    let value = raw
        .parse::<usize>()
        .map_err(|_| invalid(key, "a bounded positive integer"))?;
    if !(minimum..=maximum).contains(&value) {
        return Err(invalid(key, "a bounded positive integer"));
    }
    Ok(value)
}

fn duration_ms(
    get: &mut impl FnMut(&str) -> Option<String>,
    key: &'static str,
    fallback_ms: u64,
) -> Result<Duration, SnapshotCompanionConfigError> {
    let raw = get(key).filter(|value| !value.is_empty());
    let milliseconds = raw
        .as_deref()
        .map(str::parse::<u64>)
        .transpose()
        .map_err(|_| invalid(key, "1..=900000 milliseconds"))?
        .unwrap_or(fallback_ms);
    if milliseconds == 0 || milliseconds > MAX_DEADLINE_MS {
        return Err(invalid(key, "1..=900000 milliseconds"));
    }
    Ok(Duration::from_millis(milliseconds))
}

fn endpoint_list(
    get: &mut impl FnMut(&str) -> Option<String>,
    key: &'static str,
) -> Result<Vec<String>, SnapshotCompanionConfigError> {
    get(key)
        .unwrap_or_default()
        .split(',')
        .filter_map(|raw| {
            let value = raw.trim();
            (!value.is_empty()).then_some(value)
        })
        .map(|value| {
            let valid = value.len() <= MAX_ENDPOINT_BYTES
                && Url::parse(value).ok().is_some_and(|url| {
                    url.scheme() == "tcp"
                        && url.has_host()
                        && url.port().is_some()
                        && url.username().is_empty()
                        && url.password().is_none()
                        && matches!(url.path(), "" | "/")
                        && url.query().is_none()
                        && url.fragment().is_none()
                });
            if valid {
                Ok(value.to_owned())
            } else {
                Err(invalid(key, "tcp://host:port endpoints"))
            }
        })
        .collect()
}

const fn invalid(key: &'static str, reason: &'static str) -> SnapshotCompanionConfigError {
    SnapshotCompanionConfigError::Invalid { key, reason }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn configured() -> HashMap<&'static str, &'static str> {
        HashMap::from([
            ("MD_SNAPSHOT_COMPANION_MODE", "serve"),
            ("MD_SNAPSHOT_SOCKET_PATH", "/run/mini-dynamo/snapshot.sock"),
            ("MD_SNAPSHOT_COMPANION_UID", "12001"),
            ("MD_SNAPSHOT_CLIENT_UID", "12002"),
            ("MD_SNAPSHOT_SECRET_PATH", "/run/secrets/snapshot-session"),
            (
                "MD_SNAPSHOT_LIVE_ENDPOINTS",
                "tcp://engine-a:5557,tcp://engine-b:5557",
            ),
            (
                "MD_SNAPSHOT_REPLAY_ENDPOINTS",
                "tcp://engine-a:5558,tcp://engine-b:5558",
            ),
        ])
    }

    fn load(
        values: &HashMap<&str, &str>,
    ) -> Result<SnapshotCompanionConfig, SnapshotCompanionConfigError> {
        SnapshotCompanionConfig::from_lookup(|key| values.get(key).map(ToString::to_string))
    }

    #[test]
    fn defaults_are_safe_and_off() {
        let config = SnapshotCompanionConfig::from_lookup(|_| None).unwrap();
        assert_eq!(config.mode, SnapshotCompanionMode::Off);
        assert!(!config.enabled());
        assert!(config.socket_path.is_none());
        assert!(config.companion_uid.is_none());
        assert!(config.client_uid.is_none());
        assert!(config.secret_path.is_none());
        assert!(config.sources.is_empty());
        assert_eq!(config.max_clients, 2);
        assert_eq!(config.tail_queue_capacity, 1_024);
        assert_eq!(config.tail_queue_max_bytes, 16 * 1024 * 1024);
        assert_eq!(config.snapshot_deadline, Duration::from_secs(3));
        assert_eq!(config.tail_idle_deadline, Duration::from_secs(30));
        assert_eq!(config.shutdown_deadline, Duration::from_secs(5));
        assert_eq!(config.max_snapshot_frame_bytes, 32 * 1024 * 1024);
        assert_eq!(config.max_tail_frame_bytes, 8 * 1024 * 1024 + 4 * 1024);
        assert_eq!(config.max_batch_payload_bytes, 8 * 1024 * 1024);
        assert_eq!(config.max_batch_events, 4_096);
    }

    #[test]
    fn serve_mode_builds_typed_sources() {
        let config = load(&configured()).unwrap();
        assert!(config.enabled());
        assert_eq!(config.companion_uid, Some(12_001));
        assert_eq!(config.client_uid, Some(12_002));
        assert_eq!(config.sources.len(), 2);
        assert_eq!(config.sources[1].replay_endpoint, "tcp://engine-b:5558");
    }

    #[test]
    fn serve_mode_requires_paths_uids_and_endpoints() {
        for missing in [
            "MD_SNAPSHOT_SOCKET_PATH",
            "MD_SNAPSHOT_COMPANION_UID",
            "MD_SNAPSHOT_CLIENT_UID",
            "MD_SNAPSHOT_SECRET_PATH",
            "MD_SNAPSHOT_LIVE_ENDPOINTS",
        ] {
            let mut values = configured();
            values.remove(missing);
            assert!(load(&values).is_err(), "accepted missing {missing}");
        }
    }

    #[test]
    fn rejects_relative_or_non_normalized_paths_without_echoing_them() {
        for path in [
            "relative.sock",
            "/run/../snapshot.sock",
            "/run/./snapshot.sock",
            "/",
        ] {
            let mut values = configured();
            values.insert("MD_SNAPSHOT_SOCKET_PATH", path);
            let error = load(&values).unwrap_err();
            let rendered = error.to_string();
            assert!(!rendered.contains(path));
            assert!(matches!(
                error,
                SnapshotCompanionConfigError::Invalid {
                    key: "MD_SNAPSHOT_SOCKET_PATH",
                    ..
                }
            ));
        }

        let too_long = format!("/run/{}.sock", "x".repeat(MAX_SOCKET_PATH_BYTES));
        let mut values = configured();
        values.insert(
            "MD_SNAPSHOT_SOCKET_PATH",
            Box::leak(too_long.into_boxed_str()),
        );
        assert!(load(&values).is_err());
    }

    #[test]
    fn rejects_root_equal_or_malformed_uids() {
        for (key, value) in [
            ("MD_SNAPSHOT_COMPANION_UID", "0"),
            ("MD_SNAPSHOT_CLIENT_UID", "nope"),
            ("MD_SNAPSHOT_CLIENT_UID", "12001"),
        ] {
            let mut values = configured();
            values.insert(key, value);
            assert!(load(&values).is_err());
        }
    }

    #[test]
    fn validates_client_queue_and_deadline_bounds_even_when_off() {
        for (key, value) in [
            ("MD_SNAPSHOT_MAX_CLIENTS", "0"),
            ("MD_SNAPSHOT_MAX_CLIENTS", "3"),
            ("MD_SNAPSHOT_TAIL_QUEUE_CAPACITY", "0"),
            ("MD_SNAPSHOT_TAIL_QUEUE_CAPACITY", "65537"),
            ("MD_SNAPSHOT_TAIL_QUEUE_MAX_BYTES", "67108865"),
            ("MD_SNAPSHOT_DEADLINE_MS", "0"),
            ("MD_SNAPSHOT_TAIL_IDLE_DEADLINE_MS", "900001"),
            ("MD_SNAPSHOT_SHUTDOWN_DEADLINE_MS", "invalid"),
            ("MD_SNAPSHOT_MAX_FRAME_BYTES", "33554433"),
            ("MD_SNAPSHOT_MAX_TAIL_FRAME_BYTES", "16777217"),
            ("MD_SNAPSHOT_MAX_BATCH_PAYLOAD_BYTES", "16777217"),
            ("MD_SNAPSHOT_MAX_BATCH_EVENTS", "4097"),
        ] {
            let values = HashMap::from([(key, value)]);
            assert!(load(&values).is_err(), "accepted {key}={value}");
        }
    }

    #[test]
    fn decoded_batch_must_fit_inside_authenticated_tail_frame() {
        let mut values = configured();
        values.insert("MD_SNAPSHOT_MAX_TAIL_FRAME_BYTES", "1024");
        values.insert("MD_SNAPSHOT_MAX_BATCH_PAYLOAD_BYTES", "1024");
        assert!(matches!(
            load(&values),
            Err(SnapshotCompanionConfigError::Invalid {
                key: "MD_SNAPSHOT_MAX_BATCH_PAYLOAD_BYTES",
                ..
            })
        ));
    }

    #[test]
    fn one_max_batch_must_fit_inside_tail_queue_byte_budget() {
        let mut values = configured();
        values.insert("MD_SNAPSHOT_TAIL_QUEUE_MAX_BYTES", "4194304");
        assert!(matches!(
            load(&values),
            Err(SnapshotCompanionConfigError::Invalid {
                key: "MD_SNAPSHOT_TAIL_QUEUE_MAX_BYTES",
                ..
            })
        ));
    }

    #[test]
    fn debug_redacts_paths_uids_topics_and_endpoints() {
        let mut values = configured();
        values.insert("MD_SNAPSHOT_EVENT_TOPIC", "private-topic");
        let config = load(&values).unwrap();
        let debug = format!("{config:?}");
        for forbidden in [
            "/run/mini-dynamo/snapshot.sock",
            "/run/secrets/snapshot-session",
            "engine-a",
            "private-topic",
            "12001",
            "12002",
        ] {
            assert!(!debug.contains(forbidden), "debug leaked {forbidden}");
        }
    }

    #[test]
    fn endpoint_cardinality_and_shapes_are_exact() {
        for (key, value) in [
            ("MD_SNAPSHOT_REPLAY_ENDPOINTS", "tcp://engine-a:5558"),
            (
                "MD_SNAPSHOT_LIVE_ENDPOINTS",
                "tcp://a:1,tcp://b:2,tcp://c:3",
            ),
            ("MD_SNAPSHOT_LIVE_ENDPOINTS", "ipc:///tmp/events"),
            (
                "MD_SNAPSHOT_REPLAY_ENDPOINTS",
                "tcp://user@engine-a:5558,tcp://engine-b:5558",
            ),
        ] {
            let mut values = configured();
            values.insert(key, value);
            assert!(load(&values).is_err(), "accepted invalid endpoint set");
        }

        let oversized = format!("tcp://{}:5557", "a".repeat(MAX_ENDPOINT_BYTES));
        let mut values = configured();
        values.insert(
            "MD_SNAPSHOT_LIVE_ENDPOINTS",
            Box::leak(oversized.into_boxed_str()),
        );
        assert!(load(&values).is_err());
    }
}
