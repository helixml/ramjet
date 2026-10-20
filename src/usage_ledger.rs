//! Per-principal, per-model request accounting.
//!
//! Every proxied request lands in a minute bucket and an hour bucket keyed by
//! (principal, model), plus the same buckets under the reserved `*`
//! principal, and in a bounded table of recent request records. Day views are
//! folded from hour buckets at query time so a caller can align days to its
//! own midnight. Quantiles come from fixed log-spaced histograms: they merge
//! (and subtract) exactly across buckets and principals, at the cost of being
//! interpolated estimates rather than order statistics.
//!
//! The request path only touches an in-memory delta map. A flush merges the
//! deltas into `SQLite` rows every few seconds, so memory is bounded by the keys
//! active between flushes rather than by history or by the number of tokens.
//! The `*` pre-aggregate keeps fleet-wide and per-model views at one row per
//! bucket however many tokens exist; per-token views read only that token's
//! rows through the primary key.
//!
//! Nothing here sees prompt or response content.

use std::{
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    path::Path,
    sync::Arc,
    time::Duration,
};

use anyhow::Context;
use parking_lot::Mutex;
use prometheus::{CounterVec, Opts, Registry};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::usage::Accumulator;

pub const MINUTE_MS: u64 = 60_000;
pub const HOUR_MS: u64 = 3_600_000;
pub const DAY_MS: u64 = 86_400_000;
pub const DB_FILE: &str = "usage.sqlite3";
const SCHEMA_VERSION: i64 = 1;
/// Reserved principal holding the all-principals pre-aggregate. Real
/// principals are 12-hex token ids or fixed lowercase words.
pub const ALL_PRINCIPALS: &str = "*";
/// Group key for everything outside the top `limit` token groups.
pub const OTHER_GROUP: &str = "__other";
pub const DEFAULT_GROUP_LIMIT: usize = 20;
pub const MAX_GROUP_LIMIT: usize = 500;
/// Distinct model labels kept before later ones fold into `other`. Model
/// names are caller-supplied, so this bounds memory and metric cardinality.
pub const MAX_MODELS: usize = 32;
const MAX_MODEL_CHARS: usize = 128;
pub const OTHER_MODEL: &str = "other";
pub const NO_MODEL: &str = "-";
/// Bound on buckets a single series query may produce.
pub const MAX_QUERY_BUCKETS: u64 = 20_000;
pub const MAX_RECENT_LIMIT: usize = 1_000;

const TTFT_BOUNDS_MS: &[f64] = &[
    10.0, 25.0, 50.0, 75.0, 100.0, 150.0, 200.0, 300.0, 400.0, 500.0, 750.0, 1_000.0, 1_500.0,
    2_000.0, 3_000.0, 5_000.0, 7_500.0, 10_000.0, 15_000.0, 20_000.0, 30_000.0, 60_000.0,
    120_000.0, 300_000.0,
];
const DURATION_BOUNDS_MS: &[f64] = &[
    50.0,
    100.0,
    250.0,
    500.0,
    1_000.0,
    2_000.0,
    3_000.0,
    5_000.0,
    7_500.0,
    10_000.0,
    15_000.0,
    20_000.0,
    30_000.0,
    45_000.0,
    60_000.0,
    90_000.0,
    120_000.0,
    180_000.0,
    300_000.0,
    600_000.0,
    1_200_000.0,
];
const DECODE_TPS_BOUNDS: &[f64] = &[
    1.0, 2.0, 5.0, 10.0, 15.0, 20.0, 25.0, 30.0, 40.0, 50.0, 60.0, 70.0, 80.0, 90.0, 100.0, 120.0,
    150.0, 200.0, 250.0, 300.0, 400.0, 500.0,
];
const TPOT_BOUNDS_MS: &[f64] = &[
    2.0, 4.0, 6.0, 8.0, 10.0, 12.0, 15.0, 20.0, 25.0, 30.0, 40.0, 50.0, 75.0, 100.0, 150.0, 200.0,
    300.0, 500.0, 1_000.0,
];

#[derive(Clone, Debug)]
pub struct LedgerSettings {
    pub minute_retention_hours: u64,
    pub hour_retention_days: u64,
    pub recent_capacity: usize,
}

impl Default for LedgerSettings {
    fn default() -> Self {
        Self {
            minute_retention_hours: 24,
            hour_retention_days: 90,
            recent_capacity: 100_000,
        }
    }
}

/// A fixed-bound histogram. `counts[i]` holds observations `<= bounds[i]`
/// (and above the previous bound); the final slot is the overflow bucket.
/// Trailing zero slots are trimmed in storage.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
pub struct Hist {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    counts: Vec<u64>,
    #[serde(default, skip_serializing_if = "is_zero_f64")]
    sum: f64,
}

#[allow(clippy::trivially_copy_pass_by_ref)] // serde's skip_serializing_if signature.
fn is_zero_f64(value: &f64) -> bool {
    *value == 0.0
}

#[allow(clippy::trivially_copy_pass_by_ref)] // serde's skip_serializing_if signature.
fn is_zero(value: &u64) -> bool {
    *value == 0
}

impl Hist {
    fn observe(&mut self, bounds: &[f64], value: f64) {
        if !value.is_finite() || value < 0.0 {
            return;
        }
        let slot = bounds
            .iter()
            .position(|bound| value <= *bound)
            .unwrap_or(bounds.len());
        if self.counts.len() <= slot {
            self.counts.resize(slot + 1, 0);
        }
        self.counts[slot] += 1;
        self.sum += value;
    }

    fn merge(&mut self, other: &Self) {
        if self.counts.len() < other.counts.len() {
            self.counts.resize(other.counts.len(), 0);
        }
        for (slot, count) in other.counts.iter().enumerate() {
            self.counts[slot] += count;
        }
        self.sum += other.sum;
    }

    fn subtract(&mut self, other: &Self) {
        for (slot, count) in other.counts.iter().enumerate() {
            if let Some(mine) = self.counts.get_mut(slot) {
                *mine = mine.saturating_sub(*count);
            }
        }
        self.sum = (self.sum - other.sum).max(0.0);
        while self.counts.last() == Some(&0) {
            self.counts.pop();
        }
    }

    fn count(&self) -> u64 {
        self.counts.iter().sum()
    }

    #[allow(clippy::cast_precision_loss)]
    fn mean(&self) -> Option<f64> {
        let count = self.count();
        (count > 0).then(|| self.sum / count as f64)
    }

    /// Linear interpolation inside the bucket holding the quantile. The
    /// overflow bucket reports its lower bound: an honest floor, not a guess.
    #[allow(clippy::cast_precision_loss)]
    fn quantile(&self, bounds: &[f64], quantile: f64) -> Option<f64> {
        let total = self.count();
        if total == 0 {
            return None;
        }
        let target = (quantile.clamp(0.0, 1.0) * total as f64).max(1.0);
        let mut cumulative = 0.0;
        for (slot, count) in self.counts.iter().enumerate() {
            let count = *count as f64;
            if count == 0.0 {
                continue;
            }
            if cumulative + count >= target {
                let lower = if slot == 0 { 0.0 } else { bounds[slot - 1] };
                let Some(&upper) = bounds.get(slot) else {
                    return Some(lower);
                };
                let fraction = ((target - cumulative) / count).clamp(0.0, 1.0);
                return Some(lower + (upper - lower) * fraction);
            }
            cumulative += count;
        }
        bounds.last().copied()
    }
}

