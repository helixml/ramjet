//! Machine view: a self-contained dashboard for the whole serving box.
//!
//! The module samples three local sources on a fixed interval — the proxy's
//! own Prometheus registry, each upstream engine's `/metrics` endpoint, and an
//! optional loopback host agent (`bench/machineview_agent.py`) for CPU / GPU /
//! disk / energy telemetry — and keeps the result in a bounded in-memory ring
//! with optional JSON snapshot persistence. A small JSON API plus a static
//! bundle served under `/ui` on the metrics listener make the box observable
//! without Prometheus or Grafana.
//!
//! Everything here is observation-only: sampling never influences routing,
//! upstream health, or request handling, and the store is bounded by
//! retention, so an idle dashboard costs a few megabytes and one scrape per
//! interval.

#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]

use std::{
    collections::HashMap,
    collections::VecDeque,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use axum::{
    Router,
    body::Body,
    extract::{
        Query, State, WebSocketUpgrade,
        ws::{Message, WebSocket},
    },
    http::{Response, StatusCode},
    response::IntoResponse,
    routing::get,
};
use parking_lot::Mutex;
use prometheus::Registry;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use url::Url;

const MIN_INTERVAL_MS: u64 = 1_000;
const MAX_INTERVAL_MS: u64 = 60_000;
const MIN_RETENTION_SECONDS: u64 = 60;
const MAX_RETENTION_SECONDS: u64 = 7 * 86_400;
const DEFAULT_SERIES_POINTS: usize = 400;
const MAX_SERIES_POINTS: usize = 2_000;
const PERSIST_EVERY_TICKS: u64 = 60;
const HISTOGRAM_WINDOW_MS: u64 = 120_000;
const HOUR_MS: u64 = 3_600_000;
const MIN_STREAM_INTERVAL_MS: u64 = 200;
const MAX_STREAM_INTERVAL_MS: u64 = 10_000;
/// Frames a slow client may fall behind before it is told it lost data.
const STREAM_CHANNEL_CAPACITY: usize = 64;
/// A dashboard is a handful of tabs, not a fan-out surface; keep the work
/// this observation-only path can be asked to do bounded.
const MAX_STREAM_CLIENTS: usize = 8;
const MIN_TOKEN_HISTORY_DAYS: u64 = 1;
const MAX_TOKEN_HISTORY_DAYS: u64 = 400;
/// Version 1 files carry samples only; version 2 adds the token history and
/// is still readable by, and readable from, a version 1 snapshot.
const STATE_VERSION: u32 = 2;
const MIN_STATE_VERSION: u32 = 1;
/// Static assets must be small dashboard files; refuse to stream anything
/// that plainly is not part of the built bundle.
const MAX_STATIC_FILE_BYTES: u64 = 16 << 20;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Mode {
    Off,
    On,
}

#[derive(Clone, Debug)]
pub struct Settings {
    pub mode: Mode,
    pub interval_ms: u64,
    pub retention_seconds: u64,
    pub token_history_days: u64,
    pub stream_interval_ms: u64,
    pub agent_url: Option<Url>,
    pub state_path: Option<PathBuf>,
    pub ui_dir: Option<PathBuf>,
}

#[derive(Debug, thiserror::Error)]
pub enum SettingsError {
    #[error("invalid {key}: {value:?} (expected {expected})")]
    Invalid {
        key: &'static str,
        value: String,
        expected: &'static str,
    },
}

fn invalid(key: &'static str, value: String, expected: &'static str) -> SettingsError {
    SettingsError::Invalid {
        key,
        value,
        expected,
    }
}

impl Settings {
    /// Reads machine-view settings from process environment variables.
    ///
    /// # Errors
    ///
    /// Returns [`SettingsError`] when a value fails typed validation.
    pub fn from_env() -> Result<Self, SettingsError> {
        Self::from_lookup(|key| std::env::var(key).ok())
    }

    /// Builds settings from a lookup function, primarily for deterministic tests.
    ///
    /// # Errors
    ///
    /// Returns [`SettingsError`] when a value fails typed validation.
    pub fn from_lookup(mut get: impl FnMut(&str) -> Option<String>) -> Result<Self, SettingsError> {
        let mode = match get("RJ_MACHINEVIEW_MODE").as_deref().unwrap_or("on") {
            "on" => Mode::On,
            "off" => Mode::Off,
            value => {
                return Err(invalid(
                    "RJ_MACHINEVIEW_MODE",
                    value.to_owned(),
                    "on or off",
                ));
            }
        };
        let interval_ms = bounded(
            &mut get,
            "RJ_MACHINEVIEW_INTERVAL_MS",
            5_000,
            MIN_INTERVAL_MS,
            MAX_INTERVAL_MS,
        )?;
        let retention_seconds = bounded(
            &mut get,
            "RJ_MACHINEVIEW_RETENTION_SECONDS",
            86_400,
            MIN_RETENTION_SECONDS,
            MAX_RETENTION_SECONDS,
        )?;
        // The hourly token history is far cheaper per unit of time than the
        // sample ring — 24 buckets a day — so it keeps its own, much longer
        // retention instead of being bounded by `retention_seconds`.
        let token_history_days = bounded(
            &mut get,
            "RJ_MACHINEVIEW_TOKEN_HISTORY_DAYS",
            30,
            MIN_TOKEN_HISTORY_DAYS,
            MAX_TOKEN_HISTORY_DAYS,
        )?;
        // The live stream reads only the proxy's own in-process registry, so
        // it can run far faster than the interval that scrapes engines and
        // the host agent over the network.
        let stream_interval_ms = bounded(
            &mut get,
            "RJ_MACHINEVIEW_STREAM_INTERVAL_MS",
            1_000,
            MIN_STREAM_INTERVAL_MS,
            MAX_STREAM_INTERVAL_MS,
        )?;
        let agent_url = match get("RJ_MACHINEVIEW_AGENT_URL").filter(|value| !value.is_empty()) {
            None => None,
            Some(raw) => Some(Url::parse(&raw).ok().filter(Url::has_host).ok_or_else(|| {
                invalid("RJ_MACHINEVIEW_AGENT_URL", raw, "an absolute http(s) URL")
            })?),
        };
        let state_path = get("RJ_MACHINEVIEW_STATE_PATH")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from);
        let ui_dir = match get("RJ_MACHINEVIEW_UI_DIR") {
            // An explicitly configured directory must exist; the default is
            // best-effort so binaries outside the container image still start.
            Some(raw) if !raw.is_empty() => {
                let path = PathBuf::from(&raw);
                if path.is_dir() {
                    Some(path)
                } else {
                    return Err(invalid(
                        "RJ_MACHINEVIEW_UI_DIR",
                        raw,
                        "an existing directory",
                    ));
                }
            }
            Some(_) => None,
            None => Some(PathBuf::from("/ui")).filter(|path| path.is_dir()),
        };
        Ok(Self {
            mode,
            interval_ms,
            retention_seconds,
            token_history_days,
            stream_interval_ms,
            agent_url,
            state_path,
            ui_dir,
        })
    }
}

fn bounded(
    get: &mut impl FnMut(&str) -> Option<String>,
    key: &'static str,
    default: u64,
    min: u64,
    max: u64,
) -> Result<u64, SettingsError> {
    match get(key) {
        None => Ok(default),
        Some(raw) => raw
            .parse::<u64>()
            .ok()
            .filter(|value| (min..=max).contains(value))
            .ok_or_else(|| invalid(key, raw, "an integer within the documented bounds")),
    }
}

