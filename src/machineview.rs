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
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use axum::{
    Router,
    body::Body,
    extract::{Query, State},
    http::{Response, StatusCode},
    routing::get,
};
use parking_lot::Mutex;
use prometheus::Registry;
use serde::{Deserialize, Serialize};
use url::Url;

const MIN_INTERVAL_MS: u64 = 1_000;
const MAX_INTERVAL_MS: u64 = 60_000;
const MIN_RETENTION_SECONDS: u64 = 60;
const MAX_RETENTION_SECONDS: u64 = 7 * 86_400;
const DEFAULT_SERIES_POINTS: usize = 400;
const MAX_SERIES_POINTS: usize = 2_000;
const PERSIST_EVERY_TICKS: u64 = 60;
const HISTOGRAM_WINDOW_MS: u64 = 120_000;
const STATE_VERSION: u32 = 1;
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
        let mode = match get("MD_MACHINEVIEW_MODE").as_deref().unwrap_or("on") {
            "on" => Mode::On,
            "off" => Mode::Off,
            value => {
                return Err(invalid(
                    "MD_MACHINEVIEW_MODE",
                    value.to_owned(),
                    "on or off",
                ));
            }
        };
        let interval_ms = bounded(
            &mut get,
            "MD_MACHINEVIEW_INTERVAL_MS",
            5_000,
            MIN_INTERVAL_MS,
            MAX_INTERVAL_MS,
        )?;
        let retention_seconds = bounded(
            &mut get,
            "MD_MACHINEVIEW_RETENTION_SECONDS",
            86_400,
            MIN_RETENTION_SECONDS,
            MAX_RETENTION_SECONDS,
        )?;
        let agent_url = match get("MD_MACHINEVIEW_AGENT_URL").filter(|value| !value.is_empty()) {
            None => None,
            Some(raw) => Some(Url::parse(&raw).ok().filter(Url::has_host).ok_or_else(|| {
                invalid("MD_MACHINEVIEW_AGENT_URL", raw, "an absolute http(s) URL")
            })?),
        };
        let state_path = get("MD_MACHINEVIEW_STATE_PATH")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from);
        let ui_dir = match get("MD_MACHINEVIEW_UI_DIR") {
            // An explicitly configured directory must exist; the default is
            // best-effort so binaries outside the container image still start.
            Some(raw) if !raw.is_empty() => {
                let path = PathBuf::from(&raw);
                if path.is_dir() {
                    Some(path)
                } else {
                    return Err(invalid(
                        "MD_MACHINEVIEW_UI_DIR",
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
    pub net_rx_bps: Option<f64>,
    pub net_tx_bps: Option<f64>,
    pub disk_read_bps: Option<f64>,
    pub disk_write_bps: Option<f64>,
    pub cpu_watts: Option<f64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub disks: Vec<DiskSample>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct DiskSample {
    pub mount: String,
    pub total_bytes: f64,
    pub used_bytes: f64,
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
    pub cache_hit_pct: Option<f64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub upstreams: Vec<UpstreamSample>,
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
                cache_hit_pct: mean(entries.iter().map(|e| e.cache_hit_pct)),
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
    let requests_per_second = metric_sum(map, "ds4proxy_requests_total")
        .and_then(|value| rates.rate("self.requests", t_ms, value));
    let prompt_tps = metric_sum(map, "ds4proxy_prompt_tokens_total")
        .and_then(|value| rates.rate("self.prompt_tokens", t_ms, value));
    // Engines that never emit `prompt_tokens_details.cached_tokens` leave
    // every cache outcome "unknown". Token-weighted hit data does not exist
    // then, and a hard 0 would misreport absence as a cold cache.
    let cache_reporting = metric_by_label(map, "ds4proxy_cache_requests_total", "outcome")
        .iter()
        .any(|(outcome, count)| outcome != "unknown" && *count > 0.0);
    let cached_tps = metric_sum(map, "ds4proxy_cached_prompt_tokens_total")
        .and_then(|value| rates.rate("self.cached_tokens", t_ms, value))
        .filter(|_| cache_reporting);
    let gen_tps = metric_sum(map, "ds4proxy_completion_tokens_total")
        .and_then(|value| rates.rate("self.completion_tokens", t_ms, value));
    let cache_hit_pct = match (prompt_tps, cached_tps) {
        (Some(prompt), Some(cached)) if prompt > 0.0 => {
            Some((cached / prompt * 100.0).clamp(0.0, 100.0))
        }
        _ => None,
    };
    let ttft_buckets = histogram_buckets(map, "ds4proxy_ttft_seconds");
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
    let tpot_buckets = histogram_buckets(map, "ds4proxy_time_per_output_token_seconds");
    let tpot_p95_ms = if tpot_buckets.is_empty() {
        None
    } else {
        histograms
            .observe_quantile("self.tpot.p95", t_ms, tpot_buckets, 0.95)
            .map(|seconds| seconds * 1_000.0)
    };
    let up_by_upstream = metric_by_label(map, "ds4proxy_upstream_up", "upstream");
    let inflight_by_upstream = metric_by_label(map, "ds4proxy_upstream_inflight", "upstream");
    let requests_by_upstream = metric_by_label(map, "ds4proxy_upstream_requests_total", "upstream");
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
        inflight: metric_sum(map, "ds4proxy_requests_inflight"),
        requests_per_second,
        prompt_tps,
        gen_tps,
        cached_tps,
        ttft_p50_ms,
        ttft_p95_ms,
        tpot_p95_ms,
        cache_hit_pct,
        upstreams,
    }
}

/// Folds one engine's scraped vLLM metrics into an engine sample.
pub fn build_engine_sample(
    map: &MetricMap,
    endpoint: &str,
    t_ms: u64,
    rates: &mut RateTracker,
) -> EngineSample {
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
    EngineSample {
        endpoint: endpoint.to_owned(),
        running: metric_sum(map, "vllm:num_requests_running"),
        waiting: metric_sum(map, "vllm:num_requests_waiting"),
        kv_cache_pct,
        gen_tps,
        prompt_tps,
        prefix_hit_pct,
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
    host.cpu_watts = finite(host.cpu_watts).map(|v| v.max(0.0));
    host.disks.retain(|disk| {
        disk.total_bytes.is_finite() && disk.used_bytes.is_finite() && disk.total_bytes >= 0.0
    });
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
}

fn persist_state(path: &Path, samples: &[Sample]) -> std::io::Result<()> {
    let state = PersistedState {
        version: STATE_VERSION,
        samples: samples.to_vec(),
    };
    let body =
        serde_json::to_vec(&state).map_err(|error| std::io::Error::other(error.to_string()))?;
    let mut temporary = path.as_os_str().to_owned();
    temporary.push(".tmp");
    let temporary = PathBuf::from(temporary);
    std::fs::write(&temporary, body)?;
    std::fs::rename(&temporary, path)
}

fn load_state(path: &Path) -> Option<Vec<Sample>> {
    let body = std::fs::read(path).ok()?;
    let state: PersistedState = serde_json::from_slice(&body).ok()?;
    (state.version == STATE_VERSION).then_some(state.samples)
}

// --- Runtime ----------------------------------------------------------------

pub struct MachineView {
    shared: Arc<Shared>,
    task: tokio::task::JoinHandle<()>,
}

struct Shared {
    settings: Settings,
    store: Store,
    upstreams: Vec<Url>,
    hostname: Option<String>,
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
        if let Some(path) = &settings.state_path
            && let Some(samples) = load_state(path)
        {
            store.restore(samples, now_unix_ms());
            tracing::info!(samples = store.len(), "machineview state restored");
        }
        let shared = Arc::new(Shared {
            settings: settings.clone(),
            store,
            upstreams: upstreams.clone(),
            hostname: read_hostname(),
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
                let sample = sampler.sample(now_unix_ms()).await;
                loop_shared.store.push(sample);
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

    /// Routes served on the metrics listener: the JSON API plus the static UI.
    ///
    /// # Panics
    ///
    /// Panics only if constructing a redirect from constant headers fails.
    pub fn router(&self) -> Router {
        let mut router = Router::new()
            .route("/api/machineview/summary", get(summary_handler))
            .route("/api/machineview/series", get(series_handler))
            .with_state(self.shared.clone());
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

    pub async fn shutdown(self) {
        let _ = tokio::time::timeout(Duration::from_secs(5), self.task).await;
    }
}

async fn persist_snapshot(shared: &Arc<Shared>) {
    let Some(path) = shared.settings.state_path.clone() else {
        return;
    };
    let samples = shared.store.snapshot();
    let result = tokio::task::spawn_blocking(move || persist_state(&path, &samples)).await;
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
    async fn sample(&mut self, t_ms: u64) -> Sample {
        let self_map = gather_registry(&self.registry);
        let serving = build_serving_sample(&self_map, t_ms, &mut self.rates, &mut self.histograms);

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
        let mut engines = Vec::with_capacity(self.upstreams.len());
        for (base, body) in self.upstreams.iter().zip(engine_bodies) {
            let endpoint = base.as_str().trim_end_matches('/').to_owned();
            if let Some(body) = body {
                let map = parse_prometheus_text(&body);
                engines.push(build_engine_sample(&map, &endpoint, t_ms, &mut self.rates));
            } else {
                engines.push(EngineSample {
                    endpoint,
                    ..EngineSample::default()
                });
            }
        }

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

        Sample {
            t: t_ms,
            host,
            gpus,
            serving: Some(serving),
            engines,
            energy,
        }
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
            (key == "MD_MACHINEVIEW_MODE").then(|| "observe".to_owned())
        });
        assert!(mode.is_err());
        let interval = Settings::from_lookup(|key| {
            (key == "MD_MACHINEVIEW_INTERVAL_MS").then(|| "10".to_owned())
        });
        assert!(interval.is_err());
        let agent = Settings::from_lookup(|key| {
            (key == "MD_MACHINEVIEW_AGENT_URL").then(|| "not a url".to_owned())
        });
        assert!(agent.is_err());
        let ui = Settings::from_lookup(|key| {
            (key == "MD_MACHINEVIEW_UI_DIR").then(|| "/definitely/not/a/real/dir".to_owned())
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
            "ds4proxy_ttft_seconds_bucket{endpoint=\"chat\",le=\"0.5\"} 3\n",
            "ds4proxy_ttft_seconds_bucket{endpoint=\"chat\",le=\"+Inf\"} 4\n",
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
        let buckets = histogram_buckets(&map, "ds4proxy_ttft_seconds");
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
        persist_state(&path, &samples).expect("persist");
        let loaded = load_state(&path).expect("load");
        assert_eq!(loaded, samples);
        let store = Store::new(10);
        store.restore(loaded, 5_000);
        assert_eq!(store.len(), 2);
        store.restore(vec![sample_at(1_000, 1.0)], 500_000);
        assert_eq!(store.len(), 0, "stale samples dropped on restore");
        std::fs::remove_dir_all(&dir).ok();
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
        let sample = build_engine_sample(&first, "http://e:8000", 1_000, &mut rates);
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
        let sample = build_engine_sample(&second, "http://e:8000", 6_000, &mut rates);
        assert_eq!(sample.gen_tps, Some(200.0));
        assert_eq!(sample.prompt_tps, Some(200.0));
        assert_eq!(sample.prefix_hit_pct, Some(75.0));
    }

    #[test]
    fn build_serving_sample_reads_registry_shapes() {
        let body = concat!(
            "ds4proxy_requests_inflight 5\n",
            "ds4proxy_requests_total{code=\"200\"} 100\n",
            "ds4proxy_prompt_tokens_total{endpoint=\"chat\"} 1000\n",
            "ds4proxy_cached_prompt_tokens_total{endpoint=\"chat\"} 400\n",
            "ds4proxy_completion_tokens_total{endpoint=\"chat\"} 300\n",
            "ds4proxy_upstream_up{upstream=\"http://a:8000\"} 1\n",
            "ds4proxy_upstream_up{upstream=\"http://b:8000\"} 0\n",
            "ds4proxy_upstream_inflight{upstream=\"http://a:8000\"} 4\n",
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
            "ds4proxy_requests_inflight 5\n",
            "ds4proxy_requests_total{code=\"200\"} 110\n",
            "ds4proxy_prompt_tokens_total{endpoint=\"chat\"} 2000\n",
            "ds4proxy_cached_prompt_tokens_total{endpoint=\"chat\"} 900\n",
            "ds4proxy_completion_tokens_total{endpoint=\"chat\"} 800\n",
            "ds4proxy_cache_requests_total{endpoint=\"chat\",outcome=\"partial\"} 5\n",
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
    fn cache_hit_is_absent_when_engines_never_report_cached_tokens() {
        let mut rates = RateTracker::default();
        let mut histograms = HistogramWindows::default();
        let body_at = |prompt: u64, unknown: u64| {
            format!(
                concat!(
                    "ds4proxy_prompt_tokens_total{{endpoint=\"chat\"}} {}\n",
                    "ds4proxy_cached_prompt_tokens_total{{endpoint=\"chat\"}} 0\n",
                    "ds4proxy_cache_requests_total{{endpoint=\"chat\",outcome=\"unknown\"}} {}\n",
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
