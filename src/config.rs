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

struct TokenizerSettings {
    mode: TokenizerMode,
    path: Option<String>,
    sha256: Option<String>,
    profile: TokenizerProfile,
    min_bytes: usize,
    max_bytes: usize,
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
        })
    }
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
        && (value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()))
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
}