// --- Sample model -----------------------------------------------------------

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Sample {
    pub t: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<HostSample>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub gpus: Vec<GpuSample>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub serving: Option<ServingSample>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub engines: Vec<EngineSample>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub energy: Option<EnergySample>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct HostSample {
    pub cpu_pct: Option<f64>,
    pub load1: Option<f64>,
    pub mem_total_bytes: Option<f64>,
    pub mem_used_bytes: Option<f64>,
    pub mem_cached_bytes: Option<f64>,
    pub swap_total_bytes: Option<f64>,
    pub swap_used_bytes: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dirty_bytes: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub writeback_bytes: Option<f64>,
    pub net_rx_bps: Option<f64>,
    pub net_tx_bps: Option<f64>,
    pub disk_read_bps: Option<f64>,
    pub disk_write_bps: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_read_iops: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_write_iops: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_util_pct: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_inflight: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub iowait_pct: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub io_pressure_pct: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mem_pressure_pct: Option<f64>,
    pub cpu_watts: Option<f64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub disks: Vec<DiskSample>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct DiskSample {
    pub mount: String,
    pub total_bytes: f64,
    pub used_bytes: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inodes_total: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inodes_used: Option<f64>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct GpuSample {
    pub index: u32,
    pub name: String,
    pub util_pct: Option<f64>,
    pub mem_used_bytes: Option<f64>,
    pub mem_total_bytes: Option<f64>,
    pub power_watts: Option<f64>,
    pub temp_c: Option<f64>,
    pub sm_mhz: Option<f64>,
    // Extended telemetry; absent from older agents and optional per driver.
    #[serde(default)]
    pub mem_util_pct: Option<f64>,
    #[serde(default)]
    pub mem_clock_mhz: Option<f64>,
    #[serde(default)]
    pub power_limit_watts: Option<f64>,
    #[serde(default)]
    pub fan_pct: Option<f64>,
    #[serde(default)]
    pub pstate: Option<f64>,
    #[serde(default)]
    pub temp_mem_c: Option<f64>,
    // Throttle reasons as 0/1 flags; a bucket mean is the throttled fraction.
    #[serde(default)]
    pub throttle_sw_power: Option<f64>,
    #[serde(default)]
    pub throttle_sw_thermal: Option<f64>,
    #[serde(default)]
    pub throttle_hw_thermal: Option<f64>,
    #[serde(default)]
    pub throttle_hw: Option<f64>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ServingSample {
    pub inflight: Option<f64>,
    pub requests_per_second: Option<f64>,
    pub prompt_tps: Option<f64>,
    pub gen_tps: Option<f64>,
    pub cached_tps: Option<f64>,
    pub ttft_p50_ms: Option<f64>,
    pub ttft_p95_ms: Option<f64>,
    pub tpot_p95_ms: Option<f64>,
    /// Per-stream decode throughput quantiles over the retained histogram
    /// window: the rate an individual request's decode actually ran at.
    /// Distinct from `gen_tps`, which books a request's whole completion
    /// count into the sample where it finished and therefore spikes on
    /// completion ticks rather than measuring any stream's speed.
    #[serde(default)]
    pub stream_tps_p50: Option<f64>,
    /// Slowest-5% per-stream decode rate: the tail a user actually feels.
    /// (For throughput the bad tail is the low quantile, unlike TTFT.)
    #[serde(default)]
    pub stream_tps_p05: Option<f64>,
    pub cache_hit_pct: Option<f64>,
    /// Which layer `cache_hit_pct`/`cached_tps` came from. Absent when both
    /// are absent; never inferred by the reader.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_hit_source: Option<CacheHitSource>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub upstreams: Vec<UpstreamSample>,
}

/// Provenance of the published cache-hit ratio. The proxy's own token-weighted
/// figure is authoritative because it is measured on the served responses; the
/// engine figure is a strictly weaker substitute that counts every query the
/// engines saw, including traffic this proxy did not route.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheHitSource {
    /// `prompt_tokens_details.cached_tokens` on the proxied responses.
    ResponseUsage,
    /// Summed `vllm:prefix_cache_{hits,queries}_total` across the engines.
    EnginePrefixCache,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct UpstreamSample {
    pub name: String,
    pub up: Option<f64>,
    pub inflight: Option<f64>,
    pub requests_per_second: Option<f64>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct EngineSample {
    pub endpoint: String,
    pub running: Option<f64>,
    pub waiting: Option<f64>,
    pub kv_cache_pct: Option<f64>,
    pub gen_tps: Option<f64>,
    pub prompt_tps: Option<f64>,
    pub prefix_hit_pct: Option<f64>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct EnergySample {
    pub gpu_watts: Option<f64>,
    pub cpu_watts: Option<f64>,
    pub total_watt_hours: f64,
}

/// Payload published by the loopback host agent. Host and GPU shapes reuse the
/// stored sample types so the agent contract and the API stay aligned.
#[derive(Debug, Deserialize)]
struct AgentPayload {
    version: u32,
    #[serde(default)]
    host: Option<HostSample>,
    #[serde(default)]
    gpus: Vec<GpuSample>,
}

// --- Prometheus text parsing ------------------------------------------------

pub type MetricMap = HashMap<String, Vec<(Vec<(String, String)>, f64)>>;

/// Parses the Prometheus text exposition format into name-keyed samples.
///
/// The parser is deliberately tolerant: malformed lines are skipped rather
/// than failing the whole scrape, because one engine emitting one odd line
/// must not blank the entire dashboard.
#[must_use]
pub fn parse_prometheus_text(body: &str) -> MetricMap {
    let mut map = MetricMap::new();
    for line in body.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (name_labels, rest) = match line.find('{') {
            Some(brace) => match line[brace..].find('}') {
                Some(end) => (
                    (&line[..brace], Some(&line[brace + 1..brace + end])),
                    &line[brace + end + 1..],
                ),
                None => continue,
            },
            None => match line.find(char::is_whitespace) {
                Some(space) => ((&line[..space], None), &line[space..]),
                None => continue,
            },
        };
        let (name, raw_labels) = name_labels;
        let mut fields = rest.split_whitespace();
        let Some(value) = fields.next().and_then(parse_prom_value) else {
            continue;
        };
        let labels = raw_labels.map(parse_labels).unwrap_or_default();
        map.entry(name.to_owned())
            .or_default()
            .push((labels, value));
    }
    map
}

fn parse_prom_value(raw: &str) -> Option<f64> {
    match raw {
        "+Inf" | "Inf" => Some(f64::INFINITY),
        "-Inf" => Some(f64::NEG_INFINITY),
        "NaN" => None,
        _ => raw.parse::<f64>().ok().filter(|value| value.is_finite()),
    }
}

fn parse_labels(raw: &str) -> Vec<(String, String)> {
    let mut labels = Vec::new();
    let mut chars = raw.chars().peekable();
    loop {
        let mut key = String::new();
        while let Some(&c) = chars.peek() {
            if c == '=' {
                break;
            }
            chars.next();
            if c != ',' && c != ' ' {
                key.push(c);
            }
        }
        if chars.next().is_none() {
            break;
        }
        if chars.next() != Some('"') {
            break;
        }
        let mut value = String::new();
        let mut escaped = false;
        let mut closed = false;
        for c in chars.by_ref() {
            if escaped {
                match c {
                    'n' => value.push('\n'),
                    other => value.push(other),
                }
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                closed = true;
                break;
            } else {
                value.push(c);
            }
        }
        if !closed {
            break;
        }
        if !key.is_empty() {
            labels.push((key, value));
        }
        match chars.peek() {
            Some(',') => {
                chars.next();
            }
            _ => break,
        }
    }
    labels
}

/// Converts the in-process registry gather into the same map shape as the
/// text parser, flattening histograms into `_bucket` / `_count` / `_sum`.
#[must_use]
pub fn gather_registry(registry: &Registry) -> MetricMap {
    let mut map = MetricMap::new();
    for family in registry.gather() {
        let name = family.name().to_owned();
        let field_type = family.get_field_type();
        for metric in family.get_metric() {
            let labels: Vec<(String, String)> = metric
                .get_label()
                .iter()
                .map(|pair| (pair.name().to_owned(), pair.value().to_owned()))
                .collect();
            match field_type {
                prometheus::proto::MetricType::COUNTER => {
                    map.entry(name.clone())
                        .or_default()
                        .push((labels, metric.get_counter().get_value()));
                }
                prometheus::proto::MetricType::GAUGE => {
                    map.entry(name.clone())
                        .or_default()
                        .push((labels, metric.get_gauge().get_value()));
                }
                prometheus::proto::MetricType::HISTOGRAM => {
                    let histogram = metric.get_histogram();
                    for bucket in histogram.get_bucket() {
                        let mut bucket_labels = labels.clone();
                        bucket_labels.push(("le".to_owned(), format_le(bucket.upper_bound())));
                        map.entry(format!("{name}_bucket"))
                            .or_default()
                            .push((bucket_labels, bucket.cumulative_count() as f64));
                    }
                    let mut inf_labels = labels.clone();
                    inf_labels.push(("le".to_owned(), "+Inf".to_owned()));
                    map.entry(format!("{name}_bucket"))
                        .or_default()
                        .push((inf_labels, histogram.get_sample_count() as f64));
                    map.entry(format!("{name}_count"))
                        .or_default()
                        .push((labels.clone(), histogram.get_sample_count() as f64));
                    map.entry(format!("{name}_sum"))
                        .or_default()
                        .push((labels, histogram.get_sample_sum()));
                }
                _ => {}
            }
        }
    }
    map
}

fn format_le(bound: f64) -> String {
    if bound.is_infinite() {
        "+Inf".to_owned()
    } else {
        format!("{bound}")
    }
}

#[must_use]
pub fn metric_sum(map: &MetricMap, name: &str) -> Option<f64> {
    map.get(name)
        .map(|entries| entries.iter().map(|(_, value)| value).sum())
}

#[must_use]
pub fn metric_first_of(map: &MetricMap, names: &[&str]) -> Option<f64> {
    names.iter().find_map(|name| metric_sum(map, name))
}

/// Sums a metric per value of one label, preserving first-seen order.
#[must_use]
pub fn metric_by_label(map: &MetricMap, name: &str, label: &str) -> Vec<(String, f64)> {
    let mut ordered: Vec<(String, f64)> = Vec::new();
    let Some(entries) = map.get(name) else {
        return ordered;
    };
    for (labels, value) in entries {
        let Some((_, label_value)) = labels.iter().find(|(key, _)| key == label) else {
            continue;
        };
        match ordered.iter_mut().find(|(key, _)| key == label_value) {
            Some((_, existing)) => *existing += value,
            None => ordered.push((label_value.clone(), *value)),
        }
    }
    ordered
}

/// Merges histogram buckets across label sets: `le` upper bound → cumulative count.
#[must_use]
pub fn histogram_buckets(map: &MetricMap, name: &str) -> Vec<(f64, f64)> {
    let mut buckets: Vec<(f64, f64)> = Vec::new();
    let Some(entries) = map.get(&format!("{name}_bucket")) else {
        return buckets;
    };
    for (labels, value) in entries {
        let Some(le) = labels
            .iter()
            .find(|(key, _)| key == "le")
            .and_then(|(_, raw)| match raw.as_str() {
                "+Inf" | "Inf" => Some(f64::INFINITY),
                other => other.parse::<f64>().ok(),
            })
        else {
            continue;
        };
        match buckets.iter_mut().find(|(bound, _)| {
            (*bound - le).abs() < f64::EPSILON || (bound.is_infinite() && le.is_infinite())
        }) {
            Some((_, existing)) => *existing += value,
            None => buckets.push((le, *value)),
        }
    }
    buckets.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    buckets
}

// --- Rate + histogram-window tracking ---------------------------------------

#[derive(Default)]
pub struct RateTracker {
    previous: HashMap<String, (u64, f64)>,
}

impl RateTracker {
    /// Returns the per-second rate since the previous observation of `key`,
    /// or `None` on the first observation or after a counter reset.
    pub fn rate(&mut self, key: &str, t_ms: u64, value: f64) -> Option<f64> {
        let previous = self.previous.insert(key.to_owned(), (t_ms, value));
        let (prev_t, prev_value) = previous?;
        if t_ms <= prev_t || value < prev_value {
            return None;
        }
        Some((value - prev_value) * 1_000.0 / (t_ms - prev_t) as f64)
    }
}

/// Cumulative histogram buckets as `(upper_bound, cumulative_count)` pairs.
pub type Buckets = Vec<(f64, f64)>;

#[derive(Default)]
pub struct HistogramWindows {
    windows: HashMap<String, VecDeque<(u64, Buckets)>>,
}

impl HistogramWindows {
    /// Records a cumulative bucket snapshot and returns the interpolated
    /// quantile over the retained window, or `None` without enough traffic.
    pub fn observe_quantile(
        &mut self,
        key: &str,
        t_ms: u64,
        buckets: Vec<(f64, f64)>,
        quantile: f64,
    ) -> Option<f64> {
        let window = self.windows.entry(key.to_owned()).or_default();
        window.push_back((t_ms, buckets));
        while window
            .front()
            .is_some_and(|(front_t, _)| t_ms.saturating_sub(*front_t) > HISTOGRAM_WINDOW_MS)
            && window.len() > 2
        {
            window.pop_front();
        }
        let (_, oldest) = window.front()?;
        let (_, newest) = window.back()?;
        if window.len() < 2 {
            return None;
        }
        quantile_from_delta(oldest, newest, quantile)
    }
}

fn quantile_from_delta(oldest: &[(f64, f64)], newest: &[(f64, f64)], quantile: f64) -> Option<f64> {
    let mut deltas: Vec<(f64, f64)> = Vec::with_capacity(newest.len());
    for (le, count) in newest {
        let prior = oldest
            .iter()
            .find(|(old_le, _)| {
                (old_le - le).abs() < f64::EPSILON || (old_le.is_infinite() && le.is_infinite())
            })
            .map_or(0.0, |(_, old_count)| *old_count);
        deltas.push((*le, (count - prior).max(0.0)));
    }
    let total = deltas
        .iter()
        .find(|(le, _)| le.is_infinite())
        .map(|(_, count)| *count)?;
    if total <= 0.0 {
        return None;
    }
    let target = total * quantile;
    let mut previous_bound = 0.0_f64;
    let mut previous_count = 0.0_f64;
    for (le, cumulative) in &deltas {
        if *cumulative >= target {
            if le.is_infinite() {
                return Some(previous_bound);
            }
            let bucket_count = cumulative - previous_count;
            if bucket_count <= 0.0 {
                return Some(*le);
            }
            let fraction = (target - previous_count) / bucket_count;
            return Some(previous_bound + (le - previous_bound) * fraction);
        }
        previous_count = *cumulative;
        if !le.is_infinite() {
            previous_bound = *le;
        }
    }
    Some(previous_bound)
}

// --- Store ------------------------------------------------------------------

pub struct Store {
    inner: Mutex<StoreInner>,
}

struct StoreInner {
    samples: VecDeque<Sample>,
    retention_ms: u64,
}

impl Store {
    #[must_use]
    pub fn new(retention_seconds: u64) -> Self {
        Self {
            inner: Mutex::new(StoreInner {
                samples: VecDeque::new(),
                retention_ms: retention_seconds.saturating_mul(1_000),
            }),
        }
    }

    pub fn push(&self, sample: Sample) {
        let mut inner = self.inner.lock();
        let now = sample.t;
        // Ignore out-of-order clock jumps rather than corrupting the ring.
        if inner.samples.back().is_some_and(|latest| latest.t >= now) {
            return;
        }
        inner.samples.push_back(sample);
        let retention_ms = inner.retention_ms;
        while inner
            .samples
            .front()
            .is_some_and(|front| now.saturating_sub(front.t) > retention_ms)
        {
            inner.samples.pop_front();
        }
    }

    #[must_use]
    pub fn latest(&self) -> Option<Sample> {
        self.inner.lock().samples.back().cloned()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.inner.lock().samples.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Returns samples covering the trailing `range_seconds`, merged into at
    /// most `points` buckets (bucket mean for every numeric field).
    #[must_use]
    pub fn query(&self, now_ms: u64, range_seconds: u64, points: usize) -> Vec<Sample> {
        let inner = self.inner.lock();
        let start = now_ms.saturating_sub(range_seconds.saturating_mul(1_000));
        let selected: Vec<&Sample> = inner
            .samples
            .iter()
            .filter(|sample| sample.t >= start)
            .collect();
        if selected.is_empty() {
            return Vec::new();
        }
        let points = points.clamp(1, MAX_SERIES_POINTS);
        if selected.len() <= points {
            return selected.into_iter().cloned().collect();
        }
        let span = range_seconds.saturating_mul(1_000).max(1);
        let bucket_ms = (span / points as u64).max(1);
        let mut merged: Vec<Sample> = Vec::with_capacity(points);
        let mut bucket: Vec<&Sample> = Vec::new();
        let mut bucket_index = None;
        for sample in selected {
            let index = sample.t.saturating_sub(start) / bucket_ms;
            if bucket_index != Some(index) && !bucket.is_empty() {
                merged.push(merge_samples(&bucket));
                bucket.clear();
            }
            bucket_index = Some(index);
            bucket.push(sample);
        }
        if !bucket.is_empty() {
            merged.push(merge_samples(&bucket));
        }
        merged
    }

    #[must_use]
    pub fn snapshot(&self) -> Vec<Sample> {
        self.inner.lock().samples.iter().cloned().collect()
    }

    pub fn restore(&self, samples: Vec<Sample>, now_ms: u64) {
        let mut inner = self.inner.lock();
        let retention_ms = inner.retention_ms;
        let mut restored: VecDeque<Sample> = samples
            .into_iter()
            .filter(|sample| now_ms.saturating_sub(sample.t) <= retention_ms && sample.t <= now_ms)
            .collect();
        restored.make_contiguous().sort_by_key(|sample| sample.t);
        inner.samples = restored;
    }
}

// --- Token history ----------------------------------------------------------

/// Cumulative token and request counters as read from the proxy's registry.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct TokenCounters {
    pub prompt: Option<f64>,
    pub completion: Option<f64>,
    pub cached: Option<f64>,
    pub requests: Option<f64>,
}

/// Reads the cumulative counters the token history integrates.
#[must_use]
pub fn token_counters(map: &MetricMap) -> TokenCounters {
    TokenCounters {
        prompt: metric_sum(map, "ramjet_prompt_tokens_total"),
        completion: metric_sum(map, "ramjet_completion_tokens_total"),
        cached: metric_sum(map, "ramjet_cached_prompt_tokens_total"),
        requests: metric_sum(map, "ramjet_requests_total"),
    }
}

/// One wall-clock hour of counter deltas, keyed by its UTC hour start.
///
/// Buckets are UTC so the stored series is unambiguous; the dashboard groups
/// them into local days and local hours in the browser.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TokenBucket {
    pub t: u64,
    pub prompt: f64,
    pub completion: f64,
    pub cached: f64,
    pub requests: f64,
}

/// A long, cheap history of hourly token volume.
///
/// The sample ring answers "what is the box doing now" at seconds of
/// resolution and costs megabytes per day; this answers "when does this box
/// get used" at hours of resolution and costs 24 records per day, which is
/// what makes a month of it affordable in the same process.
pub struct TokenHistory {
    buckets: VecDeque<TokenBucket>,
    previous: TokenCounters,
    retention_ms: u64,
}

/// A counter that went backwards means the exporting process restarted, so
/// the missing interval is unknowable rather than negative; contribute
/// nothing and re-baseline on the next observation.
fn counter_delta(previous: Option<f64>, current: Option<f64>) -> f64 {
    match (previous, current) {
        (Some(previous), Some(current))
            if previous.is_finite() && current.is_finite() && current >= previous =>
        {
            current - previous
        }
        _ => 0.0,
    }
}

impl TokenHistory {
    #[must_use]
    pub fn new(retention_days: u64) -> Self {
        Self {
            buckets: VecDeque::new(),
            previous: TokenCounters::default(),
            retention_ms: retention_days.saturating_mul(86_400_000),
        }
    }

    /// Folds one scrape of the cumulative counters into its wall-clock hour.
    pub fn observe(&mut self, t_ms: u64, counters: TokenCounters) {
        let hour = t_ms - t_ms % HOUR_MS;
        // A backwards clock would corrupt the ordering the whole API relies
        // on. Re-baseline instead, so the next forward sample is still a
        // usable delta rather than a spike.
        if self.buckets.back().is_some_and(|latest| latest.t > hour) {
            self.previous = counters;
            return;
        }
        let prompt = counter_delta(self.previous.prompt, counters.prompt);
        let completion = counter_delta(self.previous.completion, counters.completion);
        let cached = counter_delta(self.previous.cached, counters.cached);
        let requests = counter_delta(self.previous.requests, counters.requests);
        self.previous = counters;
        if self.buckets.back().is_none_or(|latest| latest.t < hour) {
            self.buckets.push_back(TokenBucket {
                t: hour,
                ..TokenBucket::default()
            });
        }
        let Some(bucket) = self.buckets.back_mut() else {
            return;
        };
        bucket.prompt += prompt;
        bucket.completion += completion;
        bucket.cached += cached;
        bucket.requests += requests;
        self.trim(hour);
    }

    fn trim(&mut self, now_ms: u64) {
        while self
            .buckets
            .front()
            .is_some_and(|front| now_ms.saturating_sub(front.t) > self.retention_ms)
        {
            self.buckets.pop_front();
        }
    }

    /// Returns the buckets covering the trailing `days`, oldest first.
    #[must_use]
    pub fn query(&self, now_ms: u64, days: u64) -> Vec<TokenBucket> {
        let window_ms = days.saturating_mul(86_400_000);
        // The window starts at an hour boundary so the first bucket is whole.
        let start = (now_ms.saturating_sub(window_ms) / HOUR_MS) * HOUR_MS;
        self.buckets
            .iter()
            .filter(|bucket| bucket.t >= start)
            .copied()
            .collect()
    }

    #[must_use]
    pub fn snapshot(&self) -> Vec<TokenBucket> {
        self.buckets.iter().copied().collect()
    }

    /// Restores persisted buckets, dropping anything outside retention.
    ///
    /// The counter baseline is deliberately *not* restored: the process that
    /// produced those counters is gone, so the first scrape after a restart
    /// only re-establishes it.
    pub fn restore(&mut self, buckets: Vec<TokenBucket>, now_ms: u64) {
        let mut restored: Vec<TokenBucket> = buckets
            .into_iter()
            .filter(|bucket| {
                bucket.t <= now_ms && now_ms.saturating_sub(bucket.t) <= self.retention_ms
            })
            .collect();
        restored.sort_by_key(|bucket| bucket.t);
        restored.dedup_by_key(|bucket| bucket.t);
        self.buckets = restored.into();
        self.previous = TokenCounters::default();
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.buckets.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.buckets.is_empty()
    }
}

fn mean(values: impl Iterator<Item = Option<f64>>) -> Option<f64> {
    let mut sum = 0.0;
    let mut count = 0usize;
    for value in values.flatten() {
        sum += value;
        count += 1;
    }
    (count > 0).then(|| sum / count as f64)
}

#[allow(clippy::too_many_lines)]
fn merge_samples(bucket: &[&Sample]) -> Sample {
    let last = bucket.last().expect("non-empty bucket");
    let host = bucket.iter().any(|sample| sample.host.is_some()).then(|| {
        let hosts: Vec<&HostSample> = bucket.iter().filter_map(|s| s.host.as_ref()).collect();
        HostSample {
            cpu_pct: mean(hosts.iter().map(|h| h.cpu_pct)),
            load1: mean(hosts.iter().map(|h| h.load1)),
            mem_total_bytes: mean(hosts.iter().map(|h| h.mem_total_bytes)),
            mem_used_bytes: mean(hosts.iter().map(|h| h.mem_used_bytes)),
            mem_cached_bytes: mean(hosts.iter().map(|h| h.mem_cached_bytes)),
            swap_total_bytes: mean(hosts.iter().map(|h| h.swap_total_bytes)),
            swap_used_bytes: mean(hosts.iter().map(|h| h.swap_used_bytes)),
            net_rx_bps: mean(hosts.iter().map(|h| h.net_rx_bps)),
            net_tx_bps: mean(hosts.iter().map(|h| h.net_tx_bps)),
            disk_read_bps: mean(hosts.iter().map(|h| h.disk_read_bps)),
            disk_write_bps: mean(hosts.iter().map(|h| h.disk_write_bps)),
            disk_read_iops: mean(hosts.iter().map(|h| h.disk_read_iops)),
            disk_write_iops: mean(hosts.iter().map(|h| h.disk_write_iops)),
            disk_util_pct: mean(hosts.iter().map(|h| h.disk_util_pct)),
            disk_inflight: mean(hosts.iter().map(|h| h.disk_inflight)),
            iowait_pct: mean(hosts.iter().map(|h| h.iowait_pct)),
            io_pressure_pct: mean(hosts.iter().map(|h| h.io_pressure_pct)),
            mem_pressure_pct: mean(hosts.iter().map(|h| h.mem_pressure_pct)),
            dirty_bytes: mean(hosts.iter().map(|h| h.dirty_bytes)),
            writeback_bytes: mean(hosts.iter().map(|h| h.writeback_bytes)),
            cpu_watts: mean(hosts.iter().map(|h| h.cpu_watts)),
            disks: hosts.last().map(|h| h.disks.clone()).unwrap_or_default(),
        }
    });
    let gpus = last
        .gpus
        .iter()
        .map(|gpu| {
            let matching: Vec<&GpuSample> = bucket
                .iter()
                .flat_map(|sample| &sample.gpus)
                .filter(|candidate| candidate.index == gpu.index)
                .collect();
            GpuSample {
                index: gpu.index,
                name: gpu.name.clone(),
                util_pct: mean(matching.iter().map(|g| g.util_pct)),
                mem_used_bytes: mean(matching.iter().map(|g| g.mem_used_bytes)),
                mem_total_bytes: mean(matching.iter().map(|g| g.mem_total_bytes)),
                power_watts: mean(matching.iter().map(|g| g.power_watts)),
                temp_c: mean(matching.iter().map(|g| g.temp_c)),
                sm_mhz: mean(matching.iter().map(|g| g.sm_mhz)),
                mem_util_pct: mean(matching.iter().map(|g| g.mem_util_pct)),
                mem_clock_mhz: mean(matching.iter().map(|g| g.mem_clock_mhz)),
                power_limit_watts: mean(matching.iter().map(|g| g.power_limit_watts)),
                fan_pct: mean(matching.iter().map(|g| g.fan_pct)),
                pstate: mean(matching.iter().map(|g| g.pstate)),
                temp_mem_c: mean(matching.iter().map(|g| g.temp_mem_c)),
                throttle_sw_power: mean(matching.iter().map(|g| g.throttle_sw_power)),
                throttle_sw_thermal: mean(matching.iter().map(|g| g.throttle_sw_thermal)),
                throttle_hw_thermal: mean(matching.iter().map(|g| g.throttle_hw_thermal)),
                throttle_hw: mean(matching.iter().map(|g| g.throttle_hw)),
            }
        })
        .collect();
    let serving = bucket
        .iter()
        .any(|sample| sample.serving.is_some())
        .then(|| {
            let entries: Vec<&ServingSample> =
                bucket.iter().filter_map(|s| s.serving.as_ref()).collect();
            let upstreams = entries
                .last()
                .map(|latest| {
                    latest
                        .upstreams
                        .iter()
                        .map(|upstream| UpstreamSample {
                            name: upstream.name.clone(),
                            up: mean(entries.iter().flat_map(|entry| {
                                entry
                                    .upstreams
                                    .iter()
                                    .filter(|candidate| candidate.name == upstream.name)
                                    .map(|candidate| candidate.up)
                            })),
                            inflight: mean(entries.iter().flat_map(|entry| {
                                entry
                                    .upstreams
                                    .iter()
                                    .filter(|candidate| candidate.name == upstream.name)
                                    .map(|candidate| candidate.inflight)
                            })),
                            requests_per_second: mean(entries.iter().flat_map(|entry| {
                                entry
                                    .upstreams
                                    .iter()
                                    .filter(|candidate| candidate.name == upstream.name)
                                    .map(|candidate| candidate.requests_per_second)
                            })),
                        })
                        .collect()
                })
                .unwrap_or_default();
            ServingSample {
                inflight: mean(entries.iter().map(|e| e.inflight)),
                requests_per_second: mean(entries.iter().map(|e| e.requests_per_second)),
                prompt_tps: mean(entries.iter().map(|e| e.prompt_tps)),
                gen_tps: mean(entries.iter().map(|e| e.gen_tps)),
                cached_tps: mean(entries.iter().map(|e| e.cached_tps)),
                ttft_p50_ms: mean(entries.iter().map(|e| e.ttft_p50_ms)),
                ttft_p95_ms: mean(entries.iter().map(|e| e.ttft_p95_ms)),
                tpot_p95_ms: mean(entries.iter().map(|e| e.tpot_p95_ms)),
                stream_tps_p50: mean(entries.iter().map(|e| e.stream_tps_p50)),
                stream_tps_p05: mean(entries.iter().map(|e| e.stream_tps_p05)),
                cache_hit_pct: mean(entries.iter().map(|e| e.cache_hit_pct)),
                cache_hit_source: entries.iter().rev().find_map(|e| e.cache_hit_source),
                upstreams,
            }
        });
    let engines = last
        .engines
        .iter()
        .map(|engine| {
            let matching: Vec<&EngineSample> = bucket
                .iter()
                .flat_map(|sample| &sample.engines)
                .filter(|candidate| candidate.endpoint == engine.endpoint)
                .collect();
            EngineSample {
                endpoint: engine.endpoint.clone(),
                running: mean(matching.iter().map(|e| e.running)),
                waiting: mean(matching.iter().map(|e| e.waiting)),
                kv_cache_pct: mean(matching.iter().map(|e| e.kv_cache_pct)),
                gen_tps: mean(matching.iter().map(|e| e.gen_tps)),
                prompt_tps: mean(matching.iter().map(|e| e.prompt_tps)),
                prefix_hit_pct: mean(matching.iter().map(|e| e.prefix_hit_pct)),
            }
        })
        .collect();
    let energy = bucket
        .iter()
        .any(|sample| sample.energy.is_some())
        .then(|| {
            let entries: Vec<&EnergySample> =
                bucket.iter().filter_map(|s| s.energy.as_ref()).collect();
            EnergySample {
                gpu_watts: mean(entries.iter().map(|e| e.gpu_watts)),
                cpu_watts: mean(entries.iter().map(|e| e.cpu_watts)),
                total_watt_hours: entries.last().map_or(0.0, |latest| latest.total_watt_hours),
            }
        });
    Sample {
        t: last.t,
        host,
        gpus,
        serving,
        engines,
        energy,
    }
}

// --- Sample construction ----------------------------------------------------

/// Folds the proxy's own registry gather into the serving section.
pub fn build_serving_sample(
    map: &MetricMap,
    t_ms: u64,
    rates: &mut RateTracker,
    histograms: &mut HistogramWindows,
) -> ServingSample {
    let requests_per_second = metric_sum(map, "ramjet_requests_total")
        .and_then(|value| rates.rate("self.requests", t_ms, value));
    let prompt_tps = metric_sum(map, "ramjet_prompt_tokens_total")
        .and_then(|value| rates.rate("self.prompt_tokens", t_ms, value));
    // Engines that never emit `prompt_tokens_details.cached_tokens` leave
    // every cache outcome "unknown". Token-weighted hit data does not exist
    // then, and a hard 0 would misreport absence as a cold cache.
    let cache_reporting = metric_by_label(map, "ramjet_cache_requests_total", "outcome")
        .iter()
        .any(|(outcome, count)| outcome != "unknown" && *count > 0.0);
    let cached_tps = metric_sum(map, "ramjet_cached_prompt_tokens_total")
        .and_then(|value| rates.rate("self.cached_tokens", t_ms, value))
        .filter(|_| cache_reporting);
    let gen_tps = metric_sum(map, "ramjet_completion_tokens_total")
        .and_then(|value| rates.rate("self.completion_tokens", t_ms, value));
    let cache_hit_pct = match (prompt_tps, cached_tps) {
        (Some(prompt), Some(cached)) if prompt > 0.0 => {
            Some((cached / prompt * 100.0).clamp(0.0, 100.0))
        }
        _ => None,
    };
    let ttft_buckets = histogram_buckets(map, "ramjet_ttft_seconds");
    let (ttft_p50_ms, ttft_p95_ms) = if ttft_buckets.is_empty() {
        (None, None)
    } else {
        let p50 = histograms
            .observe_quantile("self.ttft.p50", t_ms, ttft_buckets.clone(), 0.5)
            .map(|seconds| seconds * 1_000.0);
        let p95 = histograms
            .observe_quantile("self.ttft.p95", t_ms, ttft_buckets, 0.95)
            .map(|seconds| seconds * 1_000.0);
        (p50, p95)
    };
    let tpot_buckets = histogram_buckets(map, "ramjet_time_per_output_token_seconds");
    let tpot_p95_ms = if tpot_buckets.is_empty() {
        None
    } else {
        histograms
            .observe_quantile("self.tpot.p95", t_ms, tpot_buckets, 0.95)
            .map(|seconds| seconds * 1_000.0)
    };
    let stream_buckets = histogram_buckets(map, "ramjet_decode_tokens_per_second");
    let (stream_tps_p50, stream_tps_p05) = if stream_buckets.is_empty() {
        (None, None)
    } else {
        let p50 =
            histograms.observe_quantile("self.stream_tps.p50", t_ms, stream_buckets.clone(), 0.5);
        let p05 = histograms.observe_quantile("self.stream_tps.p05", t_ms, stream_buckets, 0.05);
        (p50, p05)
    };
    let up_by_upstream = metric_by_label(map, "ramjet_upstream_up", "upstream");
    let inflight_by_upstream = metric_by_label(map, "ramjet_upstream_inflight", "upstream");
    let requests_by_upstream = metric_by_label(map, "ramjet_upstream_requests_total", "upstream");
    let upstreams = up_by_upstream
        .iter()
        .map(|(name, up)| UpstreamSample {
            name: name.clone(),
            up: Some(*up),
            inflight: inflight_by_upstream
                .iter()
                .find(|(candidate, _)| candidate == name)
                .map(|(_, value)| *value),
            requests_per_second: requests_by_upstream
                .iter()
                .find(|(candidate, _)| candidate == name)
                .and_then(|(_, value)| {
                    rates.rate(&format!("self.upstream_requests.{name}"), t_ms, *value)
                }),
        })
        .collect();
    ServingSample {
        inflight: metric_sum(map, "ramjet_requests_inflight"),
        requests_per_second,
        prompt_tps,
        gen_tps,
        cached_tps,
        ttft_p50_ms,
        ttft_p95_ms,
        tpot_p95_ms,
        stream_tps_p50,
        stream_tps_p05,
        cache_hit_pct,
        // The caller fills this in: the engine fallback needs the engine
        // scrapes, which are not available at this layer.
        cache_hit_source: None,
        upstreams,
    }
}

/// One engine's scrape: the published sample plus the raw prefix-cache token
/// rates. The serving section needs those unaveraged, because a fleet-wide
/// ratio has to be token-weighted rather than a mean of four percentages
/// carrying wildly different traffic.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct EngineScrape {
    pub sample: EngineSample,
    pub prefix_hits_per_second: Option<f64>,
    pub prefix_queries_per_second: Option<f64>,
}

/// Folds one engine's scraped vLLM metrics into an engine sample.
pub fn build_engine_sample(
    map: &MetricMap,
    endpoint: &str,
    t_ms: u64,
    rates: &mut RateTracker,
) -> EngineScrape {
    let gen_tps = metric_sum(map, "vllm:generation_tokens_total")
        .and_then(|value| rates.rate(&format!("engine.{endpoint}.generation"), t_ms, value));
    let prompt_tps = metric_sum(map, "vllm:prompt_tokens_total")
        .and_then(|value| rates.rate(&format!("engine.{endpoint}.prompt"), t_ms, value));
    let hits = metric_sum(map, "vllm:prefix_cache_hits_total")
        .and_then(|value| rates.rate(&format!("engine.{endpoint}.prefix_hits"), t_ms, value));
    let queries = metric_sum(map, "vllm:prefix_cache_queries_total")
        .and_then(|value| rates.rate(&format!("engine.{endpoint}.prefix_queries"), t_ms, value));
    let prefix_hit_pct = match (hits, queries) {
        (Some(hits), Some(queries)) if queries > 0.0 => {
            Some((hits / queries * 100.0).clamp(0.0, 100.0))
        }
        _ => None,
    };
    let kv_cache_pct = metric_first_of(
        map,
        &["vllm:gpu_cache_usage_perc", "vllm:kv_cache_usage_perc"],
    )
    .map(|fraction| (fraction * 100.0).clamp(0.0, 100.0));
    EngineScrape {
        sample: EngineSample {
            endpoint: endpoint.to_owned(),
            running: metric_sum(map, "vllm:num_requests_running"),
            waiting: metric_sum(map, "vllm:num_requests_waiting"),
            kv_cache_pct,
            gen_tps,
            prompt_tps,
            prefix_hit_pct,
        },
        prefix_hits_per_second: hits,
        prefix_queries_per_second: queries,
    }
}

/// Token-weighted fleet cache-hit ratio and cached-token rate derived from the
/// engines' own prefix-cache counters.
///
/// This is the fallback for engines that never populate
/// `prompt_tokens_details.cached_tokens`: they still publish
/// `vllm:prefix_cache_{hits,queries}_total`, which measures the same thing one
/// layer down. Rates are summed before dividing so an idle engine contributes
/// nothing instead of dragging the mean, and a `queries` rate of zero yields
/// absence rather than a fabricated 0%.
#[must_use]
pub fn engine_prefix_cache_ratio(scrapes: &[EngineScrape]) -> Option<(f64, f64)> {
    let mut hits = 0.0;
    let mut queries = 0.0;
    let mut observed = false;
    for scrape in scrapes {
        if let (Some(scraped_hits), Some(scraped_queries)) = (
            scrape.prefix_hits_per_second,
            scrape.prefix_queries_per_second,
        ) {
            hits += scraped_hits;
            queries += scraped_queries;
            observed = true;
        }
    }
    if !observed || queries <= 0.0 {
        return None;
    }
    Some((hits.max(0.0), (hits / queries * 100.0).clamp(0.0, 100.0)))
}

/// Fills an absent cache-hit ratio from the engines' prefix-cache counters.
///
/// Only ever writes when the proxy's own response-usage path produced nothing,
/// so an engine fleet that does report `cached_tokens` keeps the authoritative
/// figure. The chosen provenance is published rather than left implicit.
pub fn apply_engine_cache_fallback(serving: &mut ServingSample, scrapes: &[EngineScrape]) {
    if serving.cache_hit_pct.is_some() {
        serving.cache_hit_source = Some(CacheHitSource::ResponseUsage);
        return;
    }
    if let Some((cached_tps, hit_pct)) = engine_prefix_cache_ratio(scrapes) {
        serving.cached_tps = Some(cached_tps);
        serving.cache_hit_pct = Some(hit_pct);
        serving.cache_hit_source = Some(CacheHitSource::EnginePrefixCache);
    }
}

fn finite(value: Option<f64>) -> Option<f64> {
    value.filter(|v| v.is_finite())
}

fn sanitize_host(mut host: HostSample) -> HostSample {
    host.cpu_pct = finite(host.cpu_pct).map(|v| v.clamp(0.0, 100.0));
    host.load1 = finite(host.load1).map(|v| v.max(0.0));
    host.mem_total_bytes = finite(host.mem_total_bytes).map(|v| v.max(0.0));
    host.mem_used_bytes = finite(host.mem_used_bytes).map(|v| v.max(0.0));
    host.mem_cached_bytes = finite(host.mem_cached_bytes).map(|v| v.max(0.0));
    host.swap_total_bytes = finite(host.swap_total_bytes).map(|v| v.max(0.0));
    host.swap_used_bytes = finite(host.swap_used_bytes).map(|v| v.max(0.0));
    host.net_rx_bps = finite(host.net_rx_bps).map(|v| v.max(0.0));
    host.net_tx_bps = finite(host.net_tx_bps).map(|v| v.max(0.0));
    host.disk_read_bps = finite(host.disk_read_bps).map(|v| v.max(0.0));
    host.disk_write_bps = finite(host.disk_write_bps).map(|v| v.max(0.0));
    host.disk_read_iops = finite(host.disk_read_iops).map(|v| v.max(0.0));
    host.disk_write_iops = finite(host.disk_write_iops).map(|v| v.max(0.0));
    host.disk_util_pct = finite(host.disk_util_pct).map(|v| v.clamp(0.0, 100.0));
    host.disk_inflight = finite(host.disk_inflight).map(|v| v.max(0.0));
    host.iowait_pct = finite(host.iowait_pct).map(|v| v.clamp(0.0, 100.0));
    host.io_pressure_pct = finite(host.io_pressure_pct).map(|v| v.clamp(0.0, 100.0));
    host.mem_pressure_pct = finite(host.mem_pressure_pct).map(|v| v.clamp(0.0, 100.0));
    host.dirty_bytes = finite(host.dirty_bytes).map(|v| v.max(0.0));
    host.writeback_bytes = finite(host.writeback_bytes).map(|v| v.max(0.0));
    host.cpu_watts = finite(host.cpu_watts).map(|v| v.max(0.0));
    host.disks.retain(|disk| {
        disk.total_bytes.is_finite() && disk.used_bytes.is_finite() && disk.total_bytes >= 0.0
    });
    for disk in &mut host.disks {
        disk.mount.truncate(128);
        match (finite(disk.inodes_total), finite(disk.inodes_used)) {
            (Some(total), Some(used)) if total > 0.0 => {
                disk.inodes_total = Some(total);
                disk.inodes_used = Some(used.clamp(0.0, total));
            }
            _ => {
                disk.inodes_total = None;
                disk.inodes_used = None;
            }
        }
    }
    host.disks.truncate(16);
    host
}

fn sanitize_gpus(mut gpus: Vec<GpuSample>) -> Vec<GpuSample> {
    gpus.truncate(32);
    for gpu in &mut gpus {
        gpu.name.truncate(80);
        gpu.util_pct = finite(gpu.util_pct).map(|v| v.clamp(0.0, 100.0));
        gpu.mem_used_bytes = finite(gpu.mem_used_bytes).map(|v| v.max(0.0));
        gpu.mem_total_bytes = finite(gpu.mem_total_bytes).map(|v| v.max(0.0));
        gpu.power_watts = finite(gpu.power_watts).map(|v| v.max(0.0));
        gpu.temp_c = finite(gpu.temp_c);
        gpu.sm_mhz = finite(gpu.sm_mhz).map(|v| v.max(0.0));
        gpu.mem_util_pct = finite(gpu.mem_util_pct).map(|v| v.clamp(0.0, 100.0));
        gpu.mem_clock_mhz = finite(gpu.mem_clock_mhz).map(|v| v.max(0.0));
        gpu.power_limit_watts = finite(gpu.power_limit_watts).map(|v| v.max(0.0));
        gpu.fan_pct = finite(gpu.fan_pct).map(|v| v.clamp(0.0, 100.0));
        gpu.pstate = finite(gpu.pstate).map(|v| v.clamp(0.0, 15.0));
        gpu.temp_mem_c = finite(gpu.temp_mem_c);
        gpu.throttle_sw_power = finite(gpu.throttle_sw_power).map(|v| v.clamp(0.0, 1.0));
        gpu.throttle_sw_thermal = finite(gpu.throttle_sw_thermal).map(|v| v.clamp(0.0, 1.0));
        gpu.throttle_hw_thermal = finite(gpu.throttle_hw_thermal).map(|v| v.clamp(0.0, 1.0));
        gpu.throttle_hw = finite(gpu.throttle_hw).map(|v| v.clamp(0.0, 1.0));
    }
    gpus
}

// --- Persistence ------------------------------------------------------------

#[derive(Serialize, Deserialize)]
struct PersistedState {
    version: u32,
    samples: Vec<Sample>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    tokens: Vec<TokenBucket>,
}

fn persist_state(path: &Path, samples: &[Sample], tokens: &[TokenBucket]) -> std::io::Result<()> {
    let state = PersistedState {
        version: STATE_VERSION,
        samples: samples.to_vec(),
        tokens: tokens.to_vec(),
    };
    let body =
        serde_json::to_vec(&state).map_err(|error| std::io::Error::other(error.to_string()))?;
    let mut temporary = path.as_os_str().to_owned();
    temporary.push(".tmp");
    let temporary = PathBuf::from(temporary);
    std::fs::write(&temporary, body)?;
    std::fs::rename(&temporary, path)
}

fn load_state(path: &Path) -> Option<(Vec<Sample>, Vec<TokenBucket>)> {
    let body = std::fs::read(path).ok()?;
    let state: PersistedState = serde_json::from_slice(&body).ok()?;
    ((MIN_STATE_VERSION..=STATE_VERSION).contains(&state.version))
        .then_some((state.samples, state.tokens))
}

// --- Live stream ------------------------------------------------------------

/// One published frame. `serving` frames come from the proxy's own registry
/// on the fast interval; `sample` frames are the full network-scraped sample
/// the ring stores, and arrive on the much slower sampling interval.
#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
enum StreamFrame<'a> {
    Hello {
        now: u64,
        hostname: Option<&'a str>,
        interval_ms: u64,
        stream_interval_ms: u64,
        retention_seconds: u64,
        upstreams: Vec<String>,
    },
    Serving {
        t: u64,
        serving: ServingSample,
    },
    Sample {
        sample: &'a Sample,
    },
}

/// Serializes a frame once for every subscriber rather than per client.
fn encode_frame(frame: &StreamFrame<'_>) -> Option<Arc<str>> {
    serde_json::to_string(frame)
        .ok()
        .map(|body| Arc::from(body.as_str()))
}

/// Decrements the connected-client count however the socket task ends.
struct StreamSlot(Arc<Shared>);

impl Drop for StreamSlot {
    fn drop(&mut self) {
        self.0.stream_clients.fetch_sub(1, Ordering::Relaxed);
    }
}

async fn stream_handler(
    State(shared): State<Arc<Shared>>,
    upgrade: WebSocketUpgrade,
) -> Response<Body> {
    // Reserve the slot before upgrading so a burst of connects cannot race
    // past the cap between the check and the handshake.
    let admitted = shared
        .stream_clients
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
            (current < MAX_STREAM_CLIENTS).then_some(current + 1)
        })
        .is_ok();
    if !admitted {
        return text_error(StatusCode::SERVICE_UNAVAILABLE, "too many stream clients");
    }
    upgrade
        .on_upgrade(move |socket| async move {
            let _slot = StreamSlot(shared.clone());
            stream_socket(shared, socket).await;
        })
        .into_response()
}

