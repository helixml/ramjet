//! Time-decayed prefix affinity calibrated to the engine's eviction horizon.
//!
//! The approximate locality index remembers which fingerprint blocks each
//! replica has served, but a served block is only worth routing toward while
//! the engine still holds its KV. vLLM's prefix cache frees blocks into an
//! LRU queue and evicts from its head, so residency is a step function in
//! block age: everything last used more recently than the block currently
//! being evicted is present, and everything older is gone. That age is the
//! *eviction horizon*. A fingerprint older than it attracts traffic to a
//! replica that will pay a cold prefill anyway, usually on the loaded side
//! because that is where the warm-looking prefix lives.
//!
//! Two horizon sources are supported. `static` treats the horizon as a fixed
//! age, which is the right tool for a first observe-mode capture. `fill`
//! models the LRU directly: given the engine's KV capacity in tokens, the
//! horizon is the age of the oldest served fill that is still inside that
//! capacity. It self-calibrates from the tokens the router itself sends and
//! reports an unbounded horizon until the replica has been filled once.
//!
//! The fill model is deliberately conservative about what it does not see.
//! Only completed responses proxied through ramjet count as fill, so traffic
//! that reaches an engine directly makes the estimate optimistic; treat that
//! as a measurement caveat rather than as a reason to guess. Responses whose
//! `cached_tokens` is untrusted (Qwen reports zero) count every prompt token
//! as new, which shortens the horizon and is therefore safe.

use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

use serde::Serialize;

/// Bound on retained fill samples per replica. Beyond it adjacent interior
/// samples (every one but the oldest) merge pairwise onto the newer
/// timestamp, which can only shorten a later horizon.
const MAX_FILL_SAMPLES: usize = 65_536;

/// Bound on run-length age entries recorded per candidate for the journal.
/// Ages along a matched chain are non-decreasing and shared by every block
/// the same response touched, so real chains produce a handful of runs.
pub const MAX_AGE_RUNS: usize = 64;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AffinityHorizonMode {
    /// Legacy behaviour: age is recorded for the journal but never scored.
    Off,
    /// Score with raw overlap, publish what the horizon would have changed.
    Observe,
    /// Score with horizon-fresh overlap only.
    Enforce,
}

impl AffinityHorizonMode {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Observe => "observe",
            Self::Enforce => "enforce",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AffinityHorizonSource {
    /// Every replica evicts blocks older than one fixed age.
    Static { seconds: u64 },
    /// One KV capacity in tokens per replica drives the LRU fill model.
    /// `None` marks a replica whose capacity is not known; it is never
    /// treated as evicting, so it can only keep credit, never lose it.
    Fill { capacity_tokens: Vec<Option<u64>> },
}

impl AffinityHorizonSource {
    #[must_use]
    pub const fn label(&self) -> &'static str {
        match self {
            Self::Static { .. } => "static",
            Self::Fill { .. } => "fill",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AffinityHorizonConfig {
    pub mode: AffinityHorizonMode,
    /// Present whenever the mode is not `Off`.
    pub source: Option<AffinityHorizonSource>,
}

impl AffinityHorizonConfig {
    #[must_use]
    pub const fn off() -> Self {
        Self {
            mode: AffinityHorizonMode::Off,
            source: None,
        }
    }

    #[must_use]
    pub fn source_label(&self) -> &'static str {
        self.source
            .as_ref()
            .map_or("none", AffinityHorizonSource::label)
    }

    /// One estimator per upstream.
    ///
    /// # Panics
    ///
    /// Panics when a non-off mode has no source or the fill capacities do not
    /// match the upstream count; configuration parsing guarantees both.
    #[must_use]
    pub fn estimators(&self, upstreams: usize) -> Vec<HorizonEstimator> {
        match (&self.mode, &self.source) {
            (AffinityHorizonMode::Off, _) => vec![HorizonEstimator::Unbounded; upstreams],
            (_, None) => panic!("affinity horizon mode needs a source"),
            (_, Some(AffinityHorizonSource::Static { seconds })) => {
                vec![HorizonEstimator::Static(Duration::from_secs(*seconds)); upstreams]
            }
            (_, Some(AffinityHorizonSource::Fill { capacity_tokens })) => {
                assert_eq!(
                    capacity_tokens.len(),
                    upstreams,
                    "affinity horizon needs one KV capacity per upstream"
                );
                capacity_tokens
                    .iter()
                    .map(|capacity| {
                        capacity.map_or(HorizonEstimator::Unbounded, |capacity| {
                            HorizonEstimator::Fill(FillHorizon::new(capacity))
                        })
                    })
                    .collect()
            }
        }
    }
}

/// Per-replica eviction-horizon estimate.
#[derive(Clone, Debug)]
pub enum HorizonEstimator {
    /// No eviction is modelled; every served block stays creditable.
    Unbounded,
    Static(Duration),
    Fill(FillHorizon),
}

impl HorizonEstimator {
    /// Age beyond which a served block is treated as evicted. `None` means no
    /// bound: nothing has been evicted yet or eviction is not modelled.
    #[must_use]
    pub fn horizon(&self, now: Instant) -> Option<Duration> {
        match self {
            Self::Unbounded => None,
            Self::Static(horizon) => Some(*horizon),
            Self::Fill(fill) => fill.horizon(now),
        }
    }

