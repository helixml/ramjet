use std::{
    collections::HashSet,
    env, fmt,
    path::{Component, Path, PathBuf},
};

use thiserror::Error;
use url::Url;

const MAX_SNAPSHOT_ROUTE_SOCKET_PATH_BYTES: usize = 64;
const MAX_SNAPSHOT_ROUTE_PATH_BYTES: usize = 4_096;
const MAX_SNAPSHOT_ROUTE_ATTEMPT_TIMEOUT_MS: usize = 15 * 60 * 1_000;
const MAX_SNAPSHOT_ROUTE_RECONNECT_MS: usize = 60_000;
const MAX_SHADOW_SOAK_SOURCES: usize = 256;
const MAX_SHADOW_SOAK_COMPARISONS: usize = 1_000_000;
const MAX_SHADOW_SOAK_ATTEMPTS: usize = 2_000_000;
const MAX_SHADOW_SOAK_TOKEN_BYTES: usize = 256 << 20;
const MAX_SHADOW_SOAK_TIMEOUT_MS: usize = 15 * 60 * 1_000;

#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    pub upstreams: Vec<Url>,
    pub upstream_token: Option<String>,
    pub max_tokens_strip: i64,
    pub advertise_ctx_margin: i64,
    pub route_alpha: f64,
    pub route_chunk_bytes: usize,
    pub route_max_prefix_bytes: usize,
    pub route_max_overlap_blocks: usize,
    pub route_index_capacity: usize,
    pub route_load_unit_bytes: usize,
    pub route_max_load_units: usize,
    pub affinity: Affinity,
    pub session_affinity_mode: SessionAffinityMode,
    pub session_affinity_key: Option<SecretString>,
    pub session_affinity_bonus_blocks: usize,
    pub session_affinity_max_load_delta: usize,
    pub route_journal: bool,
    pub tokenizer_mode: TokenizerMode,
    pub tokenizer_path: Option<String>,
    pub tokenizer_sha256: Option<String>,
    pub tokenizer_profile: TokenizerProfile,
    pub tokenizer_min_bytes: usize,
    pub tokenizer_max_bytes: usize,
    pub tokenizer_workers: usize,
    pub tokenizer_queue_capacity: usize,
    pub tokenizer_timeout_ms: usize,
    pub exact_route_mode: ExactRouteMode,
    pub exact_route_manifest_path: Option<String>,
    pub exact_route_manifest_sha256: Option<String>,
    pub exact_route_workers: usize,
    pub exact_route_timeout_ms: usize,
    pub exact_route_min_gain_tokens: usize,
    pub exact_route_max_load_delta: usize,
    pub exact_route_canary_bps: usize,
    pub exact_route_canary_key: Option<SecretString>,
    pub shadow_soak_mode: ShadowSoakMode,
    pub shadow_soak_source_target: usize,
    pub shadow_soak_comparison_target: usize,
    pub shadow_soak_attempt_limit: usize,
    pub shadow_soak_max_token_bytes: usize,
    pub shadow_soak_timeout_ms: usize,
    pub kv_event_mode: KvEventMode,
    pub kv_event_sources: Vec<KvEventSourceConfig>,
    pub kv_event_replay_limit: usize,
    pub kv_event_replay_tail_limit: usize,
    pub kv_event_timeout_ms: usize,
    pub kv_event_reconnect_min_ms: usize,
    pub kv_event_reconnect_max_ms: usize,
    pub snapshot_route_mode: SnapshotRouteMode,
    pub snapshot_route_sources: Vec<SnapshotRouteSourceConfig>,
    pub snapshot_route_secret_owner_uid: u32,
    pub snapshot_route_attestation_refresh_ms: usize,
    pub snapshot_route_attempt_timeout_ms: usize,
    pub snapshot_route_reconnect_min_ms: usize,
    pub snapshot_route_reconnect_max_ms: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Affinity {
    Prefix,
    Load,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionAffinityMode {
    Off,
    Shadow,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TokenizerMode {
    Off,
    RemoteShadow,
    LocalShadow,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TokenizerProfile {
    DeepSeekV4R34,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExactRouteMode {
    Off,
    Shadow,
    Placement,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ShadowSoakMode {
    Off,
    Capture,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KvEventMode {
    Off,
    Shadow,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SnapshotRouteMode {
    Off,
    Shadow,
}

#[derive(Clone, Eq, PartialEq)]
pub struct SnapshotRouteSourceConfig {
    pub socket_path: PathBuf,
    pub companion_uid: u32,
    pub session_secret_path: PathBuf,
    pub digest_secret_path: PathBuf,
    pub attestation_path: PathBuf,
    pub data_parallel_rank: u32,
    pub group_idx: u32,
}

impl fmt::Debug for SnapshotRouteSourceConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SnapshotRouteSourceConfig")
            .field("socket_path", &"<redacted>")
            .field("companion_uid", &"<redacted>")
            .field("session_secret_path", &"<redacted>")
            .field("digest_secret_path", &"<redacted>")
            .field("attestation_path", &"<redacted>")
            .field("data_parallel_rank", &self.data_parallel_rank)
            .field("group_idx", &self.group_idx)
            .finish()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct KvEventSourceConfig {
    pub live_endpoint: String,
    pub replay_endpoint: String,
    pub topic: String,
}

struct TokenizerSettings {
    mode: TokenizerMode,
    path: Option<String>,
    sha256: Option<String>,
    profile: TokenizerProfile,
    min_bytes: usize,
    max_bytes: usize,
}

struct KvEventSettings {
    mode: KvEventMode,
    sources: Vec<KvEventSourceConfig>,
    replay_limit: usize,
    replay_tail_limit: usize,
    timeout_ms: usize,
    reconnect_min_ms: usize,
    reconnect_max_ms: usize,
}

struct ExactRouteSettings {
    mode: ExactRouteMode,
    manifest_path: Option<String>,
    manifest_sha256: Option<String>,
    workers: usize,
    timeout_ms: usize,
    min_gain_tokens: usize,
    max_load_delta: usize,
    canary_bps: usize,
    canary_key: Option<SecretString>,
}

struct SessionAffinitySettings {
    mode: SessionAffinityMode,
    key: Option<SecretString>,
    bonus_blocks: usize,
    max_load_delta: usize,
}

struct SnapshotRouteSettings {
    mode: SnapshotRouteMode,
    sources: Vec<SnapshotRouteSourceConfig>,
    secret_owner_uid: u32,
    attestation_refresh_ms: usize,
    attempt_timeout_ms: usize,
    reconnect_min_ms: usize,
    reconnect_max_ms: usize,
}

struct ShadowSoakSettings {
    mode: ShadowSoakMode,
    source_target: usize,
    comparison_target: usize,
    attempt_limit: usize,
    max_token_bytes: usize,
    timeout_ms: usize,
}

#[derive(Clone, Eq, PartialEq)]
pub struct SecretString(String);

impl SecretString {
    pub(crate) fn as_bytes(&self) -> &[u8] {
        self.0.as_bytes()
    }

    #[cfg(test)]
    fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SecretString {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("<redacted>")
    }
}

#[derive(Debug, Error, PartialEq)]
pub enum ConfigError {
    #[error("DS4_UPSTREAM contains no upstreams")]
    NoUpstreams,
    #[error("invalid DS4_UPSTREAM entry {0:?}")]
    InvalidUpstream(String),
    #[error("invalid {key}={value:?}: {reason}")]
    InvalidValue {
        key: &'static str,
        value: String,
        reason: &'static str,
    },
}

impl Config {
    /// Loads and validates the public environment-variable contract.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] when an upstream URL or typed setting is invalid.
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_lookup(|key| env::var(key).ok())
    }

    /// Builds configuration from a lookup function, primarily for deterministic tests.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] when an upstream URL or typed setting is invalid.
    #[allow(clippy::too_many_lines)]
    pub fn from_lookup(mut get: impl FnMut(&str) -> Option<String>) -> Result<Self, ConfigError> {
        let raw_upstreams =
            get("DS4_UPSTREAM").unwrap_or_else(|| "http://ds4-flash:8000".to_owned());
        let upstreams = raw_upstreams
            .split(',')
            .filter_map(|raw| {
                let trimmed = raw.trim().trim_end_matches('/');
                (!trimmed.is_empty()).then_some(trimmed)
            })
            .map(|raw| {
                Url::parse(raw)
                    .ok()
                    .filter(Url::has_host)
                    .ok_or_else(|| ConfigError::InvalidUpstream(raw.to_owned()))
            })
            .collect::<Result<Vec<_>, _>>()?;
        if upstreams.is_empty() {
            return Err(ConfigError::NoUpstreams);
        }
        let upstream_token = get("DS4_UPSTREAM_TOKEN").filter(|value| !value.is_empty());

        let route_alpha = parse(
            &mut get,
            "DS4_ROUTE_ALPHA",
            4.0_f64,
            "a finite non-negative float",
        )?;
        if !route_alpha.is_finite() || route_alpha < 0.0 {
            return Err(invalid(
                "DS4_ROUTE_ALPHA",
                route_alpha.to_string(),
                "a finite non-negative float",
            ));
        }
        let route_chunk_bytes = positive(&mut get, "DS4_ROUTE_CHUNK_BYTES", 2_048)?;
        let route_max_prefix_bytes = positive(&mut get, "DS4_ROUTE_MAX_PREFIX_BYTES", 2 << 20)?;
        let route_max_overlap_blocks = positive(&mut get, "DS4_ROUTE_MAX_OVERLAP_BLOCKS", 32)?;
        let route_index_capacity = positive(&mut get, "DS4_ROUTE_INDEX_CAPACITY", 100_000)?;
        let route_load_unit_bytes = positive(&mut get, "DS4_ROUTE_LOAD_UNIT_BYTES", 32 << 10)?;
        let route_max_load_units = positive(&mut get, "DS4_ROUTE_MAX_LOAD_UNITS", 8)?;
        let affinity = match get("DS4_AFFINITY").as_deref().unwrap_or("prefix") {
            "prefix" => Affinity::Prefix,
            "load" => Affinity::Load,
            value => return Err(invalid("DS4_AFFINITY", value.to_owned(), "prefix or load")),
        };
        let session_affinity = session_affinity_settings(
            &mut get,
            upstreams.len(),
            route_max_overlap_blocks,
            route_max_load_units,
        )?;
        let tokenizer = tokenizer_settings(&mut get)?;
        let kv_events = kv_event_settings(&mut get, upstreams.len())?;
        let snapshot_route = snapshot_route_settings(&mut get, upstreams.len())?;
        let exact_route =
            exact_route_settings(&mut get, &tokenizer, &kv_events, &snapshot_route, affinity)?;
        if snapshot_route.mode == SnapshotRouteMode::Shadow
            && exact_route.mode != ExactRouteMode::Shadow
        {
            return Err(invalid(
                "DS4_SNAPSHOT_ROUTE_MODE",
                "shadow".to_owned(),
                "DS4_EXACT_ROUTE_MODE=shadow",
            ));
        }
        let shadow_soak = shadow_soak_settings(
            &mut get,
            &tokenizer,
            &exact_route,
            &snapshot_route,
            upstream_token.is_some(),
        )?;

        Ok(Self {
            upstreams,
            upstream_token,
            max_tokens_strip: parse(&mut get, "DS4_MAX_TOKENS_STRIP", 100_000, "an integer")?,
            advertise_ctx_margin: parse(
                &mut get,
                "DS4_ADVERTISE_CTX_MARGIN",
                16_384,
                "an integer",
            )?,
            route_alpha,
            route_chunk_bytes,
            route_max_prefix_bytes,
            route_max_overlap_blocks,
            route_index_capacity,
            route_load_unit_bytes,
            route_max_load_units,
            affinity,
            session_affinity_mode: session_affinity.mode,
            session_affinity_key: session_affinity.key,
            session_affinity_bonus_blocks: session_affinity.bonus_blocks,
            session_affinity_max_load_delta: session_affinity.max_load_delta,
            route_journal: parse(&mut get, "DS4_ROUTE_JOURNAL", false, "a boolean")?,
            tokenizer_mode: tokenizer.mode,
            tokenizer_path: tokenizer.path,
            tokenizer_sha256: tokenizer.sha256,
            tokenizer_profile: tokenizer.profile,
            tokenizer_min_bytes: tokenizer.min_bytes,
            tokenizer_max_bytes: tokenizer.max_bytes,
            tokenizer_workers: positive(&mut get, "DS4_TOKENIZER_WORKERS", 1)?,
            tokenizer_queue_capacity: positive(&mut get, "DS4_TOKENIZER_QUEUE_CAPACITY", 8)?,
            tokenizer_timeout_ms: positive(&mut get, "DS4_TOKENIZER_TIMEOUT_MS", 2_000)?,
            exact_route_mode: exact_route.mode,
            exact_route_manifest_path: exact_route.manifest_path,
            exact_route_manifest_sha256: exact_route.manifest_sha256,
            exact_route_workers: exact_route.workers,
            exact_route_timeout_ms: exact_route.timeout_ms,
            exact_route_min_gain_tokens: exact_route.min_gain_tokens,
            exact_route_max_load_delta: exact_route.max_load_delta,
            exact_route_canary_bps: exact_route.canary_bps,
            exact_route_canary_key: exact_route.canary_key,
            shadow_soak_mode: shadow_soak.mode,
            shadow_soak_source_target: shadow_soak.source_target,
            shadow_soak_comparison_target: shadow_soak.comparison_target,
            shadow_soak_attempt_limit: shadow_soak.attempt_limit,
            shadow_soak_max_token_bytes: shadow_soak.max_token_bytes,
            shadow_soak_timeout_ms: shadow_soak.timeout_ms,
            kv_event_mode: kv_events.mode,
            kv_event_sources: kv_events.sources,
            kv_event_replay_limit: kv_events.replay_limit,
            kv_event_replay_tail_limit: kv_events.replay_tail_limit,
            kv_event_timeout_ms: kv_events.timeout_ms,
            kv_event_reconnect_min_ms: kv_events.reconnect_min_ms,
            kv_event_reconnect_max_ms: kv_events.reconnect_max_ms,
            snapshot_route_mode: snapshot_route.mode,
            snapshot_route_sources: snapshot_route.sources,
            snapshot_route_secret_owner_uid: snapshot_route.secret_owner_uid,
            snapshot_route_attestation_refresh_ms: snapshot_route.attestation_refresh_ms,
            snapshot_route_attempt_timeout_ms: snapshot_route.attempt_timeout_ms,
            snapshot_route_reconnect_min_ms: snapshot_route.reconnect_min_ms,
            snapshot_route_reconnect_max_ms: snapshot_route.reconnect_max_ms,
        })
    }
}

fn session_affinity_settings(
    get: &mut impl FnMut(&str) -> Option<String>,
    upstream_count: usize,
    max_overlap_blocks: usize,
    max_load_units: usize,
) -> Result<SessionAffinitySettings, ConfigError> {
    let mode = match get("DS4_SESSION_AFFINITY_MODE").as_deref().unwrap_or("off") {
        "off" => SessionAffinityMode::Off,
        "shadow" => SessionAffinityMode::Shadow,
        value => {
            return Err(invalid(
                "DS4_SESSION_AFFINITY_MODE",
                value.to_owned(),
                "off or shadow",
            ));
        }
    };
    if mode == SessionAffinityMode::Shadow && upstream_count < 2 {
        return Err(invalid(
            "DS4_SESSION_AFFINITY_MODE",
            "shadow".to_owned(),
            "shadow requires at least two upstreams",
        ));
    }
    let key = get("DS4_SESSION_AFFINITY_KEY")
        .filter(|value| !value.is_empty())
        .map(SecretString);
    if mode == SessionAffinityMode::Shadow
        && key
            .as_ref()
            .is_none_or(|key| !(32..=256).contains(&key.as_bytes().len()))
    {
        return Err(invalid(
            "DS4_SESSION_AFFINITY_KEY",
            "<redacted>".to_owned(),
            "a secret from 32 through 256 bytes in shadow mode",
        ));
    }
    if mode == SessionAffinityMode::Off && key.is_some() {
        return Err(invalid(
            "DS4_SESSION_AFFINITY_KEY",
            "<redacted>".to_owned(),
            "unset unless DS4_SESSION_AFFINITY_MODE=shadow",
        ));
    }
    let bonus_blocks = positive(get, "DS4_SESSION_AFFINITY_BONUS_BLOCKS", 4)?;
    if bonus_blocks > max_overlap_blocks {
        return Err(invalid(
            "DS4_SESSION_AFFINITY_BONUS_BLOCKS",
            bonus_blocks.to_string(),
            "no greater than DS4_ROUTE_MAX_OVERLAP_BLOCKS",
        ));
    }
    let max_load_delta = parse(
        get,
        "DS4_SESSION_AFFINITY_MAX_LOAD_DELTA",
        0_usize,
        "a non-negative integer",
    )?;
    if max_load_delta > max_load_units {
        return Err(invalid(
            "DS4_SESSION_AFFINITY_MAX_LOAD_DELTA",
            max_load_delta.to_string(),
            "no greater than DS4_ROUTE_MAX_LOAD_UNITS",
        ));
    }
    Ok(SessionAffinitySettings {
        mode,
        key,
        bonus_blocks,
        max_load_delta,
    })
}

fn exact_route_settings(
    get: &mut impl FnMut(&str) -> Option<String>,
    tokenizer: &TokenizerSettings,
    kv_events: &KvEventSettings,
    snapshot_route: &SnapshotRouteSettings,
    affinity: Affinity,
) -> Result<ExactRouteSettings, ConfigError> {
    let mode = match get("DS4_EXACT_ROUTE_MODE").as_deref().unwrap_or("off") {
        "off" => ExactRouteMode::Off,
        "shadow" => ExactRouteMode::Shadow,
        "placement" => ExactRouteMode::Placement,
        value => {
            return Err(invalid(
                "DS4_EXACT_ROUTE_MODE",
                value.to_owned(),
                "off, shadow, or placement",
            ));
        }
    };
    let manifest_path = get("DS4_EXACT_ROUTE_MANIFEST_PATH").filter(|value| !value.is_empty());
    let manifest_sha256 = get("DS4_EXACT_ROUTE_MANIFEST_SHA256")
        .filter(|value| !value.is_empty())
        .map(|value| value.to_ascii_lowercase());
    if let Some(value) = &manifest_sha256
        && !valid_sha256(value)
    {
        return Err(invalid(
            "DS4_EXACT_ROUTE_MANIFEST_SHA256",
            value.clone(),
            "a 64-character hexadecimal SHA-256",
        ));
    }
    if mode != ExactRouteMode::Off {
        let mode_label = match mode {
            ExactRouteMode::Shadow => "shadow",
            ExactRouteMode::Placement => "placement",
            ExactRouteMode::Off => unreachable!("off mode is excluded"),
        };
        if tokenizer.mode != TokenizerMode::LocalShadow {
            return Err(invalid(
                "DS4_EXACT_ROUTE_MODE",
                mode_label.to_owned(),
                "exact routing requires DS4_TOKENIZER_MODE=local-shadow",
            ));
        }
        let direct = kv_events.mode == KvEventMode::Shadow;
        let snapshot = snapshot_route.mode == SnapshotRouteMode::Shadow;
        if direct == snapshot {
            return Err(invalid(
                "DS4_EXACT_ROUTE_MODE",
                mode_label.to_owned(),
                "exact routing requires exactly one of direct KV events or snapshot shadow",
            ));
        }
        if snapshot && mode != ExactRouteMode::Shadow {
            return Err(invalid(
                "DS4_EXACT_ROUTE_MODE",
                mode_label.to_owned(),
                "snapshot inventory is observation-only and requires shadow mode",
            ));
        }
        if manifest_path.is_none() {
            return Err(invalid(
                "DS4_EXACT_ROUTE_MANIFEST_PATH",
                String::new(),
                "a compatibility manifest path when exact routing is enabled",
            ));
        }
        if manifest_sha256.is_none() {
            return Err(invalid(
                "DS4_EXACT_ROUTE_MANIFEST_SHA256",
                String::new(),
                "the expected manifest SHA-256 when exact routing is enabled",
            ));
        }
        if mode == ExactRouteMode::Placement && affinity != Affinity::Prefix {
            return Err(invalid(
                "DS4_EXACT_ROUTE_MODE",
                "placement".to_owned(),
                "placement requires DS4_AFFINITY=prefix",
            ));
        }
    }
    let (canary_bps, canary_key) = exact_canary_settings(get, mode)?;
    Ok(ExactRouteSettings {
        mode,
        manifest_path,
        manifest_sha256,
        workers: positive(get, "DS4_EXACT_ROUTE_WORKERS", 4)?,
        timeout_ms: positive(get, "DS4_EXACT_ROUTE_TIMEOUT_MS", 250)?,
        min_gain_tokens: positive(get, "DS4_EXACT_ROUTE_MIN_GAIN_TOKENS", 8_192)?,
        max_load_delta: parse(
            get,
            "DS4_EXACT_ROUTE_MAX_LOAD_DELTA",
            0_usize,
            "a non-negative integer",
        )?,
        canary_bps,
        canary_key,
    })
}

fn shadow_soak_settings(
    get: &mut impl FnMut(&str) -> Option<String>,
    tokenizer: &TokenizerSettings,
    exact_route: &ExactRouteSettings,
    snapshot_route: &SnapshotRouteSettings,
    has_upstream_token: bool,
) -> Result<ShadowSoakSettings, ConfigError> {
    let mode = match get("DS4_SHADOW_SOAK_MODE").as_deref().unwrap_or("off") {
        "off" => ShadowSoakMode::Off,
        "capture" => ShadowSoakMode::Capture,
        value => {
            return Err(invalid(
                "DS4_SHADOW_SOAK_MODE",
                value.to_owned(),
                "off or capture",
            ));
        }
    };
    let source_target = bounded_positive(
        get,
        "DS4_SHADOW_SOAK_SOURCE_TARGET",
        104,
        MAX_SHADOW_SOAK_SOURCES,
    )?;
    let comparison_target = bounded_positive(
        get,
        "DS4_SHADOW_SOAK_COMPARISON_TARGET",
        100_000,
        MAX_SHADOW_SOAK_COMPARISONS,
    )?;
    let attempt_limit = bounded_positive(
        get,
        "DS4_SHADOW_SOAK_ATTEMPT_LIMIT",
        110_000,
        MAX_SHADOW_SOAK_ATTEMPTS,
    )?;
    let max_token_bytes = bounded_positive(
        get,
        "DS4_SHADOW_SOAK_MAX_TOKEN_BYTES",
        96 << 20,
        MAX_SHADOW_SOAK_TOKEN_BYTES,
    )?;
    let timeout_ms = bounded_positive(
        get,
        "DS4_SHADOW_SOAK_TIMEOUT_MS",
        300_000,
        MAX_SHADOW_SOAK_TIMEOUT_MS,
    )?;
    if attempt_limit < comparison_target {
        return Err(invalid(
            "DS4_SHADOW_SOAK_ATTEMPT_LIMIT",
            attempt_limit.to_string(),
            "at least DS4_SHADOW_SOAK_COMPARISON_TARGET",
        ));
    }
    if mode == ShadowSoakMode::Capture {
        if tokenizer.mode != TokenizerMode::LocalShadow
            || exact_route.mode != ExactRouteMode::Shadow
            || snapshot_route.mode != SnapshotRouteMode::Shadow
        {
            return Err(invalid(
                "DS4_SHADOW_SOAK_MODE",
                "capture".to_owned(),
                "capture requires local tokenization and snapshot exact shadow",
            ));
        }
        if !has_upstream_token {
            return Err(invalid(
                "DS4_SHADOW_SOAK_MODE",
                "capture".to_owned(),
                "capture requires DS4_UPSTREAM_TOKEN authentication",
            ));
        }
    }
    Ok(ShadowSoakSettings {
        mode,
        source_target,
        comparison_target,
        attempt_limit,
        max_token_bytes,
        timeout_ms,
    })
}

#[allow(clippy::too_many_lines)]
fn snapshot_route_settings(
    get: &mut impl FnMut(&str) -> Option<String>,
    upstream_count: usize,
) -> Result<SnapshotRouteSettings, ConfigError> {
    let mode = match get("DS4_SNAPSHOT_ROUTE_MODE").as_deref().unwrap_or("off") {
        "off" => SnapshotRouteMode::Off,
        "shadow" => SnapshotRouteMode::Shadow,
        value => {
            return Err(invalid(
                "DS4_SNAPSHOT_ROUTE_MODE",
                value.to_owned(),
                "off or shadow",
            ));
        }
    };
    if mode == SnapshotRouteMode::Off {
        return Ok(SnapshotRouteSettings {
            mode,
            sources: Vec::new(),
            secret_owner_uid: 0,
            attestation_refresh_ms: 1_000,
            attempt_timeout_ms: 30_000,
            reconnect_min_ms: 250,
            reconnect_max_ms: 5_000,
        });
    }
    let attestation_refresh_ms = bounded_positive(
        get,
        "DS4_SNAPSHOT_ROUTE_ATTESTATION_REFRESH_MS",
        1_000,
        MAX_SNAPSHOT_ROUTE_RECONNECT_MS,
    )?;
    let attempt_timeout_ms = bounded_positive(
        get,
        "DS4_SNAPSHOT_ROUTE_ATTEMPT_TIMEOUT_MS",
        30_000,
        MAX_SNAPSHOT_ROUTE_ATTEMPT_TIMEOUT_MS,
    )?;
    let reconnect_min_ms = bounded_positive(
        get,
        "DS4_SNAPSHOT_ROUTE_RECONNECT_MIN_MS",
        250,
        MAX_SNAPSHOT_ROUTE_RECONNECT_MS,
    )?;
    let reconnect_max_ms = bounded_positive(
        get,
        "DS4_SNAPSHOT_ROUTE_RECONNECT_MAX_MS",
        5_000,
        MAX_SNAPSHOT_ROUTE_RECONNECT_MS,
    )?;
    if reconnect_min_ms > reconnect_max_ms {
        return Err(invalid(
            "DS4_SNAPSHOT_ROUTE_RECONNECT_MIN_MS",
            reconnect_min_ms.to_string(),
            "no greater than DS4_SNAPSHOT_ROUTE_RECONNECT_MAX_MS",
        ));
    }
    let secret_owner_uid = parse(get, "DS4_SNAPSHOT_ROUTE_SECRET_OWNER_UID", 0_u32, "a UID")?;
    let sockets = value_list(get, "DS4_SNAPSHOT_ROUTE_SOCKET_PATHS")?;
    let companion_uids = parsed_list::<u32>(get, "DS4_SNAPSHOT_ROUTE_COMPANION_UIDS", "UIDs")?;
    let session_secrets = value_list(get, "DS4_SNAPSHOT_ROUTE_SESSION_SECRET_PATHS")?;
    let digest_secrets = value_list(get, "DS4_SNAPSHOT_ROUTE_DIGEST_SECRET_PATHS")?;
    let attestations = value_list(get, "DS4_SNAPSHOT_ROUTE_ATTESTATION_PATHS")?;
    let groups = value_list(get, "DS4_SNAPSHOT_ROUTE_GROUPS")?;
    let lengths = [
        sockets.len(),
        companion_uids.len(),
        session_secrets.len(),
        digest_secrets.len(),
        attestations.len(),
        groups.len(),
    ];
    if lengths.iter().any(|length| *length != upstream_count) {
        return Err(invalid(
            "DS4_SNAPSHOT_ROUTE_SOCKET_PATHS",
            format!("list lengths {lengths:?}, {upstream_count} upstreams"),
            "one socket, UID, session secret, digest secret, attestation, and group per upstream",
        ));
    }
    if companion_uids.contains(&0) {
        return Err(invalid(
            "DS4_SNAPSHOT_ROUTE_COMPANION_UIDS",
            "<redacted>".to_owned(),
            "non-root UIDs",
        ));
    }
    let mut sources = Vec::with_capacity(upstream_count);
    let mut unique_paths = HashSet::with_capacity(upstream_count.saturating_mul(4));
    for index in 0..upstream_count {
        let socket_path =
            normalized_absolute_path(&sockets[index], MAX_SNAPSHOT_ROUTE_SOCKET_PATH_BYTES)
                .ok_or_else(|| {
                    invalid(
                        "DS4_SNAPSHOT_ROUTE_SOCKET_PATHS",
                        "<redacted>".to_owned(),
                        "normalized absolute paths",
                    )
                })?;
        let session_secret_path =
            normalized_absolute_path(&session_secrets[index], MAX_SNAPSHOT_ROUTE_PATH_BYTES)
                .ok_or_else(|| {
                    invalid(
                        "DS4_SNAPSHOT_ROUTE_SESSION_SECRET_PATHS",
                        "<redacted>".to_owned(),
                        "normalized absolute paths",
                    )
                })?;
        let digest_secret_path =
            normalized_absolute_path(&digest_secrets[index], MAX_SNAPSHOT_ROUTE_PATH_BYTES)
                .ok_or_else(|| {
                    invalid(
                        "DS4_SNAPSHOT_ROUTE_DIGEST_SECRET_PATHS",
                        "<redacted>".to_owned(),
                        "normalized absolute paths",
                    )
                })?;
        let attestation_path =
            normalized_absolute_path(&attestations[index], MAX_SNAPSHOT_ROUTE_PATH_BYTES)
                .ok_or_else(|| {
                    invalid(
                        "DS4_SNAPSHOT_ROUTE_ATTESTATION_PATHS",
                        "<redacted>".to_owned(),
                        "normalized absolute paths",
                    )
                })?;
        if session_secret_path == digest_secret_path
            || session_secret_path == attestation_path
            || digest_secret_path == attestation_path
        {
            return Err(invalid(
                "DS4_SNAPSHOT_ROUTE_ATTESTATION_PATHS",
                "<redacted>".to_owned(),
                "three distinct protected paths per upstream",
            ));
        }
        if [
            &socket_path,
            &session_secret_path,
            &digest_secret_path,
            &attestation_path,
        ]
        .into_iter()
        .any(|path| !unique_paths.insert(path.clone()))
        {
            return Err(invalid(
                "DS4_SNAPSHOT_ROUTE_SOCKET_PATHS",
                "<redacted>".to_owned(),
                "distinct authority paths across upstreams",
            ));
        }
        let (data_parallel_rank, group_idx) = parse_group(&groups[index])?;
        sources.push(SnapshotRouteSourceConfig {
            socket_path,
            companion_uid: companion_uids[index],
            session_secret_path,
            digest_secret_path,
            attestation_path,
            data_parallel_rank,
            group_idx,
        });
    }
    Ok(SnapshotRouteSettings {
        mode,
        sources,
        secret_owner_uid,
        attestation_refresh_ms,
        attempt_timeout_ms,
        reconnect_min_ms,
        reconnect_max_ms,
    })
}

fn exact_canary_settings(
    get: &mut impl FnMut(&str) -> Option<String>,
    mode: ExactRouteMode,
) -> Result<(usize, Option<SecretString>), ConfigError> {
    let canary_bps = parse(
        get,
        "DS4_EXACT_ROUTE_CANARY_BPS",
        0_usize,
        "an integer from 0 through 10000",
    )?;
    if canary_bps > 10_000 {
        return Err(invalid(
            "DS4_EXACT_ROUTE_CANARY_BPS",
            canary_bps.to_string(),
            "an integer from 0 through 10000",
        ));
    }
    if mode != ExactRouteMode::Placement && canary_bps != 0 {
        return Err(invalid(
            "DS4_EXACT_ROUTE_CANARY_BPS",
            canary_bps.to_string(),
            "zero unless DS4_EXACT_ROUTE_MODE=placement",
        ));
    }
    let canary_key = get("DS4_EXACT_ROUTE_CANARY_KEY")
        .filter(|value| !value.is_empty())
        .map(SecretString);
    if mode == ExactRouteMode::Placement
        && canary_bps != 0
        && canary_key
            .as_ref()
            .is_none_or(|key| !(32..=256).contains(&key.as_bytes().len()))
    {
        return Err(invalid(
            "DS4_EXACT_ROUTE_CANARY_KEY",
            "<redacted>".to_owned(),
            "a secret from 32 through 256 bytes when placement canary is nonzero",
        ));
    }
    if mode != ExactRouteMode::Placement && canary_key.is_some() {
        return Err(invalid(
            "DS4_EXACT_ROUTE_CANARY_KEY",
            "<redacted>".to_owned(),
            "unset unless DS4_EXACT_ROUTE_MODE=placement",
        ));
    }
    Ok((canary_bps, canary_key))
}

fn kv_event_settings(
    get: &mut impl FnMut(&str) -> Option<String>,
    upstream_count: usize,
) -> Result<KvEventSettings, ConfigError> {
    let mode = match get("DS4_KV_EVENT_MODE").as_deref().unwrap_or("off") {
        "off" => KvEventMode::Off,
        "shadow" => KvEventMode::Shadow,
        value => {
            return Err(invalid(
                "DS4_KV_EVENT_MODE",
                value.to_owned(),
                "off or shadow",
            ));
        }
    };
    let sources = kv_event_sources(get, mode, upstream_count)?;
    let reconnect_min_ms = positive(get, "DS4_KV_EVENT_RECONNECT_MIN_MS", 250)?;
    let reconnect_max_ms = positive(get, "DS4_KV_EVENT_RECONNECT_MAX_MS", 10_000)?;
    if reconnect_min_ms > reconnect_max_ms {
        return Err(invalid(
            "DS4_KV_EVENT_RECONNECT_MIN_MS",
            reconnect_min_ms.to_string(),
            "no greater than DS4_KV_EVENT_RECONNECT_MAX_MS",
        ));
    }
    Ok(KvEventSettings {
        mode,
        sources,
        replay_limit: positive(get, "DS4_KV_EVENT_REPLAY_LIMIT", 1_024)?,
        replay_tail_limit: positive(get, "DS4_KV_EVENT_REPLAY_TAIL_LIMIT", 64)?,
        timeout_ms: positive(get, "DS4_KV_EVENT_TIMEOUT_MS", 5_000)?,
        reconnect_min_ms,
        reconnect_max_ms,
    })
}

fn kv_event_sources(
    get: &mut impl FnMut(&str) -> Option<String>,
    mode: KvEventMode,
    upstream_count: usize,
) -> Result<Vec<KvEventSourceConfig>, ConfigError> {
    if mode == KvEventMode::Off {
        return Ok(Vec::new());
    }
    let live = endpoint_list(get, "DS4_KV_EVENT_LIVE_ENDPOINTS")?;
    let replay = endpoint_list(get, "DS4_KV_EVENT_REPLAY_ENDPOINTS")?;
    if live.len() != upstream_count || replay.len() != upstream_count {
        return Err(invalid(
            "DS4_KV_EVENT_LIVE_ENDPOINTS",
            format!(
                "{} live, {} replay, {upstream_count} upstreams",
                live.len(),
                replay.len()
            ),
            "one live and replay endpoint per upstream",
        ));
    }
    let topic = get("DS4_KV_EVENT_TOPIC").unwrap_or_default();
    if topic.len() > 256 {
        return Err(invalid(
            "DS4_KV_EVENT_TOPIC",
            "<oversized>".to_owned(),
            "at most 256 bytes",
        ));
    }
    Ok(live
        .into_iter()
        .zip(replay)
        .map(|(live_endpoint, replay_endpoint)| KvEventSourceConfig {
            live_endpoint,
            replay_endpoint,
            topic: topic.clone(),
        })
        .collect())
}

fn endpoint_list(
    get: &mut impl FnMut(&str) -> Option<String>,
    key: &'static str,
) -> Result<Vec<String>, ConfigError> {
    get(key)
        .unwrap_or_default()
        .split(',')
        .filter_map(|value| {
            let value = value.trim();
            (!value.is_empty()).then_some(value)
        })
        .map(|value| {
            let parsed = Url::parse(value).ok();
            if parsed.as_ref().is_some_and(|url| {
                url.scheme() == "tcp"
                    && url.has_host()
                    && url.port().is_some()
                    && url.username().is_empty()
                    && url.password().is_none()
                    && matches!(url.path(), "" | "/")
                    && url.query().is_none()
                    && url.fragment().is_none()
            }) {
                Ok(value.to_owned())
            } else {
                Err(invalid(
                    key,
                    value.to_owned(),
                    "a comma-separated list of tcp://host:port endpoints",
                ))
            }
        })
        .collect()
}

fn value_list(
    get: &mut impl FnMut(&str) -> Option<String>,
    key: &'static str,
) -> Result<Vec<String>, ConfigError> {
    let raw = get(key).unwrap_or_default();
    let values = raw
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    if raw.split(',').any(|value| value.trim().is_empty()) && !raw.is_empty() {
        return Err(invalid(
            key,
            "<redacted>".to_owned(),
            "a dense comma-separated list",
        ));
    }
    Ok(values)
}

fn parsed_list<T: std::str::FromStr>(
    get: &mut impl FnMut(&str) -> Option<String>,
    key: &'static str,
    reason: &'static str,
) -> Result<Vec<T>, ConfigError> {
    value_list(get, key)?
        .into_iter()
        .map(|value| {
            value
                .parse()
                .map_err(|_| invalid(key, "<redacted>".to_owned(), reason))
        })
        .collect()
}

fn normalized_absolute_path(raw: &str, max_bytes: usize) -> Option<PathBuf> {
    let path = Path::new(raw);
    if raw.is_empty()
        || raw.len() > max_bytes
        || !path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::CurDir | Component::ParentDir | Component::Prefix(_)
            )
        })
    {
        return None;
    }
    Some(path.to_path_buf())
}

fn parse_group(raw: &str) -> Result<(u32, u32), ConfigError> {
    let Some((rank, index)) = raw.split_once(':') else {
        return Err(invalid(
            "DS4_SNAPSHOT_ROUTE_GROUPS",
            raw.to_owned(),
            "rank:index pairs",
        ));
    };
    if index.contains(':') {
        return Err(invalid(
            "DS4_SNAPSHOT_ROUTE_GROUPS",
            raw.to_owned(),
            "rank:index pairs",
        ));
    }
    let rank = rank.parse().map_err(|_| {
        invalid(
            "DS4_SNAPSHOT_ROUTE_GROUPS",
            raw.to_owned(),
            "rank:index pairs",
        )
    })?;
    let index = index.parse().map_err(|_| {
        invalid(
            "DS4_SNAPSHOT_ROUTE_GROUPS",
            raw.to_owned(),
            "rank:index pairs",
        )
    })?;
    Ok((rank, index))
}

fn tokenizer_settings(
    get: &mut impl FnMut(&str) -> Option<String>,
) -> Result<TokenizerSettings, ConfigError> {
    let mode = match get("DS4_TOKENIZER_MODE").as_deref().unwrap_or("off") {
        "off" => TokenizerMode::Off,
        "remote-shadow" => TokenizerMode::RemoteShadow,
        "local-shadow" => TokenizerMode::LocalShadow,
        value => {
            return Err(invalid(
                "DS4_TOKENIZER_MODE",
                value.to_owned(),
                "off, remote-shadow, or local-shadow",
            ));
        }
    };
    let path = get("DS4_TOKENIZER_PATH").filter(|value| !value.is_empty());
    let sha256 = get("DS4_TOKENIZER_SHA256")
        .filter(|value| !value.is_empty())
        .map(|value| value.to_ascii_lowercase());
    let profile = match get("DS4_TOKENIZER_PROFILE")
        .as_deref()
        .unwrap_or("deepseek-v4-r34")
    {
        "deepseek-v4-r34" => TokenizerProfile::DeepSeekV4R34,
        value => {
            return Err(invalid(
                "DS4_TOKENIZER_PROFILE",
                value.to_owned(),
                "deepseek-v4-r34",
            ));
        }
    };
    if mode == TokenizerMode::LocalShadow {
        if path.is_none() {
            return Err(invalid(
                "DS4_TOKENIZER_PATH",
                String::new(),
                "a tokenizer.json path in local-shadow mode",
            ));
        }
        if sha256.is_none() {
            return Err(invalid(
                "DS4_TOKENIZER_SHA256",
                String::new(),
                "the expected 64-character tokenizer SHA-256 in local-shadow mode",
            ));
        }
    }
    if let Some(value) = &sha256
        && !valid_sha256(value)
    {
        return Err(invalid(
            "DS4_TOKENIZER_SHA256",
            value.clone(),
            "a 64-character hexadecimal SHA-256",
        ));
    }
    let min_bytes = parse(
        get,
        "DS4_TOKENIZER_MIN_BYTES",
        32 << 10,
        "a non-negative integer",
    )?;
    let max_bytes = positive(get, "DS4_TOKENIZER_MAX_BYTES", 2 << 20)?;
    if min_bytes > max_bytes {
        return Err(invalid(
            "DS4_TOKENIZER_MIN_BYTES",
            min_bytes.to_string(),
            "no greater than DS4_TOKENIZER_MAX_BYTES",
        ));
    }
    Ok(TokenizerSettings {
        mode,
        path,
        sha256,
        profile,
        min_bytes,
        max_bytes,
    })
}

fn valid_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn parse<T: std::str::FromStr>(
    get: &mut impl FnMut(&str) -> Option<String>,
    key: &'static str,
    fallback: T,
    reason: &'static str,
) -> Result<T, ConfigError> {
    let Some(value) = get(key).filter(|value| !value.is_empty()) else {
        return Ok(fallback);
    };
    value.parse().map_err(|_| invalid(key, value, reason))
}

fn positive(
    get: &mut impl FnMut(&str) -> Option<String>,
    key: &'static str,
    fallback: usize,
) -> Result<usize, ConfigError> {
    let value = parse(get, key, fallback, "a positive integer")?;
    if value == 0 {
        return Err(invalid(key, value.to_string(), "a positive integer"));
    }
    Ok(value)
}

fn bounded_positive(
    get: &mut impl FnMut(&str) -> Option<String>,
    key: &'static str,
    fallback: usize,
    maximum: usize,
) -> Result<usize, ConfigError> {
    let value = positive(get, key, fallback)?;
    if value > maximum {
        return Err(invalid(
            key,
            value.to_string(),
            "a bounded positive integer",
        ));
    }
    Ok(value)
}

fn invalid(key: &'static str, value: String, reason: &'static str) -> ConfigError {
    ConfigError::InvalidValue { key, value, reason }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    #[test]
    fn defaults_match_go_contract() {
        let config = Config::from_lookup(|_| None).unwrap();
        assert_eq!(config.upstreams[0].as_str(), "http://ds4-flash:8000/");
        assert!((config.route_alpha - 4.0).abs() < f64::EPSILON);
        assert_eq!(config.route_chunk_bytes, 2_048);
        assert_eq!(config.route_max_prefix_bytes, 2 << 20);
        assert_eq!(config.route_max_overlap_blocks, 32);
        assert_eq!(config.route_load_unit_bytes, 32 << 10);
        assert_eq!(config.route_max_load_units, 8);
        assert_eq!(config.affinity, Affinity::Prefix);
        assert_eq!(config.session_affinity_mode, SessionAffinityMode::Off);
        assert!(config.session_affinity_key.is_none());
        assert_eq!(config.session_affinity_bonus_blocks, 4);
        assert_eq!(config.session_affinity_max_load_delta, 0);
        assert!(!config.route_journal);
        assert_eq!(config.tokenizer_mode, TokenizerMode::Off);
        assert!(config.tokenizer_path.is_none());
        assert!(config.tokenizer_sha256.is_none());
        assert_eq!(config.tokenizer_profile, TokenizerProfile::DeepSeekV4R34);
        assert_eq!(config.tokenizer_min_bytes, 32 << 10);
        assert_eq!(config.tokenizer_max_bytes, 2 << 20);
        assert_eq!(config.tokenizer_workers, 1);
        assert_eq!(config.tokenizer_queue_capacity, 8);
        assert_eq!(config.tokenizer_timeout_ms, 2_000);
        assert_eq!(config.exact_route_mode, ExactRouteMode::Off);
        assert!(config.exact_route_manifest_path.is_none());
        assert!(config.exact_route_manifest_sha256.is_none());
        assert_eq!(config.exact_route_workers, 4);
        assert_eq!(config.exact_route_timeout_ms, 250);
        assert_eq!(config.exact_route_min_gain_tokens, 8_192);
        assert_eq!(config.exact_route_max_load_delta, 0);
        assert_eq!(config.exact_route_canary_bps, 0);
        assert!(config.exact_route_canary_key.is_none());
        assert_eq!(config.shadow_soak_mode, ShadowSoakMode::Off);
        assert_eq!(config.shadow_soak_source_target, 104);
        assert_eq!(config.shadow_soak_comparison_target, 100_000);
        assert_eq!(config.shadow_soak_attempt_limit, 110_000);
        assert_eq!(config.shadow_soak_max_token_bytes, 96 << 20);
        assert_eq!(config.shadow_soak_timeout_ms, 300_000);
        assert_eq!(config.kv_event_mode, KvEventMode::Off);
        assert!(config.kv_event_sources.is_empty());
        assert_eq!(config.kv_event_replay_limit, 1_024);
        assert_eq!(config.kv_event_replay_tail_limit, 64);
        assert_eq!(config.kv_event_timeout_ms, 5_000);
        assert_eq!(config.kv_event_reconnect_min_ms, 250);
        assert_eq!(config.kv_event_reconnect_max_ms, 10_000);
        assert_eq!(config.snapshot_route_mode, SnapshotRouteMode::Off);
        assert!(config.snapshot_route_sources.is_empty());
        assert_eq!(config.snapshot_route_secret_owner_uid, 0);
        assert_eq!(config.snapshot_route_attestation_refresh_ms, 1_000);
        assert_eq!(config.snapshot_route_attempt_timeout_ms, 30_000);
        assert_eq!(config.snapshot_route_reconnect_min_ms, 250);
        assert_eq!(config.snapshot_route_reconnect_max_ms, 5_000);
    }

    #[test]
    fn session_affinity_is_separate_redacted_and_shadow_only() {
        let values = HashMap::from([
            ("DS4_UPSTREAM", "http://a:1,http://b:1"),
            ("DS4_SESSION_AFFINITY_MODE", "shadow"),
            (
                "DS4_SESSION_AFFINITY_KEY",
                "0123456789abcdef0123456789abcdef",
            ),
            ("DS4_SESSION_AFFINITY_BONUS_BLOCKS", "8"),
            ("DS4_SESSION_AFFINITY_MAX_LOAD_DELTA", "2"),
        ]);
        let config = Config::from_lookup(|key| values.get(key).map(ToString::to_string)).unwrap();
        assert_eq!(config.session_affinity_mode, SessionAffinityMode::Shadow);
        assert_eq!(config.session_affinity_bonus_blocks, 8);
        assert_eq!(config.session_affinity_max_load_delta, 2);
        assert!(!format!("{config:?}").contains("0123456789abcdef"));
    }

    #[test]
    fn session_affinity_rejects_unsafe_or_inapplicable_settings() {
        let mut values = HashMap::from([
            ("DS4_UPSTREAM", "http://a:1,http://b:1"),
            ("DS4_SESSION_AFFINITY_MODE", "shadow"),
            (
                "DS4_SESSION_AFFINITY_KEY",
                "0123456789abcdef0123456789abcdef",
            ),
            ("DS4_SESSION_AFFINITY_BONUS_BLOCKS", "8"),
            ("DS4_SESSION_AFFINITY_MAX_LOAD_DELTA", "2"),
        ]);
        values.insert("DS4_SESSION_AFFINITY_MODE", "off");
        assert!(matches!(
            Config::from_lookup(|key| values.get(key).map(ToString::to_string)),
            Err(ConfigError::InvalidValue {
                key: "DS4_SESSION_AFFINITY_KEY",
                value,
                ..
            }) if value == "<redacted>"
        ));
        values.insert("DS4_SESSION_AFFINITY_MODE", "shadow");
        values.insert("DS4_SESSION_AFFINITY_KEY", "short");
        assert!(matches!(
            Config::from_lookup(|key| values.get(key).map(ToString::to_string)),
            Err(ConfigError::InvalidValue {
                key: "DS4_SESSION_AFFINITY_KEY",
                value,
                ..
            }) if value == "<redacted>"
        ));
        values.insert(
            "DS4_SESSION_AFFINITY_KEY",
            "0123456789abcdef0123456789abcdef",
        );
        values.insert("DS4_SESSION_AFFINITY_BONUS_BLOCKS", "33");
        assert!(matches!(
            Config::from_lookup(|key| values.get(key).map(ToString::to_string)),
            Err(ConfigError::InvalidValue {
                key: "DS4_SESSION_AFFINITY_BONUS_BLOCKS",
                ..
            })
        ));
        values.insert("DS4_SESSION_AFFINITY_BONUS_BLOCKS", "8");
        values.insert("DS4_SESSION_AFFINITY_MAX_LOAD_DELTA", "9");
        assert!(matches!(
            Config::from_lookup(|key| values.get(key).map(ToString::to_string)),
            Err(ConfigError::InvalidValue {
                key: "DS4_SESSION_AFFINITY_MAX_LOAD_DELTA",
                ..
            })
        ));

        let single = HashMap::from([
            ("DS4_UPSTREAM", "http://a:1"),
            ("DS4_SESSION_AFFINITY_MODE", "shadow"),
            (
                "DS4_SESSION_AFFINITY_KEY",
                "0123456789abcdef0123456789abcdef",
            ),
        ]);
        assert!(matches!(
            Config::from_lookup(|key| single.get(key).map(ToString::to_string)),
            Err(ConfigError::InvalidValue {
                key: "DS4_SESSION_AFFINITY_MODE",
                ..
            })
        ));

        let wrong_mode = HashMap::from([
            ("DS4_UPSTREAM", "http://a:1,http://b:1"),
            ("DS4_SESSION_AFFINITY_MODE", "placement"),
        ]);
        assert!(matches!(
            Config::from_lookup(|key| wrong_mode.get(key).map(ToString::to_string)),
            Err(ConfigError::InvalidValue {
                key: "DS4_SESSION_AFFINITY_MODE",
                ..
            })
        ));

        let borrowed_canary_key = HashMap::from([
            ("DS4_UPSTREAM", "http://a:1,http://b:1"),
            ("DS4_SESSION_AFFINITY_MODE", "shadow"),
            ("DS4_EXACT_ROUTE_CANARY_KEY", "not-a-session-affinity-key"),
        ]);
        assert!(matches!(
            Config::from_lookup(|key| borrowed_canary_key.get(key).map(ToString::to_string)),
            Err(ConfigError::InvalidValue {
                key: "DS4_SESSION_AFFINITY_KEY",
                value,
                ..
            }) if value == "<redacted>"
        ));
    }

    #[test]
    fn session_affinity_key_boundaries_are_exact() {
        for (length, valid) in [(31, false), (32, true), (256, true), (257, false)] {
            let boundary = HashMap::from([
                (
                    "DS4_UPSTREAM".to_owned(),
                    "http://a:1,http://b:1".to_owned(),
                ),
                ("DS4_SESSION_AFFINITY_MODE".to_owned(), "shadow".to_owned()),
                ("DS4_SESSION_AFFINITY_KEY".to_owned(), "x".repeat(length)),
            ]);
            assert_eq!(
                Config::from_lookup(|key| boundary.get(key).cloned()).is_ok(),
                valid,
                "unexpected result for {length}-byte key"
            );
        }
    }

    #[test]
    fn validates_typed_environment() {
        let values = HashMap::from([
            ("DS4_UPSTREAM", "http://a:1/, http://b:2"),
            ("DS4_ROUTE_ALPHA", "nan"),
        ]);
        let error =
            Config::from_lookup(|key| values.get(key).map(ToString::to_string)).unwrap_err();
        assert!(matches!(
            error,
            ConfigError::InvalidValue {
                key: "DS4_ROUTE_ALPHA",
                ..
            }
        ));
    }

    #[test]
    fn snapshot_route_is_off_by_default_and_rejects_partial_cardinality() {
        let values = HashMap::from([
            ("DS4_UPSTREAM", "http://a:1,http://b:2"),
            ("DS4_SNAPSHOT_ROUTE_MODE", "shadow"),
            ("DS4_SNAPSHOT_ROUTE_SOCKET_PATHS", "/run/a.sock,/run/b.sock"),
            ("DS4_SNAPSHOT_ROUTE_COMPANION_UIDS", "12001"),
            (
                "DS4_SNAPSHOT_ROUTE_SESSION_SECRET_PATHS",
                "/run/a-session,/run/b-session",
            ),
            (
                "DS4_SNAPSHOT_ROUTE_DIGEST_SECRET_PATHS",
                "/run/a-digest,/run/b-digest",
            ),
            (
                "DS4_SNAPSHOT_ROUTE_ATTESTATION_PATHS",
                "/run/a-attest,/run/b-attest",
            ),
            ("DS4_SNAPSHOT_ROUTE_GROUPS", "0:0,0:0"),
        ]);
        let error =
            Config::from_lookup(|key| values.get(key).map(ToString::to_string)).unwrap_err();
        assert!(matches!(
            error,
            ConfigError::InvalidValue {
                key: "DS4_SNAPSHOT_ROUTE_SOCKET_PATHS",
                ..
            }
        ));
    }

    #[test]
    fn snapshot_route_requires_shadow_and_distinct_protected_paths() {
        let mut values = HashMap::from([
            ("DS4_UPSTREAM", "http://a:1"),
            ("DS4_SNAPSHOT_ROUTE_MODE", "shadow"),
            ("DS4_SNAPSHOT_ROUTE_SOCKET_PATHS", "/run/a.sock"),
            ("DS4_SNAPSHOT_ROUTE_COMPANION_UIDS", "12001"),
            ("DS4_SNAPSHOT_ROUTE_SESSION_SECRET_PATHS", "/run/session"),
            ("DS4_SNAPSHOT_ROUTE_DIGEST_SECRET_PATHS", "/run/digest"),
            ("DS4_SNAPSHOT_ROUTE_ATTESTATION_PATHS", "/run/attest"),
            ("DS4_SNAPSHOT_ROUTE_GROUPS", "0:0"),
        ]);
        let error =
            Config::from_lookup(|key| values.get(key).map(ToString::to_string)).unwrap_err();
        assert!(matches!(
            error,
            ConfigError::InvalidValue {
                key: "DS4_SNAPSHOT_ROUTE_MODE",
                ..
            }
        ));

        values.insert("DS4_SNAPSHOT_ROUTE_DIGEST_SECRET_PATHS", "/run/session");
        let error =
            Config::from_lookup(|key| values.get(key).map(ToString::to_string)).unwrap_err();
        assert!(matches!(
            error,
            ConfigError::InvalidValue {
                key: "DS4_SNAPSHOT_ROUTE_ATTESTATION_PATHS",
                ..
            }
        ));
    }

    #[test]
    fn parses_snapshot_shadow_as_the_exclusive_observation_inventory() {
        let mut values = HashMap::from([
            ("DS4_UPSTREAM", "http://a:1,http://b:2"),
            ("DS4_TOKENIZER_MODE", "local-shadow"),
            ("DS4_TOKENIZER_PATH", "/models/tokenizer.json"),
            (
                "DS4_TOKENIZER_SHA256",
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            ),
            ("DS4_EXACT_ROUTE_MODE", "shadow"),
            ("DS4_EXACT_ROUTE_MANIFEST_PATH", "/compat/manifest.json"),
            (
                "DS4_EXACT_ROUTE_MANIFEST_SHA256",
                "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            ),
            ("DS4_SNAPSHOT_ROUTE_MODE", "shadow"),
            ("DS4_SNAPSHOT_ROUTE_SOCKET_PATHS", "/run/a.sock,/run/b.sock"),
            ("DS4_SNAPSHOT_ROUTE_COMPANION_UIDS", "12001,12002"),
            (
                "DS4_SNAPSHOT_ROUTE_SESSION_SECRET_PATHS",
                "/run/a-session,/run/b-session",
            ),
            (
                "DS4_SNAPSHOT_ROUTE_DIGEST_SECRET_PATHS",
                "/run/a-digest,/run/b-digest",
            ),
            (
                "DS4_SNAPSHOT_ROUTE_ATTESTATION_PATHS",
                "/run/a-attest,/run/b-attest",
            ),
            ("DS4_SNAPSHOT_ROUTE_GROUPS", "0:0,1:2"),
            ("DS4_SNAPSHOT_ROUTE_SECRET_OWNER_UID", "12000"),
            ("DS4_SNAPSHOT_ROUTE_ATTESTATION_REFRESH_MS", "250"),
        ]);
        let config = Config::from_lookup(|key| values.get(key).map(ToString::to_string)).unwrap();
        assert_eq!(config.snapshot_route_sources.len(), 2);
        assert_eq!(config.snapshot_route_sources[1].companion_uid, 12002);
        assert_eq!(config.snapshot_route_sources[1].data_parallel_rank, 1);
        assert_eq!(config.snapshot_route_sources[1].group_idx, 2);
        assert_eq!(config.snapshot_route_secret_owner_uid, 12000);
        assert_eq!(config.snapshot_route_attestation_refresh_ms, 250);
        assert!(config.kv_event_sources.is_empty());
        let debug = format!("{:?}", config.snapshot_route_sources);
        for protected in ["a.sock", "a-session", "a-digest", "a-attest", "12001"] {
            assert!(!debug.contains(protected));
        }

        values.insert("DS4_SHADOW_SOAK_MODE", "capture");
        assert!(matches!(
            Config::from_lookup(|key| values.get(key).map(ToString::to_string)),
            Err(ConfigError::InvalidValue {
                key: "DS4_SHADOW_SOAK_MODE",
                ..
            })
        ));
        values.insert("DS4_UPSTREAM_TOKEN", "private-token");
        values.insert("DS4_SHADOW_SOAK_SOURCE_TARGET", "104");
        values.insert("DS4_SHADOW_SOAK_COMPARISON_TARGET", "100000");
        values.insert("DS4_SHADOW_SOAK_ATTEMPT_LIMIT", "100001");
        values.insert("DS4_SHADOW_SOAK_MAX_TOKEN_BYTES", "100663296");
        let soak = Config::from_lookup(|key| values.get(key).map(ToString::to_string)).unwrap();
        assert_eq!(soak.shadow_soak_mode, ShadowSoakMode::Capture);
        assert_eq!(soak.shadow_soak_source_target, 104);
        assert_eq!(soak.shadow_soak_comparison_target, 100_000);
        assert_eq!(soak.shadow_soak_attempt_limit, 100_001);
        assert_eq!(soak.shadow_soak_max_token_bytes, 96 << 20);
        assert_eq!(soak.shadow_soak_timeout_ms, 300_000);
        values.insert("DS4_SHADOW_SOAK_ATTEMPT_LIMIT", "99999");
        assert!(matches!(
            Config::from_lookup(|key| values.get(key).map(ToString::to_string)),
            Err(ConfigError::InvalidValue {
                key: "DS4_SHADOW_SOAK_ATTEMPT_LIMIT",
                ..
            })
        ));
        values.insert("DS4_SHADOW_SOAK_ATTEMPT_LIMIT", "100001");

        values.insert("DS4_KV_EVENT_MODE", "shadow");
        values.insert("DS4_KV_EVENT_LIVE_ENDPOINTS", "tcp://a:1,tcp://b:1");
        values.insert("DS4_KV_EVENT_REPLAY_ENDPOINTS", "tcp://a:2,tcp://b:2");
        assert!(matches!(
            Config::from_lookup(|key| values.get(key).map(ToString::to_string)),
            Err(ConfigError::InvalidValue {
                key: "DS4_EXACT_ROUTE_MODE",
                ..
            })
        ));
    }

    #[test]
    fn validates_tokenizer_bounds_and_mode() {
        let values = HashMap::from([
            ("DS4_TOKENIZER_MODE", "remote-shadow"),
            ("DS4_TOKENIZER_MIN_BYTES", "4096"),
            ("DS4_TOKENIZER_MAX_BYTES", "1024"),
        ]);
        let error =
            Config::from_lookup(|key| values.get(key).map(ToString::to_string)).unwrap_err();
        assert!(matches!(
            error,
            ConfigError::InvalidValue {
                key: "DS4_TOKENIZER_MIN_BYTES",
                ..
            }
        ));
    }

    #[test]
    fn local_tokenizer_requires_an_artifact_path() {
        let values = HashMap::from([("DS4_TOKENIZER_MODE", "local-shadow")]);
        let error =
            Config::from_lookup(|key| values.get(key).map(ToString::to_string)).unwrap_err();
        assert!(matches!(
            error,
            ConfigError::InvalidValue {
                key: "DS4_TOKENIZER_PATH",
                ..
            }
        ));
    }

    #[test]
    fn local_tokenizer_requires_a_valid_artifact_digest() {
        let values = HashMap::from([
            ("DS4_TOKENIZER_MODE", "local-shadow"),
            ("DS4_TOKENIZER_PATH", "/models/tokenizer.json"),
            ("DS4_TOKENIZER_SHA256", "not-a-digest"),
        ]);
        let error =
            Config::from_lookup(|key| values.get(key).map(ToString::to_string)).unwrap_err();
        assert!(matches!(
            error,
            ConfigError::InvalidValue {
                key: "DS4_TOKENIZER_SHA256",
                ..
            }
        ));
    }

    #[test]
    fn exact_route_shadow_requires_local_tokens_events_and_manifest() {
        let base = HashMap::from([
            ("DS4_UPSTREAM", "http://a:1"),
            ("DS4_TOKENIZER_MODE", "local-shadow"),
            ("DS4_TOKENIZER_PATH", "/models/tokenizer.json"),
            (
                "DS4_TOKENIZER_SHA256",
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            ),
            ("DS4_EXACT_ROUTE_MODE", "shadow"),
        ]);
        let error = Config::from_lookup(|key| base.get(key).map(ToString::to_string)).unwrap_err();
        assert!(matches!(
            error,
            ConfigError::InvalidValue {
                key: "DS4_EXACT_ROUTE_MODE",
                ..
            }
        ));

        let mut values = base;
        values.insert("DS4_KV_EVENT_MODE", "shadow");
        values.insert("DS4_KV_EVENT_LIVE_ENDPOINTS", "tcp://a:5557");
        values.insert("DS4_KV_EVENT_REPLAY_ENDPOINTS", "tcp://a:5558");
        let error =
            Config::from_lookup(|key| values.get(key).map(ToString::to_string)).unwrap_err();
        assert!(matches!(
            error,
            ConfigError::InvalidValue {
                key: "DS4_EXACT_ROUTE_MANIFEST_PATH",
                ..
            }
        ));

        values.insert("DS4_EXACT_ROUTE_MANIFEST_PATH", "/compat/manifest.json");
        values.insert(
            "DS4_EXACT_ROUTE_MANIFEST_SHA256",
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        );
        assert_eq!(
            Config::from_lookup(|key| values.get(key).map(ToString::to_string))
                .unwrap()
                .exact_route_mode,
            ExactRouteMode::Shadow
        );
    }

    #[test]
    fn exact_route_placement_is_explicit_and_parses_conservative_gates() {
        let values = HashMap::from([
            ("DS4_UPSTREAM", "http://a:1,http://b:1"),
            ("DS4_TOKENIZER_MODE", "local-shadow"),
            ("DS4_TOKENIZER_PATH", "/models/tokenizer.json"),
            (
                "DS4_TOKENIZER_SHA256",
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            ),
            ("DS4_KV_EVENT_MODE", "shadow"),
            ("DS4_KV_EVENT_LIVE_ENDPOINTS", "tcp://a:5557,tcp://b:5557"),
            ("DS4_KV_EVENT_REPLAY_ENDPOINTS", "tcp://a:5558,tcp://b:5558"),
            ("DS4_EXACT_ROUTE_MODE", "placement"),
            ("DS4_EXACT_ROUTE_MANIFEST_PATH", "/compat/manifest.json"),
            (
                "DS4_EXACT_ROUTE_MANIFEST_SHA256",
                "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            ),
            ("DS4_EXACT_ROUTE_MIN_GAIN_TOKENS", "16384"),
            ("DS4_EXACT_ROUTE_MAX_LOAD_DELTA", "1"),
            ("DS4_EXACT_ROUTE_CANARY_BPS", "250"),
            (
                "DS4_EXACT_ROUTE_CANARY_KEY",
                "0123456789abcdef0123456789abcdef",
            ),
            ("DS4_SESSION_AFFINITY_MODE", "shadow"),
            (
                "DS4_SESSION_AFFINITY_KEY",
                "fedcba9876543210fedcba9876543210",
            ),
        ]);
        let config = Config::from_lookup(|key| values.get(key).map(ToString::to_string)).unwrap();
        assert_eq!(config.exact_route_mode, ExactRouteMode::Placement);
        assert_eq!(config.exact_route_min_gain_tokens, 16_384);
        assert_eq!(config.exact_route_max_load_delta, 1);
        assert_eq!(config.exact_route_canary_bps, 250);
        assert_eq!(config.session_affinity_mode, SessionAffinityMode::Shadow);
        assert_eq!(
            config
                .exact_route_canary_key
                .as_ref()
                .map(SecretString::expose),
            Some("0123456789abcdef0123456789abcdef")
        );
        assert_eq!(
            config
                .session_affinity_key
                .as_ref()
                .map(SecretString::expose),
            Some("fedcba9876543210fedcba9876543210")
        );
        let debug = format!("{config:?}");
        assert!(!debug.contains("0123456789abcdef"));
        assert!(!debug.contains("fedcba9876543210"));
        assert!(debug.contains("<redacted>"));
    }

    #[test]
    fn exact_route_placement_requires_a_bounded_explicit_canary() {
        let mut values = HashMap::from([
            ("DS4_UPSTREAM", "http://a:1"),
            ("DS4_TOKENIZER_MODE", "local-shadow"),
            ("DS4_TOKENIZER_PATH", "/models/tokenizer.json"),
            (
                "DS4_TOKENIZER_SHA256",
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            ),
            ("DS4_KV_EVENT_MODE", "shadow"),
            ("DS4_KV_EVENT_LIVE_ENDPOINTS", "tcp://a:5557"),
            ("DS4_KV_EVENT_REPLAY_ENDPOINTS", "tcp://a:5558"),
            ("DS4_EXACT_ROUTE_MODE", "placement"),
            ("DS4_EXACT_ROUTE_MANIFEST_PATH", "/compat/manifest.json"),
            (
                "DS4_EXACT_ROUTE_MANIFEST_SHA256",
                "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            ),
        ]);
        let disabled = Config::from_lookup(|key| values.get(key).map(ToString::to_string)).unwrap();
        assert_eq!(disabled.exact_route_canary_bps, 0);
        assert!(disabled.exact_route_canary_key.is_none());
        values.insert("DS4_EXACT_ROUTE_CANARY_BPS", "10001");
        assert!(matches!(
            Config::from_lookup(|key| values.get(key).map(ToString::to_string)),
            Err(ConfigError::InvalidValue {
                key: "DS4_EXACT_ROUTE_CANARY_BPS",
                ..
            })
        ));
        values.insert("DS4_EXACT_ROUTE_CANARY_BPS", "100");
        assert!(matches!(
            Config::from_lookup(|key| values.get(key).map(ToString::to_string)),
            Err(ConfigError::InvalidValue {
                key: "DS4_EXACT_ROUTE_CANARY_KEY",
                value,
                ..
            }) if value == "<redacted>"
        ));
        values.insert("DS4_EXACT_ROUTE_CANARY_KEY", "too-short");
        assert!(matches!(
            Config::from_lookup(|key| values.get(key).map(ToString::to_string)),
            Err(ConfigError::InvalidValue {
                key: "DS4_EXACT_ROUTE_CANARY_KEY",
                value,
                ..
            }) if value == "<redacted>"
        ));
        values.insert("DS4_EXACT_ROUTE_CANARY_KEY", "x".repeat(32).leak());
        assert!(Config::from_lookup(|key| values.get(key).map(ToString::to_string)).is_ok());
        values.insert("DS4_EXACT_ROUTE_CANARY_KEY", "x".repeat(256).leak());
        assert!(Config::from_lookup(|key| values.get(key).map(ToString::to_string)).is_ok());
        values.insert("DS4_EXACT_ROUTE_CANARY_KEY", "x".repeat(257).leak());
        assert!(matches!(
            Config::from_lookup(|key| values.get(key).map(ToString::to_string)),
            Err(ConfigError::InvalidValue {
                key: "DS4_EXACT_ROUTE_CANARY_KEY",
                ..
            })
        ));
    }

    #[test]
    fn shadow_kv_events_require_one_endpoint_pair_per_upstream() {
        let values = HashMap::from([
            ("DS4_UPSTREAM", "http://a:1,http://b:2"),
            ("DS4_KV_EVENT_MODE", "shadow"),
            ("DS4_KV_EVENT_LIVE_ENDPOINTS", "tcp://a:5557"),
            ("DS4_KV_EVENT_REPLAY_ENDPOINTS", "tcp://a:5558,tcp://b:5558"),
        ]);
        let error =
            Config::from_lookup(|key| values.get(key).map(ToString::to_string)).unwrap_err();
        assert!(matches!(
            error,
            ConfigError::InvalidValue {
                key: "DS4_KV_EVENT_LIVE_ENDPOINTS",
                ..
            }
        ));
    }

    #[test]
    fn parses_typed_kv_event_shadow_sources() {
        let values = HashMap::from([
            ("DS4_UPSTREAM", "http://a:1,http://b:2"),
            ("DS4_KV_EVENT_MODE", "shadow"),
            ("DS4_KV_EVENT_LIVE_ENDPOINTS", "tcp://a:5557, tcp://b:5557"),
            ("DS4_KV_EVENT_REPLAY_ENDPOINTS", "tcp://a:5558,tcp://b:5558"),
            ("DS4_KV_EVENT_TOPIC", "kv"),
            ("DS4_KV_EVENT_RECONNECT_MIN_MS", "100"),
            ("DS4_KV_EVENT_RECONNECT_MAX_MS", "200"),
        ]);
        let config = Config::from_lookup(|key| values.get(key).map(ToString::to_string)).unwrap();
        assert_eq!(config.kv_event_mode, KvEventMode::Shadow);
        assert_eq!(config.kv_event_sources.len(), 2);
        assert_eq!(config.kv_event_sources[1].live_endpoint, "tcp://b:5557");
        assert_eq!(config.kv_event_sources[1].replay_endpoint, "tcp://b:5558");
        assert_eq!(config.kv_event_sources[1].topic, "kv");
    }

    #[test]
    fn rejects_non_tcp_kv_event_endpoints_and_inverted_backoff() {
        let bad_endpoint = HashMap::from([
            ("DS4_KV_EVENT_MODE", "shadow"),
            ("DS4_KV_EVENT_LIVE_ENDPOINTS", "ipc:///tmp/events"),
            ("DS4_KV_EVENT_REPLAY_ENDPOINTS", "tcp://a:5558"),
        ]);
        assert!(Config::from_lookup(|key| bad_endpoint.get(key).map(ToString::to_string)).is_err());

        let bad_backoff = HashMap::from([
            ("DS4_KV_EVENT_RECONNECT_MIN_MS", "200"),
            ("DS4_KV_EVENT_RECONNECT_MAX_MS", "100"),
        ]);
        let error =
            Config::from_lookup(|key| bad_backoff.get(key).map(ToString::to_string)).unwrap_err();
        assert!(matches!(
            error,
            ConfigError::InvalidValue {
                key: "DS4_KV_EVENT_RECONNECT_MIN_MS",
                ..
            }
        ));
    }
}