async fn stream_socket(shared: Arc<Shared>, mut socket: WebSocket) {
    // Subscribe before sending hello so no frame is missed in between.
    let mut frames = shared.stream.subscribe();
    let hello = StreamFrame::Hello {
        now: now_unix_ms(),
        hostname: shared.hostname.as_deref(),
        interval_ms: shared.settings.interval_ms,
        stream_interval_ms: shared.settings.stream_interval_ms,
        retention_seconds: shared.settings.retention_seconds,
        upstreams: shared
            .upstreams
            .iter()
            .map(|url| url.as_str().trim_end_matches('/').to_owned())
            .collect(),
    };
    let Some(hello) = encode_frame(&hello) else {
        return;
    };
    if socket
        .send(Message::Text(hello.to_string().into()))
        .await
        .is_err()
    {
        return;
    }
    loop {
        tokio::select! {
            frame = frames.recv() => match frame {
                Ok(frame) => {
                    if socket
                        .send(Message::Text(frame.to_string().into()))
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
                // A client too slow for the fast interval is dropped rather
                // than served stale frames from a growing backlog.
                Err(_) => return,
            },
            // Reading is what surfaces a closed socket; the dashboard never
            // sends anything the LB acts on.
            incoming = socket.recv() => match incoming {
                None | Some(Err(_) | Ok(Message::Close(_))) => return,
                Some(Ok(_)) => {}
            },
        }
    }
}

// --- Runtime ----------------------------------------------------------------

pub struct MachineView {
    shared: Arc<Shared>,
    task: tokio::task::JoinHandle<()>,
}

struct Shared {
    settings: Settings,
    store: Store,
    tokens: Mutex<TokenHistory>,
    stream: broadcast::Sender<Arc<str>>,
    stream_clients: AtomicUsize,
    upstreams: Vec<Url>,
    hostname: Option<String>,
}

impl Shared {
    fn publish(&self, frame: &StreamFrame<'_>) {
        if self.stream_clients.load(Ordering::Relaxed) == 0 {
            return;
        }
        if let Some(encoded) = encode_frame(frame) {
            let _ = self.stream.send(encoded);
        }
    }
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as u64)
}