    /// Retained fill samples, oldest first; empty unless this is a fill model.
    #[cfg(test)]
    pub(crate) fn fill_samples(&self) -> Vec<(Instant, u64)> {
        match self {
            Self::Fill(fill) => fill.samples.iter().copied().collect(),
            Self::Unbounded | Self::Static(_) => Vec::new(),
        }
    }

    /// Account for KV that a completed response newly wrote on this replica.
    pub fn record_fill(&mut self, tokens: u64, now: Instant) {
        if let Self::Fill(fill) = self {
            fill.record(tokens, now);
        }
    }

    /// Forget all fill history after the replica's KV cache was discarded
    /// (engine sleep): its cache is empty, so nothing has been evicted yet.
    pub fn reset(&mut self) {
        if let Self::Fill(fill) = self {
            fill.clear();
        }
    }
}

/// LRU fill model for one replica.
///
/// A block last used at time `t` is evicted once at least `capacity_tokens`
/// of newer KV have been written after it. Keeping the minimal suffix of fill
/// samples whose sum reaches the capacity therefore leaves, at the front, the
/// oldest time that can still be resident.
#[derive(Clone, Debug)]
pub struct FillHorizon {
    capacity_tokens: u64,
    samples: VecDeque<(Instant, u64)>,
    total: u64,
}

impl FillHorizon {
    /// # Panics
    ///
    /// Panics on a zero capacity; configuration parsing rejects it first.
    #[must_use]
    pub fn new(capacity_tokens: u64) -> Self {
        assert!(capacity_tokens > 0, "KV capacity must be positive");
        Self {
            capacity_tokens,
            samples: VecDeque::new(),
            total: 0,
        }
    }

    /// Drops every fill sample, as for a freshly started replica.
    pub fn clear(&mut self) {
        self.samples.clear();
        self.total = 0;
    }

    pub fn record(&mut self, tokens: u64, now: Instant) {
        if tokens == 0 {
            return;
        }
        // The window is ordered by time. A sample stamped earlier than the
        // newest one is attributed to the newest instant instead, which keeps
        // the deque sorted and can only shorten the horizon.
        let now = self
            .samples
            .back()
            .map_or(now, |&(newest, _)| newest.max(now));
        match self.samples.back_mut() {
            Some(newest) if newest.0 == now => newest.1 = newest.1.saturating_add(tokens),
            Some(_) | None => self.samples.push_back((now, tokens)),
        }
        self.total = self.total.saturating_add(tokens);
        while let Some(&(_, front)) = self.samples.front() {
            if self.total.saturating_sub(front) >= self.capacity_tokens {
                self.samples.pop_front();
                self.total -= front;
            } else {
                break;
            }
        }
        if self.samples.len() > MAX_FILL_SAMPLES {
            self.coarsen();
        }
    }