/// One bucket's aggregate for one (principal, model).
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
pub struct Agg {
    #[serde(default, skip_serializing_if = "is_zero")]
    requests: u64,
    #[serde(default, skip_serializing_if = "is_zero")]
    ok: u64,
    #[serde(default, skip_serializing_if = "is_zero")]
    client_errors: u64,
    #[serde(default, skip_serializing_if = "is_zero")]
    server_errors: u64,
    #[serde(default, skip_serializing_if = "is_zero")]
    disconnects: u64,
    #[serde(default, skip_serializing_if = "is_zero")]
    streamed: u64,
    #[serde(default, skip_serializing_if = "is_zero")]
    prompt_tokens: u64,
    #[serde(default, skip_serializing_if = "is_zero")]
    cached_tokens: u64,
    #[serde(default, skip_serializing_if = "is_zero")]
    completion_tokens: u64,
    #[serde(default, skip_serializing_if = "is_zero")]
    cache_full: u64,
    #[serde(default, skip_serializing_if = "is_zero")]
    cache_partial: u64,
    #[serde(default, skip_serializing_if = "is_zero")]
    cache_cold: u64,
    #[serde(default, skip_serializing_if = "is_zero")]
    cache_unknown: u64,
    #[serde(default)]
    ttft: Hist,
    #[serde(default)]
    duration: Hist,
    #[serde(default)]
    decode_tps: Hist,
    #[serde(default)]
    tpot: Hist,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    upstream_requests: Vec<u64>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    finish_reasons: BTreeMap<String, u64>,
}

impl Agg {
    fn merge(&mut self, other: &Self) {
        self.requests += other.requests;
        self.ok += other.ok;
        self.client_errors += other.client_errors;
        self.server_errors += other.server_errors;
        self.disconnects += other.disconnects;
        self.streamed += other.streamed;
        self.prompt_tokens += other.prompt_tokens;
        self.cached_tokens += other.cached_tokens;
        self.completion_tokens += other.completion_tokens;
        self.cache_full += other.cache_full;
        self.cache_partial += other.cache_partial;
        self.cache_cold += other.cache_cold;
        self.cache_unknown += other.cache_unknown;
        self.ttft.merge(&other.ttft);
        self.duration.merge(&other.duration);
        self.decode_tps.merge(&other.decode_tps);
        self.tpot.merge(&other.tpot);
        if self.upstream_requests.len() < other.upstream_requests.len() {
            self.upstream_requests
                .resize(other.upstream_requests.len(), 0);
        }
        for (index, count) in other.upstream_requests.iter().enumerate() {
            self.upstream_requests[index] += count;
        }
        for (reason, count) in &other.finish_reasons {
            *self.finish_reasons.entry(reason.clone()).or_default() += count;
        }
    }

    /// Removes `other` from `self`. Every field is a count or a sum, so for
    /// `other` contained in `self` this is exact; that is what makes the
    /// "everything but the top groups" remainder exact.
    fn subtract(&mut self, other: &Self) {
        self.requests = self.requests.saturating_sub(other.requests);
        self.ok = self.ok.saturating_sub(other.ok);
        self.client_errors = self.client_errors.saturating_sub(other.client_errors);
        self.server_errors = self.server_errors.saturating_sub(other.server_errors);
        self.disconnects = self.disconnects.saturating_sub(other.disconnects);
        self.streamed = self.streamed.saturating_sub(other.streamed);
        self.prompt_tokens = self.prompt_tokens.saturating_sub(other.prompt_tokens);
        self.cached_tokens = self.cached_tokens.saturating_sub(other.cached_tokens);
        self.completion_tokens = self
            .completion_tokens
            .saturating_sub(other.completion_tokens);
        self.cache_full = self.cache_full.saturating_sub(other.cache_full);
        self.cache_partial = self.cache_partial.saturating_sub(other.cache_partial);
        self.cache_cold = self.cache_cold.saturating_sub(other.cache_cold);
        self.cache_unknown = self.cache_unknown.saturating_sub(other.cache_unknown);
        self.ttft.subtract(&other.ttft);
        self.duration.subtract(&other.duration);
        self.decode_tps.subtract(&other.decode_tps);
        self.tpot.subtract(&other.tpot);
        for (index, count) in other.upstream_requests.iter().enumerate() {
            if let Some(mine) = self.upstream_requests.get_mut(index) {
                *mine = mine.saturating_sub(*count);
            }
        }
        for (reason, count) in &other.finish_reasons {
            if let Some(mine) = self.finish_reasons.get_mut(reason) {
                *mine = mine.saturating_sub(*count);
            }
        }
        self.finish_reasons.retain(|_, count| *count > 0);
    }

    fn encode(&self) -> anyhow::Result<Vec<u8>> {
        // Named encoding: zero fields are skipped, so positional msgpack
        // would misalign them.
        rmp_serde::to_vec_named(self).context("encode usage aggregate")
    }

    fn decode(bytes: &[u8]) -> Option<Self> {
        rmp_serde::from_slice(bytes).ok()
    }

    fn observe(&mut self, record: &RequestRecord) {
        self.requests += 1;
        if record.result == "upstream_read_error" || record.status >= 500 {
            self.server_errors += 1;
        } else if record.status >= 400 {
            self.client_errors += 1;
        } else {
            self.ok += 1;
        }
        if record.result == "client_disconnect" {
            self.disconnects += 1;
        }
        if record.stream {
            self.streamed += 1;
        }
        self.prompt_tokens += record.prompt_tokens.unwrap_or(0);
        self.cached_tokens += record.cached_tokens.unwrap_or(0);
        self.completion_tokens += record.completion_tokens.unwrap_or(0);
        match record.cache_outcome.as_deref() {
            Some("full") => self.cache_full += 1,
            Some("partial") => self.cache_partial += 1,
            Some("cold") => self.cache_cold += 1,
            Some(_) => self.cache_unknown += 1,
            None => {}
        }
        if let Some(ttft) = record.ttft_ms {
            self.ttft.observe(TTFT_BOUNDS_MS, ttft);
        }
        self.duration
            .observe(DURATION_BOUNDS_MS, record.duration_ms);
        if let Some(tps) = record.decode_tps {
            self.decode_tps.observe(DECODE_TPS_BOUNDS, tps);
        }
        if let Some(tpot) = record.tpot_ms {
            self.tpot.observe(TPOT_BOUNDS_MS, tpot);
        }
        if let Some(upstream) = record.upstream {
            if self.upstream_requests.len() <= upstream {
                self.upstream_requests.resize(upstream + 1, 0);
            }
            self.upstream_requests[upstream] += 1;
        }
        if let Some(reason) = &record.finish_reason {
            *self.finish_reasons.entry(reason.clone()).or_default() += 1;
        }
    }

    #[must_use]
    pub fn stats(&self) -> Stats {
        #[allow(clippy::cast_precision_loss)]
        let cache_hit_ratio = (self.prompt_tokens > 0)
            .then(|| (self.cached_tokens as f64 / self.prompt_tokens as f64).min(1.0));
        Stats {
            requests: self.requests,
            ok: self.ok,
            client_errors: self.client_errors,
            server_errors: self.server_errors,
            disconnects: self.disconnects,
            streamed: self.streamed,
            prompt_tokens: self.prompt_tokens,
            cached_tokens: self.cached_tokens,
            completion_tokens: self.completion_tokens,
            cache_hit_ratio,
            cache_full: self.cache_full,
            cache_partial: self.cache_partial,
            cache_cold: self.cache_cold,
            cache_unknown: self.cache_unknown,
            ttft_p50_ms: self.ttft.quantile(TTFT_BOUNDS_MS, 0.5),
            ttft_p90_ms: self.ttft.quantile(TTFT_BOUNDS_MS, 0.9),
            ttft_p99_ms: self.ttft.quantile(TTFT_BOUNDS_MS, 0.99),
            ttft_mean_ms: self.ttft.mean(),
            duration_p50_ms: self.duration.quantile(DURATION_BOUNDS_MS, 0.5),
            duration_p90_ms: self.duration.quantile(DURATION_BOUNDS_MS, 0.9),
            duration_p99_ms: self.duration.quantile(DURATION_BOUNDS_MS, 0.99),
            duration_mean_ms: self.duration.mean(),
            decode_tps_p10: self.decode_tps.quantile(DECODE_TPS_BOUNDS, 0.1),
            decode_tps_p50: self.decode_tps.quantile(DECODE_TPS_BOUNDS, 0.5),
            decode_tps_p90: self.decode_tps.quantile(DECODE_TPS_BOUNDS, 0.9),
            decode_tps_mean: self.decode_tps.mean(),
            tpot_p50_ms: self.tpot.quantile(TPOT_BOUNDS_MS, 0.5),
            tpot_p90_ms: self.tpot.quantile(TPOT_BOUNDS_MS, 0.9),
            upstream_requests: self.upstream_requests.clone(),
            finish_reasons: self.finish_reasons.clone(),
        }
    }
}