fn read_hostname() -> Option<String> {
    let raw = std::fs::read_to_string("/etc/hostname")
        .ok()
        .or_else(|| std::env::var("HOSTNAME").ok())?;
    let trimmed = raw.trim();
    (!trimmed.is_empty() && trimmed.len() <= 128).then(|| trimmed.to_owned())
}

impl MachineView {
    /// Starts the sampler loop and returns the shared handle, or `None` when
    /// the mode is off.
    #[must_use]
    pub fn start(
        settings: Settings,
        registry: Arc<Registry>,
        client: reqwest::Client,
        upstreams: Vec<Url>,
        mut shutdown: tokio::sync::broadcast::Receiver<()>,
    ) -> Option<Self> {
        if settings.mode == Mode::Off {
            return None;
        }
        let store = Store::new(settings.retention_seconds);
        let mut tokens = TokenHistory::new(settings.token_history_days);
        if let Some(path) = &settings.state_path
            && let Some((samples, buckets)) = load_state(path)
        {
            let now = now_unix_ms();
            store.restore(samples, now);
            tokens.restore(buckets, now);
            tracing::info!(
                samples = store.len(),
                token_hours = tokens.len(),
                "machineview state restored"
            );
        }
        let (stream, _) = broadcast::channel(STREAM_CHANNEL_CAPACITY);
        let shared = Arc::new(Shared {
            settings: settings.clone(),
            store,
            tokens: Mutex::new(tokens),
            stream,
            stream_clients: AtomicUsize::new(0),
            upstreams: upstreams.clone(),
            hostname: read_hostname(),
        });
        // The fast publisher reads only the in-process registry, so it costs
        // nothing on the network and nothing at all while no one is watching.
        let fast_shared = shared.clone();
        let fast_registry = registry.clone();
        let mut fast_shutdown = shutdown.resubscribe();
        tokio::spawn(async move {
            let interval = Duration::from_millis(fast_shared.settings.stream_interval_ms);
            let mut rates = RateTracker::default();
            let mut histograms = HistogramWindows::default();
            loop {
                tokio::select! {
                    _ = fast_shutdown.recv() => break,
                    () = tokio::time::sleep(interval) => {}
                }
                if fast_shared.stream_clients.load(Ordering::Relaxed) == 0 {
                    // Rates are deltas between consecutive observations, so
                    // the tracker restarts cleanly when a client returns.
                    rates = RateTracker::default();
                    continue;
                }
                let t = now_unix_ms();
                let map = gather_registry(&fast_registry);
                let serving = build_serving_sample(&map, t, &mut rates, &mut histograms);
                fast_shared.publish(&StreamFrame::Serving { t, serving });
            }
        });
        let loop_shared = shared.clone();
        let task = tokio::spawn(async move {
            let mut sampler = Sampler {
                registry,
                client,
                upstreams,
                agent_url: settings.agent_url.clone(),
                rates: RateTracker::default(),
                histograms: HistogramWindows::default(),
                total_watt_seconds: loop_shared
                    .store
                    .latest()
                    .and_then(|sample| sample.energy.map(|e| e.total_watt_hours * 3_600.0))
                    .unwrap_or(0.0),
                last_energy_t_ms: None,
            };
            let interval = Duration::from_millis(settings.interval_ms);
            let mut ticks: u64 = 0;
            loop {
                let tick_started = tokio::time::Instant::now();
                let now = now_unix_ms();
                let (sample, counters) = sampler.sample(now).await;
                loop_shared.publish(&StreamFrame::Sample { sample: &sample });
                loop_shared.store.push(sample);
                loop_shared.tokens.lock().observe(now, counters);
                ticks += 1;
                if ticks.is_multiple_of(PERSIST_EVERY_TICKS) {
                    persist_snapshot(&loop_shared).await;
                }
                let elapsed = tick_started.elapsed();
                let wait = interval.saturating_sub(elapsed);
                tokio::select! {
                    _ = shutdown.recv() => break,
                    () = tokio::time::sleep(wait) => {}
                }
            }
            persist_snapshot(&loop_shared).await;
        });
        Some(Self { shared, task })
    }