    /// Halves the retained samples by merging interior neighbours onto the
    /// newer timestamp of each pair.
    ///
    /// The front is never merged. It is the horizon, and the eviction loop
    /// must keep popping it as soon as one capacity of newer fill exists;
    /// folding it into its neighbour instead grows it until the loop can never
    /// pop it again, after which `total` and the horizon age grow without
    /// bound. Interior merges leave both `total` and the front unchanged, so
    /// the eviction invariant `total - front < capacity` still holds.
    fn coarsen(&mut self) {
        let Some(front) = self.samples.pop_front() else {
            return;
        };
        let mut coarse = VecDeque::with_capacity(self.samples.len() / 2 + 2);
        coarse.push_back(front);
        while let Some((older_at, older)) = self.samples.pop_front() {
            coarse.push_back(match self.samples.pop_front() {
                Some((newer_at, newer)) => (newer_at, older.saturating_add(newer)),
                None => (older_at, older),
            });
        }
        self.samples = coarse;
    }

    #[must_use]
    pub fn horizon(&self, now: Instant) -> Option<Duration> {
        if self.total < self.capacity_tokens {
            return None;
        }
        self.samples
            .front()
            .map(|(at, _)| now.saturating_duration_since(*at))
    }

    /// Tokens written since the oldest retained sample; less than one capacity
    /// plus that oldest sample.
    #[must_use]
    pub const fn tracked_tokens(&self) -> u64 {
        self.total
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AffinityHorizonOutcome {
    Off,
    /// No candidate had any served prefix to decay.
    NoOverlap,
    /// Every credited block on every candidate was inside its horizon.
    Fresh,
    /// Some credited blocks were stale but the winner did not change.
    Trimmed,
    /// Observe mode: decayed scoring would have chosen another replica.
    WouldMove,
    /// Enforce mode: decayed scoring chose a replica raw scoring would not.
    Moved,
}

impl AffinityHorizonOutcome {
    pub const ALL: [Self; 6] = [
        Self::Off,
        Self::NoOverlap,
        Self::Fresh,
        Self::Trimmed,
        Self::WouldMove,
        Self::Moved,
    ];

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::NoOverlap => "no_overlap",
            Self::Fresh => "fresh",
            Self::Trimmed => "trimmed",
            Self::WouldMove => "would_move",
            Self::Moved => "moved",
        }
    }
}

/// Journal- and metric-safe summary of one decision's horizon effect.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct AffinityHorizonObservation {
    pub mode: &'static str,
    pub source: &'static str,
    pub outcome: &'static str,
}

impl AffinityHorizonObservation {
    #[must_use]
    pub const fn off() -> Self {
        Self {
            mode: "off",
            source: "none",
            outcome: "off",
        }
    }
}

/// Per-candidate locality walk over a fingerprint chain.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Locality {
    /// Leading blocks present in the index regardless of age.
    pub raw_overlap: usize,
    /// Leading blocks present and no older than the horizon.
    pub fresh_overlap: usize,
    /// Run-length `[blocks, age_ms]` over the raw overlap, oldest last.
    pub ages: Vec<[u64; 2]>,
    /// Horizon applied, in milliseconds; `None` when unbounded.
    pub horizon_ms: Option<u64>,
}

impl Locality {
    /// Walk `fingerprints` against a lookup of last-served instants.
    pub fn walk<'a>(
        fingerprints: impl IntoIterator<Item = &'a u64>,
        mut last_served: impl FnMut(&u64) -> Option<Instant>,
        horizon: Option<Duration>,
        now: Instant,
    ) -> Self {
        let mut locality = Self {
            horizon_ms: horizon.map(millis),
            ..Self::default()
        };
        let mut fresh_done = false;
        for fingerprint in fingerprints {
            let Some(seen) = last_served(fingerprint) else {
                break;
            };
            locality.raw_overlap += 1;
            let age = now.saturating_duration_since(seen);
            let age_ms = millis(age);
            match locality.ages.last_mut() {
                Some(run) if run[1] == age_ms => run[0] += 1,
                Some(_) | None => {
                    if locality.ages.len() < MAX_AGE_RUNS {
                        locality.ages.push([1, age_ms]);
                    }
                }
            }
            if !fresh_done && horizon.is_none_or(|horizon| age <= horizon) {
                locality.fresh_overlap += 1;
            } else {
                fresh_done = true;
            }
        }
        locality
    }

    #[must_use]
    pub const fn stale_blocks(&self) -> usize {
        self.raw_overlap.saturating_sub(self.fresh_overlap)
    }
}