/// Derived, display-ready view of an [`Agg`]. Every field is always present;
/// ratios and quantiles are `null` when there is nothing to estimate from.
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct Stats {
    pub requests: u64,
    pub ok: u64,
    pub client_errors: u64,
    pub server_errors: u64,
    pub disconnects: u64,
    pub streamed: u64,
    pub prompt_tokens: u64,
    pub cached_tokens: u64,
    pub completion_tokens: u64,
    pub cache_hit_ratio: Option<f64>,
    pub cache_full: u64,
    pub cache_partial: u64,
    pub cache_cold: u64,
    pub cache_unknown: u64,
    pub ttft_p50_ms: Option<f64>,
    pub ttft_p90_ms: Option<f64>,
    pub ttft_p99_ms: Option<f64>,
    pub ttft_mean_ms: Option<f64>,
    pub duration_p50_ms: Option<f64>,
    pub duration_p90_ms: Option<f64>,
    pub duration_p99_ms: Option<f64>,
    pub duration_mean_ms: Option<f64>,
    pub decode_tps_p10: Option<f64>,
    pub decode_tps_p50: Option<f64>,
    pub decode_tps_p90: Option<f64>,
    pub decode_tps_mean: Option<f64>,
    pub tpot_p50_ms: Option<f64>,
    pub tpot_p90_ms: Option<f64>,
    pub upstream_requests: Vec<u64>,
    pub finish_reasons: BTreeMap<String, u64>,
}

/// One completed (or refused) request. Bounded, content-free metadata only.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct RequestRecord {
    pub t_ms: u64,
    pub principal: String,
    pub model: String,
    pub endpoint: String,
    pub upstream: Option<usize>,
    pub status: u16,
    pub stream: bool,
    pub result: String,
    pub prompt_tokens: Option<u64>,
    pub cached_tokens: Option<u64>,
    pub completion_tokens: Option<u64>,
    pub cache_outcome: Option<String>,
    pub ttft_ms: Option<f64>,
    pub duration_ms: f64,
    pub decode_tps: Option<f64>,
    #[serde(default)]
    pub tpot_ms: Option<f64>,
    pub finish_reason: Option<String>,
    pub route_outcome: Option<String>,
    pub request_bytes: u64,
    pub response_bytes: u64,
}

/// What the proxy knows about a request once it has finished. The ledger
/// derives the record, aggregates, and metrics from it.
pub struct Completion<'a> {
    pub principal: &'a str,
    pub model: Option<&'a str>,
    pub endpoint: &'a str,
    pub upstream: Option<usize>,
    pub status: u16,
    pub stream: bool,
    pub result: &'a str,
    pub usage: Option<&'a Accumulator>,
    pub first_token: Option<Duration>,
    pub elapsed: Duration,
    pub route_outcome: Option<&'a str>,
    pub request_bytes: usize,
    pub response_bytes: usize,
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn token_count(value: Option<f64>) -> Option<u64> {
    value
        .filter(|value| value.is_finite() && *value >= 0.0)
        .map(|value| value as u64)
}