    /// Observation API routes served on the metrics listener.
    pub fn api_router(&self) -> Router {
        Router::new()
            .route("/api/machineview/summary", get(summary_handler))
            .route("/api/machineview/series", get(series_handler))
            .route("/api/machineview/tokens", get(tokens_handler))
            .route("/api/machineview/stream", get(stream_handler))
            .with_state(self.shared.clone())
    }

    /// Static dashboard routes. These stay separate from the data API so an
    /// authentication layer can protect observations while still serving the
    /// login application.
    ///
    /// # Panics
    ///
    /// Panics only if constructing a redirect from constant headers fails.
    pub fn ui_router(&self) -> Router {
        let mut router = Router::new();
        if self.shared.settings.ui_dir.is_some() {
            let ui_state = self.shared.clone();
            router = router
                .route(
                    "/",
                    get(|| async {
                        Response::builder()
                            .status(StatusCode::TEMPORARY_REDIRECT)
                            .header("location", "/ui/")
                            .body(Body::empty())
                            .expect("valid redirect")
                    }),
                )
                .route(
                    "/ui",
                    get(|| async {
                        Response::builder()
                            .status(StatusCode::TEMPORARY_REDIRECT)
                            .header("location", "/ui/")
                            .body(Body::empty())
                            .expect("valid redirect")
                    }),
                )
                .route(
                    "/ui/",
                    get({
                        let state = ui_state.clone();
                        move || static_response(state.clone(), String::new())
                    }),
                )
                .route(
                    "/ui/{*path}",
                    get({
                        let state = ui_state;
                        move |axum::extract::Path(path): axum::extract::Path<String>| {
                            static_response(state.clone(), path)
                        }
                    }),
                );
        }
        router
    }

    /// Historical combined router used by unauthenticated deployments and
    /// focused machine-view tests.
    pub fn router(&self) -> Router {
        self.api_router().merge(self.ui_router())
    }

    pub async fn shutdown(self) {
        let _ = tokio::time::timeout(Duration::from_secs(5), self.task).await;
    }
}

async fn persist_snapshot(shared: &Arc<Shared>) {
    let Some(path) = shared.settings.state_path.clone() else {
        return;
    };
    let samples = shared.store.snapshot();
    let tokens = shared.tokens.lock().snapshot();
    let result = tokio::task::spawn_blocking(move || persist_state(&path, &samples, &tokens)).await;
    match result {
        Ok(Ok(())) => {}
        Ok(Err(error)) => tracing::warn!(%error, "machineview state persist failed"),
        Err(error) => tracing::warn!(%error, "machineview state persist task failed"),
    }
}

struct Sampler {
    registry: Arc<Registry>,
    client: reqwest::Client,
    upstreams: Vec<Url>,
    agent_url: Option<Url>,
    rates: RateTracker,
    histograms: HistogramWindows,
    total_watt_seconds: f64,
    last_energy_t_ms: Option<u64>,
}

impl Sampler {
    /// Produces the ring sample plus the raw cumulative counters, which the
    /// caller folds into the hourly token history.
    async fn sample(&mut self, t_ms: u64) -> (Sample, TokenCounters) {
        let self_map = gather_registry(&self.registry);
        let counters = token_counters(&self_map);
        let mut serving =
            build_serving_sample(&self_map, t_ms, &mut self.rates, &mut self.histograms);

        let scrape_timeout = Duration::from_secs(4);
        let engine_bodies = futures::future::join_all(self.upstreams.iter().map(|base| {
            let client = self.client.clone();
            let mut url = base.clone();
            let path = format!("{}/metrics", base.path().trim_end_matches('/'));
            url.set_path(&path);
            async move {
                let response = client
                    .get(url)
                    .header("accept-encoding", "identity")
                    .timeout(scrape_timeout)
                    .send()
                    .await
                    .ok()?;
                if !response.status().is_success() {
                    return None;
                }
                response.text().await.ok()
            }
        }))
        .await;
        let mut scrapes = Vec::with_capacity(self.upstreams.len());
        for (base, body) in self.upstreams.iter().zip(engine_bodies) {
            let endpoint = base.as_str().trim_end_matches('/').to_owned();
            if let Some(body) = body {
                let map = parse_prometheus_text(&body);
                scrapes.push(build_engine_sample(&map, &endpoint, t_ms, &mut self.rates));
            } else {
                scrapes.push(EngineScrape {
                    sample: EngineSample {
                        endpoint,
                        ..EngineSample::default()
                    },
                    ..EngineScrape::default()
                });
            }
        }
        apply_engine_cache_fallback(&mut serving, &scrapes);
        let engines: Vec<EngineSample> = scrapes.into_iter().map(|scrape| scrape.sample).collect();

        let mut host = None;
        let mut gpus = Vec::new();
        if let Some(agent_url) = &self.agent_url {
            let payload = self
                .client
                .get(agent_url.clone())
                .timeout(scrape_timeout)
                .send()
                .await
                .ok();
            if let Some(response) = payload
                && response.status().is_success()
                && let Ok(body) = response.text().await
                && let Ok(payload) = serde_json::from_str::<AgentPayload>(&body)
                && payload.version == 1
            {
                host = payload.host.map(sanitize_host);
                gpus = sanitize_gpus(payload.gpus);
            }
        }

        let gpu_watts = {
            let watts: Vec<f64> = gpus.iter().filter_map(|gpu| gpu.power_watts).collect();
            (!watts.is_empty()).then(|| watts.iter().sum())
        };
        let cpu_watts = host.as_ref().and_then(|h| h.cpu_watts);
        let energy = match (gpu_watts, cpu_watts) {
            (None, None) => None,
            (gpu, cpu) => {
                let draw = gpu.unwrap_or(0.0) + cpu.unwrap_or(0.0);
                if let Some(previous) = self.last_energy_t_ms {
                    let dt_seconds = t_ms.saturating_sub(previous) as f64 / 1_000.0;
                    // A gap longer than a few intervals means sampling was
                    // paused; integrating across it would invent energy.
                    if dt_seconds > 0.0 && dt_seconds < 120.0 {
                        self.total_watt_seconds += draw * dt_seconds;
                    }
                }
                self.last_energy_t_ms = Some(t_ms);
                Some(EnergySample {
                    gpu_watts,
                    cpu_watts,
                    total_watt_hours: self.total_watt_seconds / 3_600.0,
                })
            }
        };

        (
            Sample {
                t: t_ms,
                host,
                gpus,
                serving: Some(serving),
                engines,
                energy,
            },
            counters,
        )
    }
}