#[must_use]
pub fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn secs(seconds: u64) -> Duration {
        Duration::from_secs(seconds)
    }

    #[test]
    fn a_cleared_fill_horizon_is_unbounded_again() {
        let start = Instant::now();
        let mut estimator = HorizonEstimator::Fill(FillHorizon::new(1_000));
        estimator.record_fill(1_200, start);
        assert_eq!(estimator.horizon(start + secs(5)), Some(secs(5)));
        estimator.reset();
        assert_eq!(estimator.horizon(start + secs(10)), None);
        estimator.record_fill(400, start + secs(20));
        assert_eq!(estimator.horizon(start + secs(30)), None);
    }

    #[test]
    fn fill_horizon_is_unbounded_until_one_capacity_has_been_written() {
        let start = Instant::now();
        let mut fill = FillHorizon::new(1_000);
        assert_eq!(fill.horizon(start), None);
        fill.record(400, start);
        fill.record(400, start + secs(10));
        assert_eq!(fill.horizon(start + secs(20)), None);
        fill.record(400, start + secs(20));
        // 1,200 tokens now cover the capacity; the oldest sample is needed.
        assert_eq!(fill.horizon(start + secs(30)), Some(secs(30)));
    }

    #[test]
    fn fill_horizon_drops_samples_no_longer_needed_to_cover_capacity() {
        let start = Instant::now();
        let mut fill = FillHorizon::new(1_000);
        fill.record(600, start);
        fill.record(600, start + secs(10));
        fill.record(600, start + secs(20));
        // The first sample is redundant: the newer two already exceed 1,000.
        assert_eq!(fill.horizon(start + secs(25)), Some(secs(15)));
        assert_eq!(fill.tracked_tokens(), 1_200);
    }

    #[test]
    fn fill_horizon_grows_while_the_replica_is_idle() {
        let start = Instant::now();
        let mut fill = FillHorizon::new(100);
        fill.record(100, start);
        assert_eq!(fill.horizon(start + secs(1)), Some(secs(1)));
        assert_eq!(fill.horizon(start + secs(3_600)), Some(secs(3_600)));
    }

    #[test]
    fn fill_horizon_ignores_empty_fill_and_bounds_samples_conservatively() {
        let start = Instant::now();
        let mut fill = FillHorizon::new(u64::MAX);
        fill.record(0, start);
        assert!(fill.samples.is_empty());
        for offset in 0..(MAX_FILL_SAMPLES as u64 + 10) {
            fill.record(1, start + Duration::from_millis(offset));
        }
        assert!(fill.samples.len() <= MAX_FILL_SAMPLES);
        assert_eq!(fill.tracked_tokens(), MAX_FILL_SAMPLES as u64 + 10);
        assert_eq!(
            fill.samples.iter().map(|sample| sample.1).sum::<u64>(),
            fill.tracked_tokens()
        );
        // The oldest sample is the horizon and is never merged away.
        assert_eq!(fill.samples.front(), Some(&(start, 1)));
        assert!(
            fill.samples
                .iter()
                .zip(fill.samples.iter().skip(1))
                .all(|(older, newer)| older.0 < newer.0)
        );
    }

    /// Exact horizon by brute force over every sample: the age of the oldest
    /// sample with less than `capacity` tokens written after it.
    fn exact_horizon(samples: &[(Instant, u64)], capacity: u64, now: Instant) -> Option<Duration> {
        let total = samples.iter().map(|sample| sample.1).sum::<u64>();
        if total < capacity {
            return None;
        }
        let mut newer = 0_u64;
        let mut oldest = None;
        for &(at, tokens) in samples.iter().rev() {
            if newer >= capacity {
                break;
            }
            oldest = Some(at);
            newer += tokens;
        }
        oldest.map(|at| now.saturating_duration_since(at))
    }

    #[test]
    fn fill_horizon_stays_bounded_past_the_sample_cap() {
        // One large fill followed by far more than MAX_FILL_SAMPLES one-token
        // fills: the regression snowballed the front sample so it was never
        // evicted, `total` grew without bound, and the horizon froze on the
        // 65,535th-newest sample instead of one capacity ago.
        let capacity = 100_000_u64;
        let start = Instant::now();
        let mut fill = FillHorizon::new(capacity);
        let mut history = vec![(start, 2 * capacity)];
        fill.record(2 * capacity, start);
        let small = 3 * MAX_FILL_SAMPLES as u64;
        for offset in 1..=small {
            let at = start + Duration::from_millis(offset);
            fill.record(1, at);
            history.push((at, 1));
            assert!(fill.samples.len() <= MAX_FILL_SAMPLES);
            assert!(
                fill.tracked_tokens() < capacity + fill.samples.front().unwrap().1,
                "front must stay evictable"
            );
        }
        assert!(fill.tracked_tokens() <= 2 * capacity);
        let now = start + Duration::from_millis(small + 1);
        let exact = exact_horizon(&history, capacity, now).unwrap();
        let modelled = fill.horizon(now).unwrap();
        // Coarsening may only shorten the horizon, by a few merged samples.
        assert!(modelled <= exact, "{modelled:?} > {exact:?}");
        assert!(
            exact.checked_sub(modelled).unwrap() <= Duration::from_millis(8),
            "{modelled:?} vs {exact:?}"
        );
        // One capacity ago, not the 65,535th-newest sample.
        assert!(exact >= Duration::from_millis(capacity - 1));
    }

    #[test]
    fn fill_horizon_matches_brute_force_across_coarsening() {
        // About 80,000 samples per capacity, so coarsening runs repeatedly.
        let capacity = 200_000_u64;
        let start = Instant::now();
        let mut fill = FillHorizon::new(capacity);
        let mut history = Vec::new();
        let mut seed = 0x9e37_79b9_u64;
        for offset in 1..=(3 * MAX_FILL_SAMPLES as u64) {
            seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
            let tokens = 1 + (seed >> 62);
            let at = start + Duration::from_millis(offset);
            fill.record(tokens, at);
            history.push((at, tokens));
            if offset % 4_099 == 0 {
                let now = at + Duration::from_millis(1);
                let exact = exact_horizon(&history, capacity, now);
                let modelled = fill.horizon(now);
                assert_eq!(exact.is_some(), modelled.is_some());
                if let (Some(exact), Some(modelled)) = (exact, modelled) {
                    assert!(modelled <= exact);
                    assert!(exact.checked_sub(modelled).unwrap() <= Duration::from_millis(64));
                }
            }
        }
        assert!(fill.tracked_tokens() < 2 * capacity);
    }

    #[test]
    fn fill_horizon_keeps_samples_ordered_when_timestamps_arrive_late() {
        let start = Instant::now();
        let mut fill = FillHorizon::new(1_000);
        fill.record(600, start + secs(10));
        // Stamped before the newest sample, as a completion that read the
        // clock before a concurrent one took the router lock would be.
        fill.record(600, start + secs(5));
        assert_eq!(fill.samples.len(), 1, "attributed to the newest instant");
        assert_eq!(fill.samples.front(), Some(&(start + secs(10), 1_200)));
        // Never longer than what monotone stamping would report.
        assert_eq!(fill.horizon(start + secs(20)), Some(secs(10)));
        fill.record(500, start + secs(30));
        assert!(
            fill.samples
                .iter()
                .zip(fill.samples.iter().skip(1))
                .all(|(older, newer)| older.0 < newer.0)
        );
    }

    #[test]
    fn estimators_follow_the_configured_source() {
        let now = Instant::now();
        let off = AffinityHorizonConfig::off().estimators(2);
        assert_eq!(off.len(), 2);
        assert_eq!(off[0].horizon(now), None);

        let stat = AffinityHorizonConfig {
            mode: AffinityHorizonMode::Observe,
            source: Some(AffinityHorizonSource::Static { seconds: 90 }),
        }
        .estimators(3);
        assert!(stat.iter().all(|e| e.horizon(now) == Some(secs(90))));

        let mut fill = AffinityHorizonConfig {
            mode: AffinityHorizonMode::Enforce,
            source: Some(AffinityHorizonSource::Fill {
                capacity_tokens: vec![Some(10), Some(20), None],
            }),
        }
        .estimators(3);
        for estimator in &mut fill {
            estimator.record_fill(10, now);
        }
        assert_eq!(fill[0].horizon(now + secs(1)), Some(secs(1)));
        assert_eq!(fill[1].horizon(now + secs(1)), None);
        // An unknown capacity never evicts, however much is served.
        fill[2].record_fill(u64::MAX, now);
        assert_eq!(fill[2].horizon(now + secs(3_600)), None);
    }

    #[test]
    #[should_panic(expected = "one KV capacity per upstream")]
    fn fill_capacity_cardinality_is_checked() {
        let _ = AffinityHorizonConfig {
            mode: AffinityHorizonMode::Enforce,
            source: Some(AffinityHorizonSource::Fill {
                capacity_tokens: vec![Some(10)],
            }),
        }
        .estimators(2);
    }

    #[test]
    fn locality_walk_credits_only_leading_blocks_inside_the_horizon() {
        let now = Instant::now();
        let mut seen = HashMap::new();
        for fingerprint in 1..=4_u64 {
            seen.insert(fingerprint, now.checked_sub(secs(5)).unwrap());
        }
        for fingerprint in 5..=7_u64 {
            seen.insert(fingerprint, now.checked_sub(secs(500)).unwrap());
        }
        let chain = [1, 2, 3, 4, 5, 6, 7, 8, 9];
        let lookup = |fingerprint: &u64| seen.get(fingerprint).copied();

        let bounded = Locality::walk(&chain, lookup, Some(secs(60)), now);
        assert_eq!(bounded.raw_overlap, 7);
        assert_eq!(bounded.fresh_overlap, 4);
        assert_eq!(bounded.stale_blocks(), 3);
        assert_eq!(bounded.ages, vec![[4, 5_000], [3, 500_000]]);
        assert_eq!(bounded.horizon_ms, Some(60_000));

        let unbounded = Locality::walk(&chain, lookup, None, now);
        assert_eq!(unbounded.fresh_overlap, 7);
        assert_eq!(unbounded.horizon_ms, None);
        assert_eq!(unbounded.ages, bounded.ages);
    }

    #[test]
    fn locality_walk_never_credits_past_a_stale_block() {
        // A fresh block behind a stale one cannot be reached: the engine's
        // chain is prefix-closed, and so is the credit.
        let now = Instant::now();
        let ages = [secs(1), secs(999), secs(1)];
        let chain = [10_u64, 11, 12];
        let lookup = |fingerprint: &u64| {
            chain
                .iter()
                .position(|value| value == fingerprint)
                .map(|position| now.checked_sub(ages[position]).unwrap())
        };
        let locality = Locality::walk(&chain, lookup, Some(secs(10)), now);
        assert_eq!(locality.raw_overlap, 3);
        assert_eq!(locality.fresh_overlap, 1);
    }

    #[test]
    fn locality_age_runs_are_bounded_but_raw_overlap_is_not() {
        let now = Instant::now();
        let chain = (0..(MAX_AGE_RUNS as u64 + 8)).collect::<Vec<_>>();
        let lookup = |fingerprint: &u64| now.checked_sub(Duration::from_millis(*fingerprint));
        let locality = Locality::walk(&chain, lookup, None, now);
        assert_eq!(locality.raw_overlap, chain.len());
        assert_eq!(locality.ages.len(), MAX_AGE_RUNS);
    }

    #[test]
    fn labels_are_bounded_and_serializable() {
        for outcome in AffinityHorizonOutcome::ALL {
            assert!(!outcome.label().is_empty());
        }
        let encoded = serde_json::to_string(&AffinityHorizonObservation::off()).unwrap();
        assert_eq!(encoded, r#"{"mode":"off","source":"none","outcome":"off"}"#);
    }
}
