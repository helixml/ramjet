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
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Affinity {
    Prefix,
    Load,
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
        })
    }
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
}