// --- HTTP API ---------------------------------------------------------------

#[derive(Serialize)]
struct SummaryResponse {
    now: u64,
    hostname: Option<String>,
    interval_ms: u64,
    retention_seconds: u64,
    upstreams: Vec<String>,
    latest: Option<Sample>,
}

async fn summary_handler(State(shared): State<Arc<Shared>>) -> Response<Body> {
    let response = SummaryResponse {
        now: now_unix_ms(),
        hostname: shared.hostname.clone(),
        interval_ms: shared.settings.interval_ms,
        retention_seconds: shared.settings.retention_seconds,
        upstreams: shared
            .upstreams
            .iter()
            .map(|url| url.as_str().trim_end_matches('/').to_owned())
            .collect(),
        latest: shared.store.latest(),
    };
    json_response(&response)
}

#[derive(Deserialize)]
struct SeriesParams {
    range: Option<u64>,
    points: Option<usize>,
}

#[derive(Serialize)]
struct SeriesResponse {
    now: u64,
    range_seconds: u64,
    points: Vec<Sample>,
}

async fn series_handler(
    State(shared): State<Arc<Shared>>,
    Query(params): Query<SeriesParams>,
) -> Response<Body> {
    let now = now_unix_ms();
    let range_seconds = params
        .range
        .unwrap_or(3_600)
        .clamp(60, shared.settings.retention_seconds);
    let points = params
        .points
        .unwrap_or(DEFAULT_SERIES_POINTS)
        .clamp(10, MAX_SERIES_POINTS);
    let response = SeriesResponse {
        now,
        range_seconds,
        points: shared.store.query(now, range_seconds, points),
    };
    json_response(&response)
}

#[derive(Deserialize)]
struct TokensParams {
    days: Option<u64>,
}

#[derive(Serialize)]
struct TokensResponse {
    now: u64,
    days: u64,
    bucket_seconds: u64,
    buckets: Vec<TokenBucket>,
}

async fn tokens_handler(
    State(shared): State<Arc<Shared>>,
    Query(params): Query<TokensParams>,
) -> Response<Body> {
    let now = now_unix_ms();
    let days = params
        .days
        .unwrap_or(shared.settings.token_history_days)
        .clamp(MIN_TOKEN_HISTORY_DAYS, shared.settings.token_history_days);
    let response = TokensResponse {
        now,
        days,
        bucket_seconds: HOUR_MS / 1_000,
        buckets: shared.tokens.lock().query(now, days),
    };
    json_response(&response)
}

fn json_response<T: Serialize>(value: &T) -> Response<Body> {
    match serde_json::to_vec(value) {
        Ok(body) => Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "application/json")
            .header("cache-control", "no-store")
            .body(Body::from(body))
            .expect("valid json response"),
        Err(_) => Response::builder()
            .status(StatusCode::INTERNAL_SERVER_ERROR)
            .body(Body::from("encoding failed"))
            .expect("valid error response"),
    }
}

// --- Static UI serving ------------------------------------------------------

/// Resolves a request path to a relative file path inside the UI bundle.
/// Rejects traversal, hidden files, and absolute/backslash components; an
/// extension-less path falls back to the SPA entry point.
#[must_use]
pub fn resolve_static_path(raw: &str) -> Option<String> {
    if raw.contains('\\') || raw.contains('\0') {
        return None;
    }
    let mut components = Vec::new();
    for component in raw.split('/') {
        if component.is_empty() {
            continue;
        }
        if component == ".." || component.starts_with('.') {
            return None;
        }
        components.push(component);
    }
    if components.is_empty() {
        return Some("index.html".to_owned());
    }
    let joined = components.join("/");
    let is_file = components.last().is_some_and(|name| name.contains('.'));
    if is_file {
        Some(joined)
    } else {
        Some("index.html".to_owned())
    }
}

#[must_use]
pub fn content_type_for(path: &str) -> &'static str {
    match path.rsplit('.').next().unwrap_or_default() {
        "html" => "text/html; charset=utf-8",
        "js" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" | "map" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "ico" => "image/x-icon",
        "woff2" => "font/woff2",
        "txt" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

async fn static_response(shared: Arc<Shared>, raw_path: String) -> Response<Body> {
    let Some(ui_dir) = shared.settings.ui_dir.clone() else {
        return text_error(StatusCode::NOT_FOUND, "ui disabled");
    };
    let Some(relative) = resolve_static_path(&raw_path) else {
        return text_error(StatusCode::NOT_FOUND, "not found");
    };
    let full = ui_dir.join(&relative);
    let read = tokio::task::spawn_blocking(move || {
        let metadata = std::fs::metadata(&full).ok()?;
        if !metadata.is_file() || metadata.len() > MAX_STATIC_FILE_BYTES {
            return None;
        }
        std::fs::read(&full).ok()
    })
    .await;
    match read {
        Ok(Some(body)) => {
            let cache_control = if std::path::Path::new(&relative)
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("html"))
            {
                "no-cache"
            } else {
                "public, max-age=3600"
            };
            Response::builder()
                .status(StatusCode::OK)
                .header("content-type", content_type_for(&relative))
                .header("cache-control", cache_control)
                .body(Body::from(body))
                .expect("valid static response")
        }
        _ => text_error(StatusCode::NOT_FOUND, "not found"),
    }
}

