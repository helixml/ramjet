use std::env;

use thiserror::Error;
use url::Url;

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
    pub kv_event_mode: KvEventMode,
    pub kv_event_sources: Vec<KvEventSourceConfig>,
    pub kv_event_replay_limit: usize,
    pub kv_event_replay_tail_limit: usize,
    pub kv_event_timeout_ms: usize,
    pub kv_event_reconnect_min_ms: usize,
    pub kv_event_reconnect_max_ms: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Affinity {
    Prefix,
    Load,
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
pub enum KvEventMode {
    Off,
    Shadow,
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
        let tokenizer = tokenizer_settings(&mut get)?;
        let kv_events = kv_event_settings(&mut get, upstreams.len())?;
        let exact_route = exact_route_settings(&mut get, &tokenizer, &kv_events, affinity)?;

        Ok(Self {
            upstreams,
            upstream_token: get("DS4_UPSTREAM_TOKEN").filter(|value| !value.is_empty()),
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
            kv_event_mode: kv_events.mode,
            kv_event_sources: kv_events.sources,
            kv_event_replay_limit: kv_events.replay_limit,
            kv_event_replay_tail_limit: kv_events.replay_tail_limit,
            kv_event_timeout_ms: kv_events.timeout_ms,
            kv_event_reconnect_min_ms: kv_events.reconnect_min_ms,
            kv_event_reconnect_max_ms: kv_events.reconnect_max_ms,
        })
    }
}

fn exact_route_settings(
    get: &mut impl FnMut(&str) -> Option<String>,
    tokenizer: &TokenizerSettings,
    kv_events: &KvEventSettings,
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
        if kv_events.mode != KvEventMode::Shadow {
            return Err(invalid(
                "DS4_EXACT_ROUTE_MODE",
                mode_label.to_owned(),
                "exact routing requires DS4_KV_EVENT_MODE=shadow",
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
    })
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
        assert_eq!(config.kv_event_mode, KvEventMode::Off);
        assert!(config.kv_event_sources.is_empty());
        assert_eq!(config.kv_event_replay_limit, 1_024);
        assert_eq!(config.kv_event_replay_tail_limit, 64);
        assert_eq!(config.kv_event_timeout_ms, 5_000);
        assert_eq!(config.kv_event_reconnect_min_ms, 250);
        assert_eq!(config.kv_event_reconnect_max_ms, 10_000);
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
            ("DS4_EXACT_ROUTE_MIN_GAIN_TOKENS", "16384"),
            ("DS4_EXACT_ROUTE_MAX_LOAD_DELTA", "1"),
        ]);
        let config = Config::from_lookup(|key| values.get(key).map(ToString::to_string)).unwrap();
        assert_eq!(config.exact_route_mode, ExactRouteMode::Placement);
        assert_eq!(config.exact_route_min_gain_tokens, 16_384);
        assert_eq!(config.exact_route_max_load_delta, 1);
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