impl Completion<'_> {
    fn into_record(self, t_ms: u64, model: String) -> RequestRecord {
        let usage = self.usage;
        let completion = usage.and_then(|usage| token_count(usage.completion));
        let decode = self
            .first_token
            .map_or(self.elapsed, |first| self.elapsed.saturating_sub(first));
        #[allow(clippy::cast_precision_loss)]
        let (decode_tps, tpot_ms) = match completion {
            // Same admission rule as `ramjet_decode_tps`: short or tiny
            // completions measure scheduling noise, not decode speed.
            Some(tokens) if decode > Duration::from_millis(500) && tokens > 8 => (
                Some(tokens as f64 / decode.as_secs_f64()),
                Some(decode.as_secs_f64() * 1_000.0 / (tokens as f64 - 1.0).max(1.0)),
            ),
            _ => (None, None),
        };
        let measured = usage.filter(|usage| usage.prompt.is_some() || usage.completion.is_some());
        RequestRecord {
            t_ms,
            principal: self.principal.to_owned(),
            model,
            endpoint: self.endpoint.to_owned(),
            upstream: self.upstream,
            status: self.status,
            stream: self.stream,
            result: self.result.to_owned(),
            prompt_tokens: usage.and_then(|usage| token_count(usage.prompt)),
            cached_tokens: usage.and_then(|usage| token_count(usage.cached)),
            completion_tokens: completion,
            cache_outcome: measured.map(|usage| usage.cache_outcome().to_owned()),
            ttft_ms: self
                .first_token
                .filter(|_| self.stream)
                .map(|ttft| ttft.as_secs_f64() * 1_000.0),
            duration_ms: self.elapsed.as_secs_f64() * 1_000.0,
            decode_tps,
            tpot_ms,
            finish_reason: measured
                .filter(|usage| !usage.finish_reason.is_empty())
                .map(|usage| crate::shims::finish_reason(&usage.finish_reason).to_owned()),
            route_outcome: self.route_outcome.map(ToOwned::to_owned),
            request_bytes: self.request_bytes as u64,
            response_bytes: self.response_bytes as u64,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Tier {
    Minute,
    Hour,
}

impl Tier {
    const fn width(self) -> u64 {
        match self {
            Self::Minute => MINUTE_MS,
            Self::Hour => HOUR_MS,
        }
    }

    const fn code(self) -> i64 {
        match self {
            Self::Minute => 0,
            Self::Hour => 1,
        }
    }
}

/// (tier, bucket start, principal, model).
type Key = (Tier, u64, Arc<str>, Arc<str>);

/// Everything recorded since the last flush.
#[derive(Default)]
struct Pending {
    buckets: HashMap<Key, Agg>,
    requests: VecDeque<RequestRecord>,
}

struct State {
    pending: Pending,
    principals: HashSet<Arc<str>>,
    models: HashSet<Arc<str>>,
}

impl State {
    fn intern(set: &mut HashSet<Arc<str>>, value: &str) -> Arc<str> {
        if let Some(existing) = set.get(value) {
            return existing.clone();
        }
        let value: Arc<str> = Arc::from(value);
        set.insert(value.clone());
        value
    }

    /// Bounds caller-supplied model names before they become keys.
    fn model_label(&mut self, model: Option<&str>) -> Arc<str> {
        let model = model.map(str::trim).filter(|model| !model.is_empty());
        let Some(model) = model else {
            return Self::intern(&mut self.models, NO_MODEL);
        };
        let model = model
            .chars()
            .filter(|character| !character.is_control())
            .take(MAX_MODEL_CHARS)
            .collect::<String>();
        if self.models.contains(model.as_str()) {
            return Self::intern(&mut self.models, &model);
        }
        // `-` and `other` do not count against the cap.
        let named = self
            .models
            .iter()
            .filter(|known| !matches!(known.as_ref(), NO_MODEL | OTHER_MODEL))
            .count();
        if named >= MAX_MODELS {
            Self::intern(&mut self.models, OTHER_MODEL)
        } else {
            Self::intern(&mut self.models, &model)
        }
    }
}

struct LedgerMetrics {
    requests: CounterVec,
    tokens: CounterVec,
}

pub struct UsageLedger {
    settings: LedgerSettings,
    state: Mutex<State>,
    metrics: Option<LedgerMetrics>,
    /// Lock order is always `db` then `state`. A flush holds `db` while it
    /// moves deltas out of `state`, so a query never observes deltas that
    /// have left memory but not yet reached the database.
    db: Mutex<Connection>,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum Granularity {
    Minute,
    #[default]
    Hour,
    Day,
}

impl Granularity {
    #[must_use]
    pub const fn bucket_ms(self) -> u64 {
        match self {
            Self::Minute => MINUTE_MS,
            Self::Hour => HOUR_MS,
            Self::Day => DAY_MS,
        }
    }

    #[must_use]
    pub const fn default_range_ms(self) -> u64 {
        match self {
            Self::Minute => HOUR_MS,
            Self::Hour => DAY_MS,
            Self::Day => 30 * DAY_MS,
        }
    }

    const fn tier(self) -> Tier {
        match self {
            Self::Minute => Tier::Minute,
            Self::Hour | Self::Day => Tier::Hour,
        }
    }
}

impl Serialize for Granularity {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(match self {
            Self::Minute => "minute",
            Self::Hour => "hour",
            Self::Day => "day",
        })
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum GroupBy {
    #[default]
    Token,
    Model,
    None,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct SeriesQuery {
    #[serde(default)]
    pub granularity: Granularity,
    pub from_ms: Option<u64>,
    pub to_ms: Option<u64>,
    #[serde(default)]
    pub group_by: GroupBy,
    pub token: Option<String>,
    pub model: Option<String>,
    /// Minutes east of UTC; aligns day buckets to the caller's midnight.
    #[serde(default)]
    pub tz_offset_min: i32,
    /// Token groups returned individually; the rest fold into `__other`.
    pub limit: Option<usize>,
}

#[derive(Debug, Serialize)]
pub struct SeriesPoint {
    pub t: u64,
    #[serde(flatten)]
    pub stats: Stats,
}

#[derive(Debug, Serialize)]
pub struct SeriesGroup {
    pub key: String,
    pub points: Vec<SeriesPoint>,
}

#[derive(Debug, Serialize)]
pub struct GroupTotal {
    pub key: String,
    #[serde(flatten)]
    pub stats: Stats,
}

#[derive(Debug, Serialize)]
pub struct SeriesResponse {
    pub granularity: Granularity,
    pub bucket_ms: u64,
    pub from_ms: u64,
    pub to_ms: u64,
    pub group_by: GroupBy,
    /// Distinct groups with traffic in range, before the `limit` fold.
    pub group_count: usize,
    pub series: Vec<SeriesGroup>,
    pub totals: Vec<GroupTotal>,
    pub overall: Stats,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct RecentQuery {
    pub limit: Option<usize>,
    pub token: Option<String>,
    pub model: Option<String>,
    #[serde(default)]
    pub errors_only: bool,
    pub before_ms: Option<u64>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("{0}")]
pub struct QueryError(pub String);

impl From<rusqlite::Error> for QueryError {
    fn from(error: rusqlite::Error) -> Self {
        tracing::warn!(%error, "usage query failed");
        Self("usage store query failed".to_owned())
    }
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS usage (
    tier INTEGER NOT NULL,
    principal TEXT NOT NULL,
    t INTEGER NOT NULL,
    model TEXT NOT NULL,
    requests INTEGER NOT NULL,
    agg BLOB NOT NULL,
    PRIMARY KEY (tier, principal, t, model)
) WITHOUT ROWID;
-- Covering: ranking principals over a range never touches the row blobs.
CREATE INDEX IF NOT EXISTS usage_by_time ON usage (tier, t, principal, model, requests);
CREATE TABLE IF NOT EXISTS requests (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    t INTEGER NOT NULL,
    principal TEXT NOT NULL,
    model TEXT NOT NULL,
    ok INTEGER NOT NULL,
    record BLOB NOT NULL
);
CREATE INDEX IF NOT EXISTS requests_by_principal ON requests (principal, id);
CREATE TABLE IF NOT EXISTS schema_meta (key TEXT PRIMARY KEY, value INTEGER NOT NULL);
";

fn sql_int(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

fn from_sql_int(value: i64) -> u64 {
    u64::try_from(value).unwrap_or(0)
}

fn record_ok(record: &RequestRecord) -> bool {
    record.status < 400 && record.result == "complete"
}

/// Rows of one principal (or of the `*` pre-aggregate) in a time range.
fn fetch_rows(
    db: &Connection,
    tier: Tier,
    principal: &str,
    from: u64,
    to: u64,
    model: Option<&str>,
) -> Result<Vec<(u64, String, Agg)>, QueryError> {
    let mut statement = db.prepare_cached(
        "SELECT t, model, agg FROM usage
         WHERE tier = ?1 AND principal = ?2 AND t >= ?3 AND t < ?4
           AND (?5 IS NULL OR model = ?5)",
    )?;
    let rows = statement.query_map(
        params![tier.code(), principal, sql_int(from), sql_int(to), model],
        |row| {
            Ok((
                from_sql_int(row.get(0)?),
                row.get::<_, String>(1)?,
                row.get::<_, Vec<u8>>(2)?,
            ))
        },
    )?;
    let mut out = Vec::new();
    for row in rows {
        let (t, model, blob) = row?;
        if let Some(agg) = Agg::decode(&blob) {
            out.push((t, model, agg));
        }
    }
    Ok(out)
}

impl UsageLedger {
    /// Opens the ledger. `state_dir` selects `usage.sqlite3` there (created
    /// mode 0600); without it the store is in memory. `registry` adds
    /// per-principal Prometheus counters.
    ///
    /// # Errors
    ///
    /// Database open/schema failure or metric registration failure.
    pub fn new(
        settings: LedgerSettings,
        registry: Option<&Registry>,
        state_dir: Option<&Path>,
    ) -> anyhow::Result<Self> {
        let db = match state_dir {
            Some(dir) => {
                let path = dir.join(DB_FILE);
                let db =
                    Connection::open(&path).with_context(|| format!("open {}", path.display()))?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
                        .with_context(|| format!("restrict {}", path.display()))?;
                }
                db.pragma_update(None, "journal_mode", "WAL")?;
                db.pragma_update(None, "synchronous", "NORMAL")?;
                db
            }
            None => Connection::open_in_memory().context("open in-memory usage store")?,
        };
        db.execute_batch(SCHEMA).context("create usage schema")?;
        let version: Option<i64> = db
            .query_row(
                "SELECT value FROM schema_meta WHERE key = 'version'",
                [],
                |row| row.get(0),
            )
            .optional()?;
        match version {
            None => {
                db.execute(
                    "INSERT INTO schema_meta (key, value) VALUES ('version', ?1)",
                    [SCHEMA_VERSION],
                )?;
            }
            Some(SCHEMA_VERSION) => {}
            Some(other) => anyhow::bail!("unsupported usage store schema version {other}"),
        }
        let mut models = HashSet::new();
        {
            let mut statement =
                db.prepare("SELECT DISTINCT model FROM usage WHERE tier = 1 AND principal = ?1")?;
            for model in statement.query_map([ALL_PRINCIPALS], |row| row.get::<_, String>(0))? {
                models.insert(Arc::from(model?.as_str()));
            }
        }
        // Registered last so a failed open can be retried without a duplicate
        // registration.
        let metrics = registry.map(register_metrics).transpose()?;
        Ok(Self {
            settings,
            state: Mutex::new(State {
                pending: Pending::default(),
                principals: HashSet::new(),
                models,
            }),
            metrics,
            db: Mutex::new(db),
        })
    }

    #[must_use]
    pub fn settings(&self) -> &LedgerSettings {
        &self.settings
    }

    /// Records one finished request at wall-clock time `t_ms`. Touches only
    /// memory; [`Self::flush`] makes it durable.
    pub fn record_at(&self, t_ms: u64, completion: Completion<'_>) {
        let principal = completion.principal;
        let model_hint = completion.model;
        let mut state = self.state.lock();
        let principal = State::intern(&mut state.principals, principal);
        let all = State::intern(&mut state.principals, ALL_PRINCIPALS);
        let model = state.model_label(model_hint);
        let record = completion.into_record(t_ms, model.to_string());
        for tier in [Tier::Minute, Tier::Hour] {
            let start = t_ms - t_ms % tier.width();
            for owner in [&principal, &all] {
                state
                    .pending
                    .buckets
                    .entry((tier, start, owner.clone(), model.clone()))
                    .or_default()
                    .observe(&record);
            }
        }
        let capacity = self.settings.recent_capacity;
        if capacity > 0 {
            if state.pending.requests.len() >= capacity {
                state.pending.requests.pop_front();
            }
            state.pending.requests.push_back(record.clone());
        }
        drop(state);
        if let Some(metrics) = &self.metrics {
            metrics.observe(&principal, &model, &record);
        }
    }

    /// Records one finished request now.
    pub fn record(&self, completion: Completion<'_>) {
        self.record_at(unix_ms(), completion);
    }

    /// Merges pending deltas into the store and applies retention. On failure
    /// the deltas return to memory for the next attempt.
    ///
    /// # Errors
    ///
    /// Database write failure.
    pub fn flush(&self, now_ms: u64) -> anyhow::Result<()> {
        let mut db = self.db.lock();
        let pending = std::mem::take(&mut self.state.lock().pending);
        if let Err(error) = self.write(&mut db, &pending, now_ms) {
            let mut state = self.state.lock();
            for (key, delta) in pending.buckets {
                state.pending.buckets.entry(key).or_default().merge(&delta);
            }
            let newer = std::mem::take(&mut state.pending.requests);
            state.pending.requests = pending.requests;
            state.pending.requests.extend(newer);
            let excess = state
                .pending
                .requests
                .len()
                .saturating_sub(self.settings.recent_capacity);
            state.pending.requests.drain(..excess);
            return Err(error);
        }
        Ok(())
    }

    fn write(&self, db: &mut Connection, pending: &Pending, now_ms: u64) -> anyhow::Result<()> {
        let transaction = db.transaction()?;
        {
            let mut select = transaction.prepare_cached(
                "SELECT agg FROM usage WHERE tier = ?1 AND principal = ?2 AND t = ?3 AND model = ?4",
            )?;
            let mut upsert = transaction.prepare_cached(
                "INSERT INTO usage (tier, principal, t, model, requests, agg)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT (tier, principal, t, model)
                 DO UPDATE SET requests = excluded.requests, agg = excluded.agg",
            )?;
            for ((tier, t, principal, model), delta) in &pending.buckets {
                let key = params![tier.code(), principal.as_ref(), sql_int(*t), model.as_ref()];
                let mut agg = select
                    .query_row(key, |row| row.get::<_, Vec<u8>>(0))
                    .optional()?
                    .and_then(|blob| Agg::decode(&blob))
                    .unwrap_or_default();
                agg.merge(delta);
                upsert.execute(params![
                    tier.code(),
                    principal.as_ref(),
                    sql_int(*t),
                    model.as_ref(),
                    sql_int(agg.requests),
                    agg.encode()?,
                ])?;
            }
            let mut insert = transaction.prepare_cached(
                "INSERT INTO requests (t, principal, model, ok, record) VALUES (?1, ?2, ?3, ?4, ?5)",
            )?;
            for record in &pending.requests {
                insert.execute(params![
                    sql_int(record.t_ms),
                    record.principal,
                    record.model,
                    record_ok(record),
                    rmp_serde::to_vec_named(record).context("encode request record")?,
                ])?;
            }
        }
        let minute_cutoff = now_ms.saturating_sub(self.settings.minute_retention_hours * HOUR_MS);
        let hour_cutoff = now_ms.saturating_sub(self.settings.hour_retention_days * DAY_MS);
        transaction.execute(
            "DELETE FROM usage WHERE tier = 0 AND t < ?1",
            [sql_int(minute_cutoff)],
        )?;
        transaction.execute(
            "DELETE FROM usage WHERE tier = 1 AND t < ?1",
            [sql_int(hour_cutoff)],
        )?;
        transaction.execute(
            "DELETE FROM requests WHERE id <= (SELECT MAX(id) FROM requests) - ?1",
            [sql_int(self.settings.recent_capacity as u64)],
        )?;
        transaction.commit()?;
        Ok(())
    }

    /// Flushes off the async workers and logs failures: accounting must never
    /// block or fail serving.
    pub async fn flush_async(self: &Arc<Self>) {
        let ledger = Arc::clone(self);
        match tokio::task::spawn_blocking(move || ledger.flush(unix_ms())).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => tracing::warn!(error = %format!("{error:#}"), "usage flush failed"),
            Err(error) => tracing::warn!(%error, "usage flush task failed"),
        }
    }

    /// Every principal key with usage in retention.
    ///
    /// # Errors
    ///
    /// Database read failure.
    pub fn principals(&self) -> Result<Vec<String>, QueryError> {
        let db = self.db.lock();
        let mut keys = HashSet::new();
        {
            let state = self.state.lock();
            for (_, _, principal, _) in state.pending.buckets.keys() {
                keys.insert(principal.to_string());
            }
        }
        let mut statement =
            db.prepare_cached("SELECT DISTINCT principal FROM usage WHERE tier = 1")?;
        for key in statement.query_map([], |row| row.get::<_, String>(0))? {
            keys.insert(key?);
        }
        keys.remove(ALL_PRINCIPALS);
        let mut keys = keys.into_iter().collect::<Vec<_>>();
        keys.sort();
        Ok(keys)
    }

    #[must_use]
    pub fn models(&self) -> Vec<String> {
        let mut models = self
            .state
            .lock()
            .models
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        models.sort();
        models
    }

    /// Aggregates a time range.
    ///
    /// # Errors
    ///
    /// Rejects inverted ranges or an unbounded bucket count; reports store
    /// read failures.
    #[allow(clippy::too_many_lines)]
    pub fn series(&self, query: &SeriesQuery, now_ms: u64) -> Result<SeriesResponse, QueryError> {
        let width = query.granularity.bucket_ms();
        let offset_ms = i64::from(query.tz_offset_min.clamp(-14 * 60, 14 * 60)) * 60_000;
        let to_ms = query.to_ms.unwrap_or(now_ms);
        let from_ms = query
            .from_ms
            .unwrap_or_else(|| to_ms.saturating_sub(query.granularity.default_range_ms()));
        if from_ms >= to_ms {
            return Err(QueryError("from_ms must be before to_ms".to_owned()));
        }
        if (to_ms - from_ms) / width > MAX_QUERY_BUCKETS {
            return Err(QueryError(format!(
                "range spans more than {MAX_QUERY_BUCKETS} buckets"
            )));
        }
        let limit = query
            .limit
            .unwrap_or(DEFAULT_GROUP_LIMIT)
            .clamp(1, MAX_GROUP_LIMIT);
        let align = |t: u64| -> u64 {
            match query.granularity {
                Granularity::Day => align_day(t, offset_ms),
                _ => t - t % width,
            }
        };
        let tier = query.granularity.tier();
        let scan_from = from_ms - from_ms % tier.width();
        let model = query.model.as_deref();

        let db = self.db.lock();
        // Deltas not yet flushed, restricted to this tier, range, and model.
        let pending = {
            let state = self.state.lock();
            state
                .pending
                .buckets
                .iter()
                .filter(|((row_tier, t, _, row_model), _)| {
                    *row_tier == tier
                        && (scan_from..to_ms).contains(t)
                        && model.is_none_or(|wanted| wanted == row_model.as_ref())
                })
                .map(|((_, t, principal, row_model), agg)| {
                    (
                        *t,
                        principal.to_string(),
                        row_model.to_string(),
                        agg.clone(),
                    )
                })
                .collect::<Vec<_>>()
        };
        let rows_for = |principal: &str| -> Result<Vec<(u64, String, Agg)>, QueryError> {
            let mut rows = fetch_rows(&db, tier, principal, scan_from, to_ms, model)?;
            rows.extend(
                pending
                    .iter()
                    .filter(|(_, owner, _, _)| owner == principal)
                    .map(|(t, _, row_model, agg)| (*t, row_model.clone(), agg.clone())),
            );
            Ok(rows)
        };

        let mut points: BTreeMap<(String, u64), Agg> = BTreeMap::new();
        let mut totals: BTreeMap<String, Agg> = BTreeMap::new();
        let mut overall = Agg::default();
        let mut add = |group: &str, t: u64, agg: &Agg| {
            points
                .entry((group.to_owned(), align(t)))
                .or_default()
                .merge(agg);
            totals.entry(group.to_owned()).or_default().merge(agg);
        };
        let group_count;
        match (query.token.as_deref(), query.group_by) {
            (Some(principal), group_by) => {
                for (t, row_model, agg) in rows_for(principal)? {
                    let group = match group_by {
                        GroupBy::Token => principal,
                        GroupBy::Model => row_model.as_str(),
                        GroupBy::None => "all",
                    };
                    add(group, t, &agg);
                    overall.merge(&agg);
                }
                group_count = totals.len();
            }
            (None, GroupBy::Model | GroupBy::None) => {
                for (t, row_model, agg) in rows_for(ALL_PRINCIPALS)? {
                    let group = if query.group_by == GroupBy::Model {
                        row_model.as_str()
                    } else {
                        "all"
                    };
                    add(group, t, &agg);
                    overall.merge(&agg);
                }
                group_count = totals.len();
            }
            (None, GroupBy::Token) => {
                // Rank on the cheap `requests` column, then read full rows
                // only for the winners. The remainder is total minus winners.
                let mut ranking: HashMap<String, u64> = HashMap::new();
                {
                    let mut statement = db.prepare_cached(
                        "SELECT principal, SUM(requests) FROM usage
                         WHERE tier = ?1 AND t >= ?2 AND t < ?3 AND principal != ?4
                           AND (?5 IS NULL OR model = ?5)
                         GROUP BY principal",
                    )?;
                    let rows = statement.query_map(
                        params![
                            tier.code(),
                            sql_int(scan_from),
                            sql_int(to_ms),
                            ALL_PRINCIPALS,
                            model
                        ],
                        |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
                    )?;
                    for row in rows {
                        let (principal, requests) = row?;
                        *ranking.entry(principal).or_default() += from_sql_int(requests);
                    }
                }
                for (_, principal, _, agg) in &pending {
                    if principal != ALL_PRINCIPALS {
                        *ranking.entry(principal.clone()).or_default() += agg.requests;
                    }
                }
                let mut ranked = ranking.into_iter().collect::<Vec<_>>();
                ranked
                    .sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
                group_count = ranked.len();
                let mut winners: BTreeMap<u64, Agg> = BTreeMap::new();
                for (principal, _) in ranked.iter().take(limit) {
                    for (t, _, agg) in rows_for(principal)? {
                        add(principal, t, &agg);
                        winners.entry(align(t)).or_default().merge(&agg);
                    }
                }
                let mut everyone: BTreeMap<u64, Agg> = BTreeMap::new();
                for (t, _, agg) in rows_for(ALL_PRINCIPALS)? {
                    everyone.entry(align(t)).or_default().merge(&agg);
                    overall.merge(&agg);
                }
                if ranked.len() > limit {
                    for (t, mut rest) in everyone {
                        if let Some(top) = winners.get(&t) {
                            rest.subtract(top);
                        }
                        if rest.requests > 0 {
                            add(OTHER_GROUP, t, &rest);
                        }
                    }
                }
            }
        }
        drop(db);

        let mut series: Vec<SeriesGroup> = Vec::new();
        for ((group, t), agg) in points {
            let point = SeriesPoint {
                t,
                stats: agg.stats(),
            };
            match series.last_mut() {
                Some(last) if last.key == group => last.points.push(point),
                _ => series.push(SeriesGroup {
                    key: group,
                    points: vec![point],
                }),
            }
        }
        let mut totals = totals
            .into_iter()
            .map(|(key, agg)| GroupTotal {
                key,
                stats: agg.stats(),
            })
            .collect::<Vec<_>>();
        totals.sort_by(|left, right| {
            (left.key == OTHER_GROUP)
                .cmp(&(right.key == OTHER_GROUP))
                .then_with(|| right.stats.requests.cmp(&left.stats.requests))
                .then_with(|| left.key.cmp(&right.key))
        });
        Ok(SeriesResponse {
            granularity: query.granularity,
            bucket_ms: width,
            from_ms: align(from_ms),
            to_ms,
            group_by: query.group_by,
            group_count,
            series,
            totals,
            overall: overall.stats(),
        })
    }

    /// Recent individual requests, newest first.
    ///
    /// # Errors
    ///
    /// Database read failure.
    pub fn recent(&self, query: &RecentQuery) -> Result<Vec<RequestRecord>, QueryError> {
        let limit = query.limit.unwrap_or(200).clamp(1, MAX_RECENT_LIMIT);
        let matches = |record: &RequestRecord| {
            query.before_ms.is_none_or(|before| record.t_ms < before)
                && query
                    .token
                    .as_deref()
                    .is_none_or(|token| token == record.principal)
                && query
                    .model
                    .as_deref()
                    .is_none_or(|model| model == record.model)
                && (!query.errors_only || !record_ok(record))
        };
        let db = self.db.lock();
        let mut out = self
            .state
            .lock()
            .pending
            .requests
            .iter()
            .rev()
            .filter(|record| matches(record))
            .take(limit)
            .cloned()
            .collect::<Vec<_>>();
        if out.len() < limit {
            let mut statement = db.prepare_cached(
                "SELECT record FROM requests
                 WHERE (?1 IS NULL OR principal = ?1)
                   AND (?2 IS NULL OR model = ?2)
                   AND (?3 = 0 OR ok = 0)
                   AND (?4 IS NULL OR t < ?4)
                 ORDER BY id DESC LIMIT ?5",
            )?;
            let rows = statement.query_map(
                params![
                    query.token,
                    query.model,
                    query.errors_only,
                    query.before_ms.map(sql_int),
                    sql_int((limit - out.len()) as u64),
                ],
                |row| row.get::<_, Vec<u8>>(0),
            )?;
            for blob in rows {
                if let Ok(record) = rmp_serde::from_slice::<RequestRecord>(&blob?) {
                    out.push(record);
                }
            }
        }
        Ok(out)
    }
}

impl LedgerMetrics {
    fn observe(&self, principal: &str, model: &str, record: &RequestRecord) {
        let status_class = match record.status {
            100..=199 => "1xx",
            200..=299 => "2xx",
            300..=399 => "3xx",
            400..=499 => "4xx",
            500..=599 => "5xx",
            _ => "other",
        };
        self.requests
            .with_label_values(&[principal, model, status_class])
            .inc();
        #[allow(clippy::cast_precision_loss)]
        for (kind, value) in [
            ("prompt", record.prompt_tokens),
            ("cached", record.cached_tokens),
            ("completion", record.completion_tokens),
        ] {
            if let Some(value) = value.filter(|value| *value > 0) {
                self.tokens
                    .with_label_values(&[principal, model, kind])
                    .inc_by(value as f64);
            }
        }
    }
}

/// Counters only: a per-principal latency histogram would multiply series by
/// bucket count for every token. Latency per token lives in the ledger.
fn register_metrics(registry: &Registry) -> anyhow::Result<LedgerMetrics> {
    let requests = CounterVec::new(
        Opts::new(
            "ramjet_principal_requests_total",
            "Proxied requests by authenticated principal, model, and status class",
        ),
        &["principal", "model", "status_class"],
    )?;
    let tokens = CounterVec::new(
        Opts::new(
            "ramjet_principal_tokens_total",
            "Response-reported tokens by principal, model, and kind (prompt, cached, completion)",
        ),
        &["principal", "model", "kind"],
    )?;
    registry.register(Box::new(requests.clone()))?;
    registry.register(Box::new(tokens.clone()))?;
    Ok(LedgerMetrics { requests, tokens })
}

/// Start of the local day containing `t`, for a zone `offset_ms` east of UTC.
fn align_day(t: u64, offset_ms: i64) -> u64 {
    let day = i64::try_from(DAY_MS).expect("day fits i64");
    let local = i64::try_from(t)
        .unwrap_or(i64::MAX)
        .saturating_add(offset_ms);
    let start = local.div_euclid(day) * day - offset_ms;
    u64::try_from(start).unwrap_or(0)
}

#[must_use]
pub fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: u64 = 1_760_000_400_000; // 2025-10-09T09:00:00Z, hour-aligned.

    fn usage(prompt: f64, cached: f64, completion: f64) -> Accumulator {
        Accumulator {
            prompt: Some(prompt),
            cached: Some(cached),
            completion: Some(completion),
            finish_reason: "stop".to_owned(),
            ..Accumulator::default()
        }
    }

    fn completion<'a>(
        principal: &'a str,
        model: &'a str,
        usage: Option<&'a Accumulator>,
        status: u16,
    ) -> Completion<'a> {
        Completion {
            principal,
            model: Some(model),
            endpoint: "chat",
            upstream: Some(1),
            status,
            stream: true,
            result: "complete",
            usage,
            first_token: Some(Duration::from_millis(180)),
            elapsed: Duration::from_millis(2_180),
            route_outcome: Some("affinity"),
            request_bytes: 1_000,
            response_bytes: 2_000,
        }
    }

    fn ledger() -> UsageLedger {
        UsageLedger::new(LedgerSettings::default(), None, None).unwrap()
    }

    fn range(from: u64, to: u64) -> SeriesQuery {
        SeriesQuery {
            from_ms: Some(from),
            to_ms: Some(to),
            ..SeriesQuery::default()
        }
    }

    fn temp_dir() -> std::path::PathBuf {
        let mut nonce = [0_u8; 8];
        getrandom::fill(&mut nonce).unwrap();
        let dir = std::env::temp_dir().join(format!("rj-usage-{}", crate::ui_auth::hex(&nonce)));
        std::fs::create_dir(&dir).unwrap();
        dir
    }

    #[test]
    fn usage_ledger_groups_by_token_and_model_with_cache_ratio() {
        for flushed in [false, true] {
            let ledger = ledger();
            let warm = usage(1_000.0, 900.0, 101.0);
            let cold = usage(1_000.0, 0.0, 11.0);
            ledger.record_at(T0, completion("tok-a", "qwen", Some(&warm), 200));
            ledger.record_at(T0 + 1, completion("tok-a", "glm", Some(&cold), 200));
            if flushed {
                ledger.flush(T0).unwrap();
            }
            ledger.record_at(T0 + 2, completion("master", "qwen", None, 503));
            let by_token = ledger
                .series(&range(T0 - HOUR_MS, T0 + HOUR_MS), T0)
                .unwrap();
            assert_eq!(by_token.overall.requests, 3);
            assert_eq!(by_token.overall.server_errors, 1);
            assert_eq!(by_token.group_count, 2);
            assert_eq!(by_token.totals[0].key, "tok-a");
            let tok_a = &by_token.totals[0].stats;
            assert_eq!(tok_a.prompt_tokens, 2_000);
            assert_eq!(tok_a.cached_tokens, 900);
            assert_eq!(tok_a.cache_hit_ratio, Some(0.45));
            assert_eq!((tok_a.cache_partial, tok_a.cache_cold), (1, 1));
            assert_eq!(tok_a.upstream_requests, vec![0, 2]);
            assert_eq!(tok_a.finish_reasons.get("stop"), Some(&2));
            assert!(tok_a.decode_tps_p50.is_some());

            let by_model = ledger
                .series(
                    &SeriesQuery {
                        group_by: GroupBy::Model,
                        token: Some("tok-a".to_owned()),
                        ..range(T0 - HOUR_MS, T0 + HOUR_MS)
                    },
                    T0,
                )
                .unwrap();
            let keys = by_model
                .series
                .iter()
                .map(|group| group.key.as_str())
                .collect::<Vec<_>>();
            assert_eq!(keys, ["glm", "qwen"]);
            assert_eq!(by_model.overall.requests, 2);

            let fleet_by_model = ledger
                .series(
                    &SeriesQuery {
                        group_by: GroupBy::Model,
                        ..range(T0 - HOUR_MS, T0 + HOUR_MS)
                    },
                    T0,
                )
                .unwrap();
            assert_eq!(fleet_by_model.totals[0].key, "qwen");
            assert_eq!(fleet_by_model.totals[0].stats.requests, 2);
        }
    }

    #[test]
    fn usage_ledger_folds_beyond_limit_into_an_exact_remainder() {
        let ledger = ledger();
        let one = usage(100.0, 50.0, 10.0);
        for (index, principal) in ["a", "b", "c", "d"].iter().enumerate() {
            for _ in 0..=index {
                ledger.record_at(T0, completion(principal, "m", Some(&one), 200));
            }
        }
        ledger.flush(T0).unwrap();
        ledger.record_at(T0 + 1, completion("a", "m", Some(&one), 500));
        let response = ledger
            .series(
                &SeriesQuery {
                    limit: Some(2),
                    ..range(T0 - HOUR_MS, T0 + HOUR_MS)
                },
                T0,
            )
            .unwrap();
        assert_eq!(response.group_count, 4);
        let keys = response
            .totals
            .iter()
            .map(|total| (total.key.as_str(), total.stats.requests))
            .collect::<Vec<_>>();
        // d=4, c=3 lead; a (1 + the unflushed error = 2) and b=2 fold.
        assert_eq!(keys, [("d", 4), ("c", 3), (OTHER_GROUP, 4)]);
        let other = &response.totals[2].stats;
        assert_eq!(other.server_errors, 1);
        assert_eq!(other.prompt_tokens, 400);
        assert_eq!(response.overall.requests, 11);
        assert!(other.ttft_p50_ms.is_some());
    }

    #[test]
    fn usage_ledger_minute_hour_and_local_day_buckets() {
        let ledger = ledger();
        let one = usage(10.0, 0.0, 1.0);
        ledger.record_at(T0, completion("t", "m", Some(&one), 200));
        ledger.record_at(T0 + 2 * MINUTE_MS, completion("t", "m", Some(&one), 200));
        ledger.flush(T0 + 2 * MINUTE_MS).unwrap();
        ledger.record_at(T0 + 20 * HOUR_MS, completion("t", "m", Some(&one), 200));
        let minute = ledger
            .series(
                &SeriesQuery {
                    granularity: Granularity::Minute,
                    group_by: GroupBy::None,
                    ..range(T0 - MINUTE_MS, T0 + 10 * MINUTE_MS)
                },
                T0 + 20 * HOUR_MS,
            )
            .unwrap();
        assert_eq!(minute.series[0].points.len(), 2);
        assert_eq!(
            minute.series[0].points[1].t - minute.series[0].points[0].t,
            2 * MINUTE_MS
        );
        let day = |tz_offset_min| {
            ledger
                .series(
                    &SeriesQuery {
                        granularity: Granularity::Day,
                        group_by: GroupBy::None,
                        tz_offset_min,
                        ..range(T0 - DAY_MS, T0 + 2 * DAY_MS)
                    },
                    T0,
                )
                .unwrap()
        };
        // 09:00Z and 05:00Z next day: two UTC days, one day at UTC-6.
        assert_eq!(day(0).series[0].points.len(), 2);
        let local = day(-360);
        assert_eq!(local.series[0].points.len(), 1);
        assert_eq!(local.series[0].points[0].stats.requests, 3);
        // Local midnight at UTC-6 is 06:00Z.
        assert_eq!((local.series[0].points[0].t - 6 * HOUR_MS) % DAY_MS, 0);
    }

    #[test]
    fn usage_ledger_applies_retention_and_caps_models() {
        let ledger = UsageLedger::new(
            LedgerSettings {
                minute_retention_hours: 1,
                hour_retention_days: 1,
                recent_capacity: 3,
            },
            None,
            None,
        )
        .unwrap();
        for index in 0..(MAX_MODELS + 3) {
            let model = format!("model-{index}");
            ledger.record_at(T0, completion("t", &model, None, 200));
        }
        let models = ledger.models();
        assert!(models.contains(&OTHER_MODEL.to_owned()));
        assert_eq!(models.len(), MAX_MODELS + 1);
        ledger.flush(T0).unwrap();
        assert_eq!(ledger.recent(&RecentQuery::default()).unwrap().len(), 3);
        ledger.record_at(T0 + 2 * DAY_MS, completion("t", "model-0", None, 200));
        ledger.flush(T0 + 2 * DAY_MS).unwrap();
        let db = ledger.db.lock();
        let rows: i64 = db
            .query_row(
                "SELECT COUNT(*) FROM usage WHERE principal = 't' AND t >= ?1",
                [sql_int(T0 + 2 * DAY_MS - HOUR_MS)],
                |row| row.get(0),
            )
            .unwrap();
        let all_rows: i64 = db
            .query_row(
                "SELECT COUNT(*) FROM usage WHERE principal = 't'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        // Only the new minute and hour buckets survive.
        assert_eq!((rows, all_rows), (2, 2));
        let requests: i64 = db
            .query_row("SELECT COUNT(*) FROM requests", [], |row| row.get(0))
            .unwrap();
        assert_eq!(requests, 3);
    }

    #[test]
    fn usage_ledger_recent_filters_newest_first_across_flushes() {
        let ledger = ledger();
        ledger.record_at(T0, completion("a", "m", None, 200));
        ledger.record_at(T0 + 1, completion("b", "m", None, 401));
        ledger.flush(T0).unwrap();
        ledger.record_at(T0 + 2, completion("a", "m", None, 200));
        let recent = ledger.recent(&RecentQuery::default()).unwrap();
        assert_eq!(
            recent.iter().map(|record| record.t_ms).collect::<Vec<_>>(),
            [T0 + 2, T0 + 1, T0]
        );
        let errors = ledger
            .recent(&RecentQuery {
                errors_only: true,
                ..RecentQuery::default()
            })
            .unwrap();
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].principal, "b");
        let only_a = ledger
            .recent(&RecentQuery {
                token: Some("a".to_owned()),
                before_ms: Some(T0 + 2),
                ..RecentQuery::default()
            })
            .unwrap();
        assert_eq!(only_a.len(), 1);
        assert_eq!(only_a[0].t_ms, T0);
    }

    #[test]
    fn usage_ledger_histogram_quantiles_interpolate_and_subtract() {
        let mut hist = Hist::default();
        for value in [100.0, 100.0, 100.0, 1_000.0] {
            hist.observe(TTFT_BOUNDS_MS, value);
        }
        let p50 = hist.quantile(TTFT_BOUNDS_MS, 0.5).unwrap();
        assert!((75.0..=100.0).contains(&p50), "{p50}");
        let p99 = hist.quantile(TTFT_BOUNDS_MS, 0.99).unwrap();
        assert!((750.0..=1_000.0).contains(&p99), "{p99}");
        assert_eq!(hist.mean(), Some(325.0));
        let mut slow = Hist::default();
        slow.observe(TTFT_BOUNDS_MS, 1_000.0);
        hist.subtract(&slow);
        assert_eq!(hist.count(), 3);
        assert_eq!(hist.mean(), Some(100.0));
        let mut overflow = Hist::default();
        overflow.observe(TTFT_BOUNDS_MS, 1e9);
        assert_eq!(overflow.quantile(TTFT_BOUNDS_MS, 0.5), Some(300_000.0));
    }

    #[test]
    fn usage_ledger_rejects_unbounded_queries() {
        let ledger = ledger();
        assert!(
            ledger
                .series(
                    &SeriesQuery {
                        granularity: Granularity::Minute,
                        ..range(0, T0)
                    },
                    T0
                )
                .is_err()
        );
        assert!(ledger.series(&range(T0, T0), T0).is_err());
    }

    #[test]
    fn usage_ledger_persists_across_reopen_and_merges_open_buckets() {
        let dir = temp_dir();
        let warm = usage(100.0, 50.0, 20.0);
        let now = unix_ms();
        {
            let ledger = UsageLedger::new(LedgerSettings::default(), None, Some(&dir)).unwrap();
            ledger.record_at(now, completion("tok", "qwen", Some(&warm), 200));
            ledger.flush(now).unwrap();
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.join(DB_FILE))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let reopened = UsageLedger::new(LedgerSettings::default(), None, Some(&dir)).unwrap();
        assert_eq!(reopened.models(), ["qwen"]);
        // The same open bucket keeps accumulating after a restart.
        reopened.record_at(now, completion("tok", "qwen", Some(&warm), 200));
        reopened.flush(now).unwrap();
        let series = reopened
            .series(&range(now - HOUR_MS, now + 1), now)
            .unwrap();
        assert_eq!(series.overall.requests, 2);
        assert_eq!(series.overall.cached_tokens, 100);
        assert!(series.overall.ttft_p50_ms.is_some());
        assert_eq!(reopened.recent(&RecentQuery::default()).unwrap().len(), 2);
        assert_eq!(reopened.principals().unwrap(), ["tok"]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn usage_ledger_exports_principal_counters() {
        let registry = Registry::new();
        let ledger = UsageLedger::new(LedgerSettings::default(), Some(&registry), None).unwrap();
        let warm = usage(100.0, 50.0, 20.0);
        ledger.record_at(T0, completion("tok", "qwen", Some(&warm), 200));
        let text = {
            use prometheus::Encoder;
            let mut out = Vec::new();
            prometheus::TextEncoder::new()
                .encode(&registry.gather(), &mut out)
                .unwrap();
            String::from_utf8(out).unwrap()
        };
        assert!(text.contains(
            r#"ramjet_principal_requests_total{model="qwen",principal="tok",status_class="2xx"} 1"#
        ));
        assert!(text.contains(
            r#"ramjet_principal_tokens_total{kind="cached",model="qwen",principal="tok"} 50"#
        ));
        assert!(!text.contains("principal=\"*\""));
    }
}