fn text_error(status: StatusCode, message: &'static str) -> Response<Body> {
    Response::builder()
        .status(status)
        .header("content-type", "text/plain")
        .body(Body::from(message))
        .expect("valid error response")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_at(t: u64, cpu: f64) -> Sample {
        Sample {
            t,
            host: Some(HostSample {
                cpu_pct: Some(cpu),
                ..HostSample::default()
            }),
            ..Sample::default()
        }
    }

    #[test]
    fn settings_defaults_are_bounded() {
        let settings = Settings::from_lookup(|_| None).expect("defaults valid");
        assert_eq!(settings.mode, Mode::On);
        assert_eq!(settings.interval_ms, 5_000);
        assert_eq!(settings.retention_seconds, 86_400);
        assert!(settings.agent_url.is_none());
        assert!(settings.state_path.is_none());
    }

    #[test]
    fn settings_reject_invalid_values() {
        let mode = Settings::from_lookup(|key| {
            (key == "RJ_MACHINEVIEW_MODE").then(|| "observe".to_owned())
        });
        assert!(mode.is_err());
        let interval = Settings::from_lookup(|key| {
            (key == "RJ_MACHINEVIEW_INTERVAL_MS").then(|| "10".to_owned())
        });
        assert!(interval.is_err());
        let agent = Settings::from_lookup(|key| {
            (key == "RJ_MACHINEVIEW_AGENT_URL").then(|| "not a url".to_owned())
        });
        assert!(agent.is_err());
        let ui = Settings::from_lookup(|key| {
            (key == "RJ_MACHINEVIEW_UI_DIR").then(|| "/definitely/not/a/real/dir".to_owned())
        });
        assert!(ui.is_err());
    }

    #[test]
    fn prometheus_text_parses_values_labels_and_histograms() {
        let body = concat!(
            "# HELP vllm:num_requests_running requests\n",
            "# TYPE vllm:num_requests_running gauge\n",
            "vllm:num_requests_running{model_name=\"deepseek\"} 7\n",
            "vllm:generation_tokens_total{model_name=\"deepseek\"} 1234.5\n",
            "ramjet_ttft_seconds_bucket{endpoint=\"chat\",le=\"0.5\"} 3\n",
            "ramjet_ttft_seconds_bucket{endpoint=\"chat\",le=\"+Inf\"} 4\n",
            "bad line without value\n",
            "vllm:num_requests_waiting 2\n",
        );
        let map = parse_prometheus_text(body);
        assert_eq!(metric_sum(&map, "vllm:num_requests_running"), Some(7.0));
        assert_eq!(metric_sum(&map, "vllm:num_requests_waiting"), Some(2.0));
        assert_eq!(
            metric_sum(&map, "vllm:generation_tokens_total"),
            Some(1234.5)
        );
        let buckets = histogram_buckets(&map, "ramjet_ttft_seconds");
        assert_eq!(buckets.len(), 2);
        assert_eq!(buckets[0], (0.5, 3.0));
        assert!(buckets[1].0.is_infinite());
    }

    #[test]
    fn prometheus_label_values_with_escapes_and_commas() {
        let body = "metric{a=\"x,y\",b=\"q\\\"z\"} 1\n";
        let map = parse_prometheus_text(body);
        let entries = map.get("metric").expect("metric parsed");
        assert_eq!(
            entries[0].0,
            vec![
                ("a".to_owned(), "x,y".to_owned()),
                ("b".to_owned(), "q\"z".to_owned())
            ]
        );
    }

    #[test]
    fn metric_by_label_sums_and_preserves_order() {
        let body = concat!(
            "up{upstream=\"b\",code=\"200\"} 1\n",
            "up{upstream=\"a\",code=\"200\"} 2\n",
            "up{upstream=\"b\",code=\"500\"} 3\n",
        );
        let map = parse_prometheus_text(body);
        let by_label = metric_by_label(&map, "up", "upstream");
        assert_eq!(by_label, vec![("b".to_owned(), 4.0), ("a".to_owned(), 2.0)]);
    }

    #[test]
    fn rate_tracker_handles_first_sample_resets_and_rates() {
        let mut rates = RateTracker::default();
        assert_eq!(rates.rate("k", 1_000, 100.0), None);
        assert_eq!(rates.rate("k", 6_000, 150.0), Some(10.0));
        // Counter reset: no negative rates.
        assert_eq!(rates.rate("k", 11_000, 3.0), None);
        assert_eq!(rates.rate("k", 16_000, 8.0), Some(1.0));
    }

    #[test]
    fn histogram_window_quantile_interpolates_delta() {
        let mut windows = HistogramWindows::default();
        let start = vec![(0.1, 0.0), (0.5, 0.0), (f64::INFINITY, 0.0)];
        assert_eq!(
            windows.observe_quantile("h", 1_000, start, 0.95),
            None,
            "single snapshot has no delta"
        );
        let later = vec![(0.1, 10.0), (0.5, 90.0), (f64::INFINITY, 100.0)];
        let p50 = windows
            .observe_quantile("h", 6_000, later, 0.5)
            .expect("p50 available");
        assert!((p50 - 0.3).abs() < 0.01, "p50 was {p50}");
    }

    #[test]
    fn store_trims_by_retention_and_rejects_backwards_time() {
        let store = Store::new(10);
        store.push(sample_at(1_000, 10.0));
        store.push(sample_at(500, 10.0));
        assert_eq!(store.len(), 1, "backwards sample rejected");
        store.push(sample_at(12_000, 20.0));
        assert_eq!(store.len(), 1, "expired sample trimmed");
        assert_eq!(store.latest().expect("latest").t, 12_000);
    }

    #[test]
    fn store_query_downsamples_with_bucket_means() {
        let store = Store::new(1_000);
        for i in 0..100u64 {
            store.push(sample_at((i + 1) * 1_000, i as f64));
        }
        let now = 100_000;
        let merged = store.query(now, 100, 10);
        assert!(merged.len() <= 11, "got {} buckets", merged.len());
        let first = merged.first().expect("first bucket");
        let cpu = first.host.as_ref().and_then(|h| h.cpu_pct).expect("cpu");
        assert!(cpu > 0.0 && cpu < 15.0, "bucket mean was {cpu}");
        let raw = store.query(now, 100, 2_000);
        assert_eq!(raw.len(), 100, "under the cap the raw samples pass through");
    }

    #[test]
    fn merge_samples_averages_gpus_and_engines_by_identity() {
        let mut a = Sample {
            t: 1_000,
            ..Sample::default()
        };
        a.gpus.push(GpuSample {
            index: 0,
            name: "gpu0".to_owned(),
            util_pct: Some(20.0),
            ..GpuSample::default()
        });
        a.engines.push(EngineSample {
            endpoint: "http://e:8000".to_owned(),
            running: Some(4.0),
            ..EngineSample::default()
        });
        let mut b = Sample {
            t: 2_000,
            ..Sample::default()
        };
        b.gpus.push(GpuSample {
            index: 0,
            name: "gpu0".to_owned(),
            util_pct: Some(40.0),
            ..GpuSample::default()
        });
        b.engines.push(EngineSample {
            endpoint: "http://e:8000".to_owned(),
            running: Some(6.0),
            ..EngineSample::default()
        });
        let merged = merge_samples(&[&a, &b]);
        assert_eq!(merged.t, 2_000);
        assert_eq!(merged.gpus[0].util_pct, Some(30.0));
        assert_eq!(merged.engines[0].running, Some(5.0));
    }

    #[test]
    fn persistence_roundtrip_and_retention_filter() {
        let dir = std::env::temp_dir().join(format!(
            "machineview-test-{}-{}",
            std::process::id(),
            now_unix_ms()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let path = dir.join("state.json");
        let samples = vec![sample_at(1_000, 1.0), sample_at(2_000, 2.0)];
        let buckets = vec![TokenBucket {
            t: 0,
            prompt: 10.0,
            completion: 2.0,
            cached: 4.0,
            requests: 1.0,
        }];
        persist_state(&path, &samples, &buckets).expect("persist");
        let (loaded, loaded_tokens) = load_state(&path).expect("load");
        assert_eq!(loaded, samples);
        assert_eq!(loaded_tokens, buckets);
        let store = Store::new(10);
        store.restore(loaded, 5_000);
        assert_eq!(store.len(), 2);
        store.restore(vec![sample_at(1_000, 1.0)], 500_000);
        assert_eq!(store.len(), 0, "stale samples dropped on restore");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn version_one_state_without_tokens_still_loads() {
        let dir = std::env::temp_dir().join(format!(
            "machineview-v1-{}-{}",
            std::process::id(),
            now_unix_ms()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let path = dir.join("state.json");
        std::fs::write(&path, br#"{"version":1,"samples":[{"t":1000}]}"#).expect("write v1 state");
        let (samples, tokens) = load_state(&path).expect("v1 state loads");
        assert_eq!(samples.len(), 1);
        assert!(tokens.is_empty());
        std::fs::write(&path, br#"{"version":99,"samples":[]}"#).expect("write future state");
        assert!(
            load_state(&path).is_none(),
            "a newer schema must be ignored, not misread"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Bucket sums are exact small integers; the tolerance is only here
    /// because comparing f64 with `==` is a lint, not because they drift.
    #[track_caller]
    fn assert_tokens(actual: f64, expected: f64) {
        assert!(
            (actual - expected).abs() < 1e-9,
            "expected {expected}, got {actual}"
        );
    }

    fn counters(prompt: f64, completion: f64, cached: f64, requests: f64) -> TokenCounters {
        TokenCounters {
            prompt: Some(prompt),
            completion: Some(completion),
            cached: Some(cached),
            requests: Some(requests),
        }
    }

    #[test]
    fn token_history_accumulates_deltas_into_hour_buckets() {
        let mut history = TokenHistory::new(30);
        // The first observation only establishes the baseline: the counters
        // already held whatever the process served before this scrape.
        history.observe(HOUR_MS, counters(1_000.0, 100.0, 400.0, 5.0));
        assert_eq!(history.len(), 1);
        assert_tokens(history.snapshot()[0].prompt, 0.0);

        history.observe(HOUR_MS + 60_000, counters(1_500.0, 150.0, 600.0, 8.0));
        history.observe(HOUR_MS + 120_000, counters(1_800.0, 170.0, 700.0, 10.0));
        let first = history.snapshot()[0];
        assert_eq!(first.t, HOUR_MS);
        assert_tokens(first.prompt, 800.0);
        assert_tokens(first.completion, 70.0);
        assert_tokens(first.cached, 300.0);
        assert_tokens(first.requests, 5.0);

        // A later hour opens a new bucket without disturbing the closed one.
        history.observe(2 * HOUR_MS + 1_000, counters(2_000.0, 180.0, 750.0, 11.0));
        let buckets = history.snapshot();
        assert_eq!(buckets.len(), 2);
        assert_tokens(buckets[0].prompt, 800.0);
        assert_eq!(buckets[1].t, 2 * HOUR_MS);
        assert_tokens(buckets[1].prompt, 200.0);
    }

    #[test]
    fn token_history_treats_a_counter_reset_as_unknowable_not_negative() {
        let mut history = TokenHistory::new(30);
        history.observe(HOUR_MS, counters(1_000.0, 100.0, 0.0, 5.0));
        history.observe(HOUR_MS + 10_000, counters(2_000.0, 200.0, 0.0, 9.0));
        // The proxy restarted: counters are back at zero.
        history.observe(HOUR_MS + 20_000, counters(0.0, 0.0, 0.0, 0.0));
        history.observe(HOUR_MS + 30_000, counters(300.0, 30.0, 0.0, 2.0));
        let bucket = history.snapshot()[0];
        // The reset contributes nothing and the next delta is measured from
        // zero: 1,000 before it, 300 after.
        assert_tokens(bucket.prompt, 1_300.0);
        assert_tokens(bucket.requests, 6.0);
        assert!(bucket.prompt >= 0.0);
    }

    #[test]
    fn token_history_ignores_absent_counters_and_backwards_clocks() {
        let mut history = TokenHistory::new(30);
        history.observe(2 * HOUR_MS, counters(100.0, 10.0, 0.0, 1.0));
        history.observe(2 * HOUR_MS + 5_000, TokenCounters::default());
        history.observe(2 * HOUR_MS + 10_000, counters(500.0, 50.0, 0.0, 4.0));
        // A scrape that lost the metric must not be read as a delta.
        assert_tokens(history.snapshot()[0].prompt, 0.0);
        // A clock jump backwards must not push an out-of-order bucket.
        history.observe(HOUR_MS, counters(900.0, 90.0, 0.0, 7.0));
        let buckets = history.snapshot();
        assert_eq!(buckets.len(), 1);
        assert_eq!(buckets[0].t, 2 * HOUR_MS);
    }

    #[test]
    fn token_history_trims_by_retention_and_queries_a_window() {
        let mut history = TokenHistory::new(1);
        for hour in 0..30u64 {
            history.observe(hour * HOUR_MS, counters(hour as f64 * 100.0, 0.0, 0.0, 0.0));
        }
        let buckets = history.snapshot();
        assert!(
            buckets.len() <= 25,
            "one day of retention keeps at most 25 hour boundaries, got {}",
            buckets.len()
        );
        assert_eq!(buckets.last().expect("a bucket").t, 29 * HOUR_MS);
        let window = history.query(29 * HOUR_MS, 1);
        assert!(window.iter().all(|bucket| bucket.t >= 5 * HOUR_MS));
    }

    #[test]
    fn token_history_restore_drops_stale_buckets_and_the_counter_baseline() {
        let mut history = TokenHistory::new(1);
        let now = 100 * HOUR_MS;
        history.restore(
            vec![
                TokenBucket {
                    t: 2 * HOUR_MS,
                    prompt: 1.0,
                    ..TokenBucket::default()
                },
                TokenBucket {
                    t: now,
                    prompt: 7.0,
                    ..TokenBucket::default()
                },
                TokenBucket {
                    t: now + HOUR_MS,
                    prompt: 9.0,
                    ..TokenBucket::default()
                },
            ],
            now,
        );
        let buckets = history.snapshot();
        assert_eq!(buckets.len(), 1, "stale and future buckets are dropped");
        assert_tokens(buckets[0].prompt, 7.0);
        // The restored history came from a process whose counters are gone;
        // the first scrape after restart must only re-baseline.
        history.observe(now + 60_000, counters(50_000.0, 5_000.0, 0.0, 400.0));
        assert_tokens(history.snapshot()[0].prompt, 7.0);
        history.observe(now + 120_000, counters(50_100.0, 5_010.0, 0.0, 401.0));
        assert_tokens(history.snapshot()[0].prompt, 107.0);
    }

    #[test]
    fn stream_frames_are_tagged_and_carry_their_payload() {
        let hello = encode_frame(&StreamFrame::Hello {
            now: 7,
            hostname: Some("node06"),
            interval_ms: 5_000,
            stream_interval_ms: 1_000,
            retention_seconds: 86_400,
            upstreams: vec!["http://a:8000".to_owned()],
        })
        .expect("hello encodes");
        assert!(hello.contains(r#""kind":"hello""#), "{hello}");
        assert!(hello.contains(r#""stream_interval_ms":1000"#), "{hello}");

        let serving = encode_frame(&StreamFrame::Serving {
            t: 11,
            serving: ServingSample {
                gen_tps: Some(1_234.5),
                ..ServingSample::default()
            },
        })
        .expect("serving encodes");
        assert!(serving.contains(r#""kind":"serving""#), "{serving}");
        assert!(serving.contains(r#""gen_tps":1234.5"#), "{serving}");

        let sample = sample_at(21, 42.0);
        let full = encode_frame(&StreamFrame::Sample { sample: &sample }).expect("sample encodes");
        assert!(full.contains(r#""kind":"sample""#), "{full}");
        assert!(full.contains(r#""cpu_pct":42.0"#), "{full}");
    }

    #[test]
    fn stream_interval_defaults_to_one_second_and_is_bounded() {
        let settings = Settings::from_lookup(|_| None).expect("defaults valid");
        assert_eq!(settings.stream_interval_ms, 1_000);
        let fast = Settings::from_lookup(|key| {
            (key == "RJ_MACHINEVIEW_STREAM_INTERVAL_MS").then(|| "250".to_owned())
        })
        .expect("250ms is inside the bounds");
        assert_eq!(fast.stream_interval_ms, 250);
        for rejected in ["0", "50", "60000", "every second"] {
            assert!(
                Settings::from_lookup(|key| {
                    (key == "RJ_MACHINEVIEW_STREAM_INTERVAL_MS").then(|| rejected.to_owned())
                })
                .is_err(),
                "{rejected} must be rejected"
            );
        }
    }

    #[test]
    fn token_counters_read_the_proxy_registry_shapes() {
        let map = parse_prometheus_text(concat!(
            "ramjet_prompt_tokens_total 1234\n",
            "ramjet_completion_tokens_total 56\n",
            "ramjet_cached_prompt_tokens_total 789\n",
            "ramjet_requests_total{route=\"chat\"} 3\n",
            "ramjet_requests_total{route=\"completions\"} 4\n",
        ));
        let counters = token_counters(&map);
        assert_eq!(counters.prompt, Some(1234.0));
        assert_eq!(counters.completion, Some(56.0));
        assert_eq!(counters.cached, Some(789.0));
        assert_eq!(counters.requests, Some(7.0), "labelled series are summed");
        assert_eq!(token_counters(&MetricMap::new()), TokenCounters::default());
    }

    #[test]
    fn build_engine_sample_computes_rates_and_percentages() {
        let first = parse_prometheus_text(concat!(
            "vllm:generation_tokens_total 1000\n",
            "vllm:prompt_tokens_total 5000\n",
            "vllm:prefix_cache_hits_total 100\n",
            "vllm:prefix_cache_queries_total 200\n",
            "vllm:gpu_cache_usage_perc 0.42\n",
            "vllm:num_requests_running 3\n",
            "vllm:num_requests_waiting 1\n",
        ));
        let mut rates = RateTracker::default();
        let sample = build_engine_sample(&first, "http://e:8000", 1_000, &mut rates).sample;
        assert_eq!(sample.running, Some(3.0));
        assert_eq!(sample.waiting, Some(1.0));
        assert_eq!(sample.kv_cache_pct, Some(42.0));
        assert_eq!(sample.gen_tps, None, "first scrape has no rate");
        let second = parse_prometheus_text(concat!(
            "vllm:generation_tokens_total 2000\n",
            "vllm:prompt_tokens_total 6000\n",
            "vllm:prefix_cache_hits_total 175\n",
            "vllm:prefix_cache_queries_total 300\n",
            "vllm:gpu_cache_usage_perc 0.5\n",
            "vllm:num_requests_running 2\n",
            "vllm:num_requests_waiting 0\n",
        ));
        let sample = build_engine_sample(&second, "http://e:8000", 6_000, &mut rates).sample;
        assert_eq!(sample.gen_tps, Some(200.0));
        assert_eq!(sample.prompt_tps, Some(200.0));
        assert_eq!(sample.prefix_hit_pct, Some(75.0));
    }

    #[test]
    fn build_serving_sample_reads_registry_shapes() {
        let body = concat!(
            "ramjet_requests_inflight 5\n",
            "ramjet_requests_total{code=\"200\"} 100\n",
            "ramjet_prompt_tokens_total{endpoint=\"chat\"} 1000\n",
            "ramjet_cached_prompt_tokens_total{endpoint=\"chat\"} 400\n",
            "ramjet_completion_tokens_total{endpoint=\"chat\"} 300\n",
            "ramjet_upstream_up{upstream=\"http://a:8000\"} 1\n",
            "ramjet_upstream_up{upstream=\"http://b:8000\"} 0\n",
            "ramjet_upstream_inflight{upstream=\"http://a:8000\"} 4\n",
        );
        let map = parse_prometheus_text(body);
        let mut rates = RateTracker::default();
        let mut histograms = HistogramWindows::default();
        let first = build_serving_sample(&map, 1_000, &mut rates, &mut histograms);
        assert_eq!(first.inflight, Some(5.0));
        assert_eq!(first.upstreams.len(), 2);
        assert_eq!(first.upstreams[0].up, Some(1.0));
        assert_eq!(first.upstreams[0].inflight, Some(4.0));
        assert_eq!(first.upstreams[1].up, Some(0.0));
        let body = concat!(
            "ramjet_requests_inflight 5\n",
            "ramjet_requests_total{code=\"200\"} 110\n",
            "ramjet_prompt_tokens_total{endpoint=\"chat\"} 2000\n",
            "ramjet_cached_prompt_tokens_total{endpoint=\"chat\"} 900\n",
            "ramjet_completion_tokens_total{endpoint=\"chat\"} 800\n",
            "ramjet_cache_requests_total{endpoint=\"chat\",outcome=\"partial\"} 5\n",
        );
        let map = parse_prometheus_text(body);
        let second = build_serving_sample(&map, 6_000, &mut rates, &mut histograms);
        assert_eq!(second.requests_per_second, Some(2.0));
        assert_eq!(second.prompt_tps, Some(200.0));
        assert_eq!(second.cached_tps, Some(100.0));
        assert_eq!(second.gen_tps, Some(100.0));
        assert_eq!(second.cache_hit_pct, Some(50.0));
    }

    #[test]
    fn build_serving_sample_surfaces_per_stream_decode_quantiles() {
        let mut rates = RateTracker::default();
        let mut histograms = HistogramWindows::default();
        let body_at = |b60: u64, b120: u64, inf: u64| {
            format!(
                concat!(
                    "ramjet_decode_tokens_per_second_bucket{{endpoint=\"chat\",le=\"60\"}} {}\n",
                    "ramjet_decode_tokens_per_second_bucket{{endpoint=\"chat\",le=\"120\"}} {}\n",
                    "ramjet_decode_tokens_per_second_bucket{{endpoint=\"chat\",le=\"+Inf\"}} {}\n",
                ),
                b60, b120, inf
            )
        };
        let map = parse_prometheus_text(&body_at(0, 0, 0));
        let first = build_serving_sample(&map, 1_000, &mut rates, &mut histograms);
        assert_eq!(first.stream_tps_p50, None);
        assert_eq!(first.stream_tps_p05, None);
        // 100 requests land in the window: 10 below 60 tok/s, 90 in 60-120.
        let map = parse_prometheus_text(&body_at(10, 100, 100));
        let second = build_serving_sample(&map, 6_000, &mut rates, &mut histograms);
        let p50 = second.stream_tps_p50.expect("p50 after traffic");
        let p05 = second.stream_tps_p05.expect("p05 after traffic");
        // Median falls in the 60-120 bucket; the slow 5% inside 0-60.
        assert!((60.0..=120.0).contains(&p50), "p50 {p50}");
        assert!((0.0..=60.0).contains(&p05), "p05 {p05}");
        assert!(p05 < p50, "tail must sit below the median");
    }

    #[test]
    fn cache_hit_is_absent_when_engines_never_report_cached_tokens() {
        let mut rates = RateTracker::default();
        let mut histograms = HistogramWindows::default();
        let body_at = |prompt: u64, unknown: u64| {
            format!(
                concat!(
                    "ramjet_prompt_tokens_total{{endpoint=\"chat\"}} {}\n",
                    "ramjet_cached_prompt_tokens_total{{endpoint=\"chat\"}} 0\n",
                    "ramjet_cache_requests_total{{endpoint=\"chat\",outcome=\"unknown\"}} {}\n",
                ),
                prompt, unknown
            )
        };
        let map = parse_prometheus_text(&body_at(1_000, 40));
        build_serving_sample(&map, 1_000, &mut rates, &mut histograms);
        let map = parse_prometheus_text(&body_at(2_000, 80));
        let sample = build_serving_sample(&map, 6_000, &mut rates, &mut histograms);
        assert_eq!(sample.prompt_tps, Some(200.0));
        assert_eq!(
            sample.cached_tps, None,
            "unknown-only outcomes mean the engine reports no cache detail"
        );
        assert_eq!(sample.cache_hit_pct, None);
        assert_eq!(sample.cache_hit_source, None);
    }

    /// Two scrapes per engine: the first seeds the rate tracker, the second
    /// produces the rate the fallback consumes.
    fn scrape_pair(
        rates: &mut RateTracker,
        endpoint: &str,
        first: &str,
        second: &str,
    ) -> EngineScrape {
        build_engine_sample(&parse_prometheus_text(first), endpoint, 1_000, rates);
        build_engine_sample(&parse_prometheus_text(second), endpoint, 6_000, rates)
    }

    fn prefix_body(hits: u64, queries: u64) -> String {
        format!(
            concat!(
                "vllm:prefix_cache_hits_total {}\n",
                "vllm:prefix_cache_queries_total {}\n",
            ),
            hits, queries
        )
    }

    #[test]
    fn engine_prefix_cache_ratio_is_token_weighted_not_a_mean_of_percentages() {
        let mut rates = RateTracker::default();
        // A busy engine at 90% and a nearly idle one at 40%. The unweighted
        // mean would be 65%; the fleet actually served 8_900/9_950 tokens.
        let busy = scrape_pair(
            &mut rates,
            "http://busy:8000",
            &prefix_body(0, 0),
            &prefix_body(9_000, 10_000),
        );
        let quiet = scrape_pair(
            &mut rates,
            "http://quiet:8000",
            &prefix_body(0, 0),
            &prefix_body(20, 50),
        );
        let (cached_tps, hit_pct) =
            engine_prefix_cache_ratio(&[busy, quiet]).expect("both engines reported");
        assert!((cached_tps - 1_804.0).abs() < 1e-6, "{cached_tps}");
        assert!((hit_pct - 89.751_243_781_094_53).abs() < 1e-9, "{hit_pct}");
    }

    #[test]
    fn engine_prefix_cache_ratio_is_absent_without_queries() {
        let mut rates = RateTracker::default();
        // Every engine idle across the interval: no queries, so no ratio. A
        // zero here would render as a stone-cold cache on a healthy fleet.
        let idle = scrape_pair(
            &mut rates,
            "http://idle:8000",
            &prefix_body(500, 1_000),
            &prefix_body(500, 1_000),
        );
        assert_eq!(engine_prefix_cache_ratio(&[idle]), None);
        assert_eq!(engine_prefix_cache_ratio(&[]), None);
        assert_eq!(engine_prefix_cache_ratio(&[EngineScrape::default()]), None);
    }

    #[test]
    fn engine_fallback_fills_cache_hit_when_responses_never_report_cached_tokens() {
        let mut rates = RateTracker::default();
        let mut histograms = HistogramWindows::default();
        let body_at = |prompt: u64, unknown: u64| {
            format!(
                concat!(
                    "ramjet_prompt_tokens_total{{endpoint=\"chat\"}} {}\n",
                    "ramjet_cached_prompt_tokens_total{{endpoint=\"chat\"}} 0\n",
                    "ramjet_cache_requests_total{{endpoint=\"chat\",outcome=\"unknown\"}} {}\n",
                ),
                prompt, unknown
            )
        };
        let map = parse_prometheus_text(&body_at(1_000, 40));
        build_serving_sample(&map, 1_000, &mut rates, &mut histograms);
        let map = parse_prometheus_text(&body_at(2_000, 80));
        let mut serving = build_serving_sample(&map, 6_000, &mut rates, &mut histograms);
        assert_eq!(serving.cache_hit_pct, None, "precondition");

        let engine = scrape_pair(
            &mut rates,
            "http://e:8000",
            &prefix_body(0, 0),
            &prefix_body(900, 1_000),
        );
        apply_engine_cache_fallback(&mut serving, &[engine]);
        assert_eq!(serving.cache_hit_pct, Some(90.0));
        assert_eq!(serving.cached_tps, Some(180.0));
        assert_eq!(
            serving.cache_hit_source,
            Some(CacheHitSource::EnginePrefixCache)
        );
    }

    #[test]
    fn engine_fallback_never_overwrites_response_usage() {
        let mut rates = RateTracker::default();
        let mut histograms = HistogramWindows::default();
        let body_at = |prompt: u64, cached: u64| {
            format!(
                concat!(
                    "ramjet_prompt_tokens_total{{endpoint=\"chat\"}} {}\n",
                    "ramjet_cached_prompt_tokens_total{{endpoint=\"chat\"}} {}\n",
                    "ramjet_cache_requests_total{{endpoint=\"chat\",outcome=\"full\"}} 10\n",
                ),
                prompt, cached
            )
        };
        let map = parse_prometheus_text(&body_at(1_000, 400));
        build_serving_sample(&map, 1_000, &mut rates, &mut histograms);
        let map = parse_prometheus_text(&body_at(2_000, 900));
        let mut serving = build_serving_sample(&map, 6_000, &mut rates, &mut histograms);
        assert_eq!(serving.cache_hit_pct, Some(50.0), "precondition");

        // An engine claiming 90% must not displace the proxy's own 50%.
        let engine = scrape_pair(
            &mut rates,
            "http://e:8000",
            &prefix_body(0, 0),
            &prefix_body(900, 1_000),
        );
        apply_engine_cache_fallback(&mut serving, &[engine]);
        assert_eq!(serving.cache_hit_pct, Some(50.0));
        assert_eq!(serving.cached_tps, Some(100.0));
        assert_eq!(
            serving.cache_hit_source,
            Some(CacheHitSource::ResponseUsage)
        );
    }

    #[test]
    fn condense_keeps_the_newest_published_cache_hit_source() {
        let serving = |source: Option<CacheHitSource>| ServingSample {
            cache_hit_pct: Some(90.0),
            cache_hit_source: source,
            ..ServingSample::default()
        };
        let samples: Vec<Sample> = [
            Some(CacheHitSource::ResponseUsage),
            Some(CacheHitSource::EnginePrefixCache),
            None,
        ]
        .into_iter()
        .enumerate()
        .map(|(index, source)| Sample {
            t: 1_000 + index as u64,
            serving: Some(serving(source)),
            ..Sample::default()
        })
        .collect();
        let condensed = merge_samples(&samples.iter().collect::<Vec<_>>());
        assert_eq!(
            condensed.serving.and_then(|s| s.cache_hit_source),
            Some(CacheHitSource::EnginePrefixCache),
        );
    }

    #[test]
    fn registry_gather_flattens_histograms() {
        let registry = Registry::new();
        let histogram = prometheus::Histogram::with_opts(
            prometheus::HistogramOpts::new("test_seconds", "test").buckets(vec![0.1, 1.0]),
        )
        .expect("histogram");
        registry
            .register(Box::new(histogram.clone()))
            .expect("register");
        histogram.observe(0.05);
        histogram.observe(0.5);
        histogram.observe(5.0);
        let map = gather_registry(&registry);
        let buckets = histogram_buckets(&map, "test_seconds");
        assert_eq!(buckets, vec![(0.1, 1.0), (1.0, 2.0), (f64::INFINITY, 3.0)]);
        assert_eq!(metric_sum(&map, "test_seconds_count"), Some(3.0));
    }

    #[test]
    fn static_paths_reject_traversal_and_fall_back_to_index() {
        assert_eq!(resolve_static_path(""), Some("index.html".to_owned()));
        assert_eq!(
            resolve_static_path("assets/app-abc123.js"),
            Some("assets/app-abc123.js".to_owned())
        );
        assert_eq!(resolve_static_path("../etc/passwd"), None);
        assert_eq!(resolve_static_path("a/../../b.js"), None);
        assert_eq!(resolve_static_path(".hidden"), None);
        assert_eq!(resolve_static_path("a\\b"), None);
        assert_eq!(
            resolve_static_path("deep/route"),
            Some("index.html".to_owned()),
            "extension-less SPA routes serve the entry point"
        );
    }

    #[test]
    fn content_types_cover_bundle_assets() {
        assert_eq!(content_type_for("index.html"), "text/html; charset=utf-8");
        assert_eq!(
            content_type_for("assets/app.js"),
            "text/javascript; charset=utf-8"
        );
        assert_eq!(
            content_type_for("assets/app.css"),
            "text/css; charset=utf-8"
        );
        assert_eq!(content_type_for("unknown.bin"), "application/octet-stream");
    }

    #[test]
    fn agent_payload_sanitizes_hostile_values() {
        let host = sanitize_host(HostSample {
            cpu_pct: Some(250.0),
            net_rx_bps: Some(-5.0),
            cpu_watts: Some(f64::NAN),
            ..HostSample::default()
        });
        assert_eq!(host.cpu_pct, Some(100.0));
        assert_eq!(host.net_rx_bps, Some(0.0));
        assert_eq!(host.cpu_watts, None);
        let host = sanitize_host(HostSample {
            iowait_pct: Some(140.0),
            io_pressure_pct: Some(-2.0),
            disk_inflight: Some(-1.0),
            disks: vec![DiskSample {
                mount: "/".repeat(200),
                total_bytes: 100.0,
                used_bytes: 40.0,
                inodes_total: Some(100.0),
                inodes_used: Some(250.0),
            }],
            ..HostSample::default()
        });
        assert_eq!(host.iowait_pct, Some(100.0));
        assert_eq!(host.io_pressure_pct, Some(0.0));
        assert_eq!(host.disk_inflight, Some(0.0));
        assert_eq!(host.disks[0].mount.len(), 128);
        assert_eq!(host.disks[0].inodes_used, Some(100.0));
        let gpus = sanitize_gpus(vec![GpuSample {
            index: 0,
            name: "x".repeat(200),
            util_pct: Some(f64::INFINITY),
            throttle_sw_power: Some(7.0),
            fan_pct: Some(-3.0),
            ..GpuSample::default()
        }]);
        assert_eq!(gpus[0].name.len(), 80);
        assert_eq!(gpus[0].util_pct, None);
        assert_eq!(gpus[0].throttle_sw_power, Some(1.0));
        assert_eq!(gpus[0].fan_pct, Some(0.0));
    }
}
