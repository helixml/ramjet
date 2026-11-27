//! Long-session placement: keep conversations that each fill most of a
//! replica's KV pool on different replicas.
//!
//! A replica's prefix cache is an LRU over a fixed token pool. Two agent
//! conversations of ~300k tokens do not fit a 500k-token pool together, so
//! when both route to the same replica each turn of one evicts the other and
//! every turn re-prefills hundreds of thousands of tokens. Measured on node06
//! on 2026-10-26: four to five 310k-440k conversations on two GLM replicas
//! paid 45-112s TTFT on roughly one turn in ten, while a cache hit took 2s.
//!
//! The approximate prefix index cannot see that: it remembers every block a
//! replica served and never learns of an eviction, so it keeps crediting the
//! evicted conversation to the replica that lost it. This module keeps a small
//! per-replica model of which long conversations are resident:
//!
//! * On completion, a prompt at or above the threshold is recorded on the
//!   replica that served it with its engine-reported token count, and removed
//!   from every other replica. If the replica's resident total then exceeds
//!   its KV capacity, the least recently served conversations are evicted
//!   from the model, and their blocks that nothing has touched since are
//!   removed from the router's prefix index so they stop attracting traffic.
//! * When the engine reports a cache miss for a conversation the model
//!   thought was resident, the model was optimistic about that replica:
//!   every conversation served there before it is treated as evicted too.
//! * At routing time, a long prompt whose chosen replica already holds other
//!   long conversations that, together with this one, exceed the capacity is
//!   moved to the best-ranked serving replica where it fits. When none fits,
//!   ordinary routing stands.
//!
//! A conversation is identified by its fingerprint chain: a later turn is the
//! same conversation when its chain is at least as long as the recorded one
//! and repeats all of it except a small tail (the recorded turn's partial last
//! block and generation prompt). A shared system prompt alone never aliases
//! two conversations, and a shorter prompt never inherits a longer one's size.
//! Only fixed outcome labels reach the journal and metrics.

use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde::Serialize;

use crate::router::Decision;

/// Bound on remembered conversations per replica. The capacity model keeps at
/// most `capacity / threshold + 1`, so this only bounds a misconfiguration.
const MAX_RESIDENTS: usize = 16;

/// Initial tokens per fingerprint block before the first long completion
/// calibrates it: 2 KiB canonical blocks at about four bytes per token.
const DEFAULT_TOKENS_PER_BLOCK: f64 = 512.0;

/// Blocks at the end of a recorded chain that a continuation may rewrite: the
/// partial last block and the generation prompt that the next turn replaces
/// with the assistant's reply. Longer chains tolerate proportionally more.
const MIN_CONTINUATION_SLACK_BLOCKS: usize = 2;
const CONTINUATION_SLACK_DIVISOR: usize = 32;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LongSessionConfig {
    /// Prompt tokens at or above which a conversation is tracked; 0 is off.
    pub threshold_tokens: u64,
    /// KV pool size per upstream; `None` never counts as crowded.
    pub capacity_tokens: Vec<Option<u64>>,
    /// A conversation not served for this long is forgotten.
    pub ttl: Duration,
}

impl LongSessionConfig {
    #[must_use]
    pub const fn off() -> Self {
        Self {
            threshold_tokens: 0,
            capacity_tokens: Vec::new(),
            ttl: Duration::from_mins(30),
        }
    }

    #[must_use]
    pub const fn enabled(&self) -> bool {
        self.threshold_tokens > 0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LongSessionOutcome {
    Off,
    /// Estimated below the threshold.
    Below,
    /// The chosen replica has room for this conversation.
    Fits,
    /// Moved off a replica already full of other long conversations.
    Moved,
    /// Every serving candidate is full; ordinary routing stands.
    AllCrowded,
}

impl LongSessionOutcome {
    pub const ALL: [Self; 5] = [
        Self::Off,
        Self::Below,
        Self::Fits,
        Self::Moved,
        Self::AllCrowded,
    ];

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Below => "below",
            Self::Fits => "fits",
            Self::Moved => "moved",
            Self::AllCrowded => "all_crowded",
        }
    }
}

/// Privacy-bounded route-journal annotation.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct LongSessionObservation {
    pub outcome: &'static str,
    /// Estimated prompt tokens, rounded up to thousands; 0 when off.
    pub estimated_ktokens: u64,
}

impl LongSessionObservation {
    #[must_use]
    pub const fn off() -> Self {
        Self {
            outcome: LongSessionOutcome::Off.label(),
            estimated_ktokens: 0,
        }
    }

    /// Whether the request was long enough for a placement decision, the
    /// population `ramjet_route_long_session_total` records.
    #[must_use]
    pub fn counted(&self) -> bool {
        !matches!(self.outcome, "off" | "below")
    }
}

struct Resident {
    fingerprints: Vec<u64>,
    tokens: u64,
    last_served: Instant,
}

impl Resident {
    /// Whether a request with `fingerprints` continues this conversation.
    ///
    /// A continuation is never shorter than the recorded turn and repeats it
    /// up to a small tail. Two chains truncated at the router's cap are only
    /// the same conversation when they agree on almost the whole cap, not
    /// merely on a long shared system prompt.
    fn continued_by(&self, fingerprints: &[u64]) -> bool {
        let recorded = self.fingerprints.len();
        if recorded == 0 || fingerprints.len() < recorded {
            return false;
        }
        let shared = self
            .fingerprints
            .iter()
            .zip(fingerprints)
            .take_while(|(left, right)| left == right)
            .count();
        let slack = (recorded / CONTINUATION_SLACK_DIVISOR).max(MIN_CONTINUATION_SLACK_BLOCKS);
        shared > 0 && shared.saturating_add(slack) >= recorded
    }
}

struct State {
    residents: Vec<Vec<Resident>>,
    /// Calibrated tokens per fingerprint block per upstream. Replicas of
    /// different models tokenize differently, so one model's completions must
    /// not move another's estimate. `None` is uncalibrated.
    tokens_per_block: Vec<Option<f64>>,
    /// Whether an upstream has ever reported a positive cached-token count.
    /// Until it has, a zero is indistinguishable from an engine that does not
    /// report prefix-cache telemetry and is not evidence of a miss.
    cache_reported: Vec<bool>,
}

impl State {
    fn prune_expired(&mut self, now: Instant, ttl: Duration) {
        for residents in &mut self.residents {
            residents.retain(|resident| now.saturating_duration_since(resident.last_served) <= ttl);
        }
    }

    /// Tokens per block for a request that may be served by `candidates`,
    /// which the proxy has already restricted to one model's replicas: the
    /// mean of their calibrations, or the default before any calibrates.
    fn tokens_per_block(&self, candidates: &[usize]) -> f64 {
        let (sum, count) = candidates
            .iter()
            .filter_map(|&upstream| self.tokens_per_block.get(upstream).copied().flatten())
            .fold((0.0, 0_u32), |(sum, count), value| (sum + value, count + 1));
        if count == 0 {
            DEFAULT_TOKENS_PER_BLOCK
        } else {
            sum / f64::from(count)
        }
    }
}

pub struct LongSessions {
    config: LongSessionConfig,
    max_blocks: usize,
    state: Mutex<State>,
}

/// Blocks a caller should drop from the router's prefix index on `upstream`,
/// unless something served them after `served_before`.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Evicted {
    pub upstream: usize,
    pub fingerprints: Vec<u64>,
    pub served_before: Option<Instant>,
}

impl LongSessions {
    /// `max_blocks` is the router's fingerprint cap; a chain that reaches it
    /// is truncated and cannot calibrate tokens per block.
    #[must_use]
    pub fn new(config: LongSessionConfig, upstreams: usize, max_blocks: usize) -> Self {
        Self {
            config,
            max_blocks,
            state: Mutex::new(State {
                residents: (0..upstreams).map(|_| Vec::new()).collect(),
                tokens_per_block: vec![None; upstreams],
                cache_reported: vec![false; upstreams],
            }),
        }
    }

    fn capacity(&self, upstream: usize) -> Option<u64> {
        self.config.capacity_tokens.get(upstream).copied().flatten()
    }

    /// Moves a long prompt off a replica its conversation cannot share.
    pub fn place(
        &self,
        decision: &mut Decision,
        fingerprints: &[u64],
        now: Instant,
    ) -> LongSessionObservation {
        if !self.config.enabled() || fingerprints.is_empty() {
            return LongSessionObservation::off();
        }
        let mut state = self.state.lock();
        state.prune_expired(now, self.config.ttl);
        // A continued conversation is at least as long as its recorded turn.
        let recorded = state
            .residents
            .iter()
            .flatten()
            .filter(|resident| resident.continued_by(fingerprints))
            .map(|resident| resident.tokens)
            .max()
            .unwrap_or(0);
        #[allow(
            clippy::cast_precision_loss,
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss
        )]
        let estimated = recorded
            .max((fingerprints.len() as f64 * state.tokens_per_block(&decision.candidates)) as u64);
        let observation = |outcome: LongSessionOutcome| LongSessionObservation {
            outcome: outcome.label(),
            estimated_ktokens: estimated.div_ceil(1000),
        };
        if estimated < self.config.threshold_tokens {
            return observation(LongSessionOutcome::Below);
        }
        let fits = |upstream: usize| {
            let Some(capacity) = self.capacity(upstream) else {
                return true;
            };
            let others: u64 = state.residents[upstream]
                .iter()
                .filter(|resident| !resident.continued_by(fingerprints))
                .map(|resident| resident.tokens)
                .sum();
            others.saturating_add(estimated) <= capacity
        };
        let Some(&chosen) = decision.candidates.first() else {
            return observation(LongSessionOutcome::Fits);
        };
        if fits(chosen) {
            return observation(LongSessionOutcome::Fits);
        }
        let serving = |upstream: usize| {
            decision
                .candidate_state
                .iter()
                .any(|candidate| candidate.index == upstream && candidate.healthy)
        };
        // Candidate state is in score order, so the first fitting serving
        // replica is the best-ranked alternative.
        let alternative = decision
            .candidate_state
            .iter()
            .map(|candidate| candidate.index)
            .find(|&upstream| {
                upstream != chosen
                    && decision.candidates.contains(&upstream)
                    && serving(upstream)
                    && fits(upstream)
            });
        match alternative {
            Some(upstream) if decision.promote(upstream) => observation(LongSessionOutcome::Moved),
            _ => observation(LongSessionOutcome::AllCrowded),
        }
    }

    /// Records a completed generation and returns index blocks that the
    /// capacity model now considers evicted.
    pub fn served(
        &self,
        upstream: usize,
        fingerprints: &[u64],
        prompt_tokens: Option<u64>,
        cached_tokens: Option<u64>,
        now: Instant,
    ) -> Vec<Evicted> {
        let Some(prompt) = prompt_tokens else {
            return Vec::new();
        };
        if !self.config.enabled() || fingerprints.is_empty() {
            return Vec::new();
        }
        let mut state = self.state.lock();
        if upstream >= state.residents.len() {
            return Vec::new();
        }
        state.prune_expired(now, self.config.ttl);
        if cached_tokens.is_some_and(|cached| cached > 0) {
            state.cache_reported[upstream] = true;
        }
        if fingerprints.len() < self.max_blocks {
            #[allow(clippy::cast_precision_loss)]
            let observed = prompt as f64 / fingerprints.len() as f64;
            if prompt >= self.config.threshold_tokens && observed.is_finite() && observed > 0.0 {
                let calibrated = &mut state.tokens_per_block[upstream];
                *calibrated = Some(calibrated.map_or(observed, |previous| {
                    0.8f64.mul_add(previous, 0.2 * observed)
                }));
            }
        }
        if prompt < self.config.threshold_tokens {
            return Vec::new();
        }
        // Only an engine known to report prefix-cache hits can report a miss.
        let reported_cached = cached_tokens.filter(|_| state.cache_reported[upstream]);
        let mut evicted = Vec::new();
        // The conversation now lives here; a copy elsewhere is the LRU's to
        // drop and must not crowd out the replica it left.
        for (index, residents) in state.residents.iter_mut().enumerate() {
            if index != upstream {
                residents.retain(|resident| !resident.continued_by(fingerprints));
            }
        }
        let residents = &mut state.residents[upstream];
        // Replace every record of this conversation, not just the first: a
        // duplicate would count the same KV twice against capacity.
        let mut previous: Option<Resident> = None;
        residents.retain_mut(|resident| {
            if !resident.continued_by(fingerprints) {
                return true;
            }
            if previous
                .as_ref()
                .is_none_or(|kept| resident.last_served > kept.last_served)
            {
                previous = Some(Resident {
                    fingerprints: std::mem::take(&mut resident.fingerprints),
                    tokens: resident.tokens,
                    last_served: resident.last_served,
                });
            }
            false
        });
        // A miss for a conversation the model held resident means the replica
        // churned more than the model saw: everything older than its last turn
        // is gone too. Missing cache telemetry is not a miss.
        if let Some(previous) = &previous
            && let Some(cached) = reported_cached
            && cached.saturating_mul(2) < previous.tokens
        {
            let horizon = previous.last_served;
            residents.retain(|resident| {
                if resident.last_served <= horizon {
                    evicted.push(Evicted {
                        upstream,
                        fingerprints: resident.fingerprints.clone(),
                        served_before: Some(resident.last_served),
                    });
                    false
                } else {
                    true
                }
            });
        }
        residents.push(Resident {
            fingerprints: fingerprints.to_vec(),
            tokens: prompt,
            last_served: now,
        });
        if let Some(capacity) = self.capacity(upstream) {
            residents.sort_by_key(|resident| resident.last_served);
            let mut total: u64 = residents.iter().map(|resident| resident.tokens).sum();
            while (total > capacity && residents.len() > 1) || residents.len() > MAX_RESIDENTS {
                let oldest = residents.remove(0);
                total -= oldest.tokens;
                evicted.push(Evicted {
                    upstream,
                    fingerprints: oldest.fingerprints,
                    served_before: Some(oldest.last_served),
                });
            }
        } else if residents.len() > MAX_RESIDENTS {
            residents.sort_by_key(|resident| resident.last_served);
            residents.remove(0);
        }
        evicted
    }

    /// Forgets every resident conversation on a replica whose KV cache was
    /// discarded, so they neither crowd out new placements nor claim residency.
    pub fn invalidate(&self, upstream: usize) {
        if let Some(residents) = self.state.lock().residents.get_mut(upstream) {
            residents.clear();
        }
    }

    /// Modelled resident long-conversation tokens per upstream.
    #[must_use]
    pub fn resident_tokens(&self) -> Vec<u64> {
        self.state
            .lock()
            .residents
            .iter()
            .map(|residents| residents.iter().map(|resident| resident.tokens).sum())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::router::{CandidateState, Outcome};

    fn sessions(upstreams: usize) -> LongSessions {
        LongSessions::new(
            LongSessionConfig {
                threshold_tokens: 100_000,
                capacity_tokens: vec![Some(500_000); upstreams],
                ttl: Duration::from_mins(30),
            },
            upstreams,
            1024,
        )
    }

    /// A conversation `id` of `blocks` blocks sharing `shared` system blocks.
    fn chain(id: u64, shared: u64, blocks: u64) -> Vec<u64> {
        (0..blocks)
            .map(|block| {
                if block < shared {
                    block
                } else {
                    (id << 32) | block
                }
            })
            .collect()
    }

    fn decision(order: &[usize], healthy: &[bool]) -> Decision {
        Decision {
            candidates: order.to_vec(),
            candidate_state: order
                .iter()
                .enumerate()
                .map(|(rank, &index)| CandidateState {
                    index,
                    rank,
                    overlap_blocks: 0,
                    affinity_blocks: 0,
                    load_units: 0,
                    request_load_units: 1,
                    healthy: healthy[index],
                    stale_blocks: 0,
                    horizon_ms: None,
                    overlap_ages_ms: Vec::new(),
                })
                .collect(),
            overlap_blocks: 0,
            total_blocks: 0,
            affinity_blocks: 0,
            load_units: 1,
            rotation: 0,
            outcome: Outcome::Overlap,
        }
    }

    #[test]
    fn invalidating_a_replica_forgets_only_its_residents() {
        let sessions = sessions(2);
        let now = Instant::now();
        sessions.served(0, &chain(1, 4, 200), Some(350_000), Some(0), now);
        sessions.served(1, &chain(2, 4, 200), Some(300_000), Some(0), now);
        assert_eq!(sessions.resident_tokens(), vec![350_000, 300_000]);
        sessions.invalidate(1);
        assert_eq!(sessions.resident_tokens(), vec![350_000, 0]);
    }

    #[test]
    fn off_records_and_moves_nothing() {
        let off = LongSessions::new(LongSessionConfig::off(), 2, 1024);
        let now = Instant::now();
        let a = chain(1, 25, 700);
        assert!(off.served(0, &a, Some(350_000), Some(0), now).is_empty());
        let mut routed = decision(&[0, 1], &[true, true]);
        assert_eq!(off.place(&mut routed, &a, now).outcome, "off");
        assert_eq!(routed.candidates[0], 0);
    }

    #[test]
    fn a_second_long_conversation_moves_to_a_replica_where_it_fits() {
        let sessions = sessions(2);
        let now = Instant::now();
        let a = chain(1, 25, 700);
        let b = chain(2, 25, 700);
        sessions.served(0, &a, Some(350_000), Some(0), now);
        let mut routed = decision(&[0, 1], &[true, true]);
        let observed = sessions.place(&mut routed, &b, now);
        assert_eq!(observed.outcome, "moved");
        assert_eq!(routed.candidates[0], 1);
        assert_eq!(routed.candidate_state[0].index, 1);
    }

    #[test]
    fn a_continued_conversation_stays_home() {
        let sessions = sessions(2);
        let now = Instant::now();
        let a = chain(1, 25, 700);
        sessions.served(0, &a, Some(350_000), Some(0), now);
        let next_turn = chain(1, 25, 720);
        let mut routed = decision(&[0, 1], &[true, true]);
        assert_eq!(sessions.place(&mut routed, &next_turn, now).outcome, "fits");
        assert_eq!(routed.candidates[0], 0);
    }

    #[test]
    fn conversations_that_fit_together_share_a_replica() {
        let sessions = sessions(2);
        let now = Instant::now();
        sessions.served(0, &chain(1, 25, 400), Some(200_000), Some(0), now);
        let mut routed = decision(&[0, 1], &[true, true]);
        let observed = sessions.place(&mut routed, &chain(2, 25, 400), now);
        assert_eq!(observed.outcome, "fits");
        assert_eq!(routed.candidates[0], 0);
    }

    #[test]
    fn short_prompts_are_never_moved() {
        let sessions = sessions(2);
        let now = Instant::now();
        sessions.served(0, &chain(1, 25, 700), Some(350_000), Some(0), now);
        let mut routed = decision(&[0, 1], &[true, true]);
        assert_eq!(
            sessions.place(&mut routed, &chain(2, 25, 40), now).outcome,
            "below"
        );
        assert_eq!(routed.candidates[0], 0);
    }

    #[test]
    fn an_unhealthy_or_ineligible_replica_is_never_the_alternative() {
        let sessions = sessions(3);
        let now = Instant::now();
        sessions.served(0, &chain(1, 25, 700), Some(350_000), Some(0), now);
        // Upstream 1 is unhealthy and upstream 2 serves another model.
        let mut routed = decision(&[0, 1, 2], &[true, false, true]);
        routed.candidates.retain(|&upstream| upstream != 2);
        let observed = sessions.place(&mut routed, &chain(2, 25, 700), now);
        assert_eq!(observed.outcome, "all_crowded");
        assert_eq!(routed.candidates[0], 0);
    }

    #[test]
    fn capacity_eviction_forgets_the_least_recent_conversation() {
        let sessions = sessions(1);
        let start = Instant::now();
        let a = chain(1, 25, 700);
        let b = chain(2, 25, 700);
        assert!(
            sessions
                .served(0, &a, Some(350_000), Some(0), start)
                .is_empty()
        );
        let evicted = sessions.served(
            0,
            &b,
            Some(350_000),
            Some(0),
            start + Duration::from_secs(5),
        );
        assert_eq!(evicted.len(), 1);
        assert_eq!(evicted[0].fingerprints, a);
        assert_eq!(evicted[0].served_before, Some(start));
        assert_eq!(sessions.resident_tokens(), vec![350_000]);
    }

    #[test]
    fn a_miss_on_a_resident_conversation_evicts_everything_older() {
        let sessions = LongSessions::new(
            LongSessionConfig {
                threshold_tokens: 100_000,
                // Optimistic capacity: the model believes all three fit.
                capacity_tokens: vec![Some(2_000_000)],
                ttl: Duration::from_mins(30),
            },
            1,
            1024,
        );
        let start = Instant::now();
        let at = |seconds| start + Duration::from_secs(seconds);
        let a = chain(1, 25, 600);
        let b = chain(2, 25, 600);
        let c = chain(3, 25, 600);
        sessions.served(0, &b, Some(300_000), Some(0), at(0));
        sessions.served(0, &a, Some(300_000), Some(0), at(1));
        sessions.served(0, &c, Some(300_000), Some(0), at(2));
        // A's next turn hit only its system prompt: B (older than A's last
        // turn) is gone; C (newer) is kept.
        let evicted = sessions.served(0, &chain(1, 25, 610), Some(305_000), Some(12_800), at(3));
        assert_eq!(evicted.len(), 1);
        assert_eq!(evicted[0].fingerprints, b);
        assert_eq!(sessions.resident_tokens(), vec![605_000]);
    }

    #[test]
    fn a_conversation_that_moved_no_longer_crowds_its_old_replica() {
        let sessions = sessions(2);
        let now = Instant::now();
        let a = chain(1, 25, 700);
        sessions.served(0, &a, Some(350_000), Some(0), now);
        sessions.served(1, &chain(1, 25, 710), Some(355_000), Some(0), now);
        assert_eq!(sessions.resident_tokens(), vec![0, 355_000]);
    }

    #[test]
    fn idle_conversations_expire() {
        let sessions = sessions(2);
        let start = Instant::now();
        sessions.served(0, &chain(1, 25, 700), Some(350_000), Some(0), start);
        let mut routed = decision(&[0, 1], &[true, true]);
        let later = start + Duration::from_secs(1801);
        assert_eq!(
            sessions
                .place(&mut routed, &chain(2, 25, 700), later)
                .outcome,
            "fits"
        );
        assert_eq!(routed.candidates[0], 0);
    }

    #[test]
    fn tokens_per_block_calibrates_from_untruncated_long_completions() {
        let sessions = sessions(2);
        let now = Instant::now();
        // 250 tokens per block observed: a 500-block prompt is ~125k, which
        // the default 512/block would have estimated as 256k.
        for id in 0..20 {
            sessions.served(1, &chain(100 + id, 0, 400), Some(100_000), Some(0), now);
        }
        let observed = sessions.place(
            &mut decision(&[0, 1], &[true, true]),
            &chain(1, 0, 500),
            now,
        );
        let estimated = observed.estimated_ktokens;
        assert!((125..140).contains(&estimated), "{estimated}");
    }

    #[test]
    fn missing_cache_telemetry_never_evicts_residents() {
        let sessions = LongSessions::new(
            LongSessionConfig {
                threshold_tokens: 100_000,
                capacity_tokens: vec![Some(2_000_000)],
                ttl: Duration::from_mins(30),
            },
            1,
            1024,
        );
        let start = Instant::now();
        let at = |seconds| start + Duration::from_secs(seconds);
        let a = chain(1, 25, 600);
        let b = chain(2, 25, 600);
        sessions.served(0, &b, Some(300_000), None, at(0));
        sessions.served(0, &a, Some(300_000), None, at(1));
        // The engine omits cached_tokens on A's next turn: not a miss.
        assert!(
            sessions
                .served(0, &chain(1, 25, 610), Some(305_000), None, at(2))
                .is_empty()
        );
        assert_eq!(sessions.resident_tokens(), vec![605_000]);
    }

    #[test]
    fn zero_cached_tokens_from_an_engine_without_cache_telemetry_is_not_a_miss() {
        let sessions = LongSessions::new(
            LongSessionConfig {
                threshold_tokens: 100_000,
                capacity_tokens: vec![Some(2_000_000); 2],
                ttl: Duration::from_mins(30),
            },
            2,
            1024,
        );
        let start = Instant::now();
        let at = |seconds| start + Duration::from_secs(seconds);
        let a = chain(1, 25, 600);
        let b = chain(2, 25, 600);
        sessions.served(0, &b, Some(300_000), Some(0), at(0));
        sessions.served(0, &a, Some(300_000), Some(0), at(1));
        // Upstream 0 has never reported a hit; a zero is untrusted.
        assert!(
            sessions
                .served(0, &chain(1, 25, 610), Some(305_000), Some(0), at(2))
                .is_empty()
        );
        assert_eq!(sessions.resident_tokens(), vec![605_000, 0]);

        // Another upstream's hits do not make upstream 0's zeros trusted.
        sessions.served(1, &chain(3, 25, 400), Some(200_000), Some(50_000), at(3));
        assert!(
            sessions
                .served(0, &chain(1, 25, 620), Some(310_000), Some(0), at(4))
                .is_empty()
        );

        // Once upstream 0 reports a hit, a later zero is a real miss.
        sessions.served(0, &chain(1, 25, 630), Some(315_000), Some(300_000), at(5));
        let c = chain(4, 25, 600);
        sessions.served(0, &c, Some(300_000), Some(0), at(6));
        let evicted = sessions.served(0, &chain(1, 25, 640), Some(320_000), Some(0), at(7));
        assert_eq!(evicted.len(), 1);
        assert_eq!(evicted[0].fingerprints, b);
    }

    #[test]
    fn a_shorter_prompt_sharing_a_prefix_is_not_the_conversation() {
        let sessions = sessions(2);
        let now = Instant::now();
        let a = chain(1, 25, 700);
        sessions.served(0, &a, Some(350_000), Some(0), now);
        // A sub-agent that forks the first 400 blocks of A is a shorter,
        // distinct prompt: it must not inherit A's 350k size.
        let fork = a[..400].to_vec();
        let mut routed = decision(&[0, 1], &[true, true]);
        let observed = sessions.place(&mut routed, &fork, now);
        // 400 blocks at A's calibrated 500 tokens per block.
        assert_eq!(observed.estimated_ktokens, 200);
        // Nor may serving it replace A's record.
        sessions.served(0, &fork, Some(100_000), Some(0), now);
        assert_eq!(sessions.resident_tokens(), vec![450_000, 0]);
    }

    #[test]
    fn a_shared_system_prompt_does_not_alias_conversations() {
        let sessions = sessions(2);
        let now = Instant::now();
        // Two conversations sharing a 400-block system prompt, then
        // diverging; under the old half-shared rule they were one.
        let a = chain(1, 400, 700);
        let b = chain(2, 400, 720);
        sessions.served(0, &a, Some(350_000), Some(0), now);
        let mut routed = decision(&[0, 1], &[true, true]);
        assert_eq!(sessions.place(&mut routed, &b, now).outcome, "moved");
        assert_eq!(routed.candidates[0], 1);
        sessions.served(1, &b, Some(360_000), Some(0), now);
        // Serving B elsewhere does not remove A from its own replica.
        assert_eq!(sessions.resident_tokens(), vec![350_000, 360_000]);
    }

    #[test]
    fn truncated_chains_alias_only_when_they_agree_on_the_cap() {
        let sessions = LongSessions::new(
            LongSessionConfig {
                threshold_tokens: 100_000,
                capacity_tokens: vec![Some(2_000_000); 2],
                ttl: Duration::from_mins(30),
            },
            2,
            1024,
        );
        let now = Instant::now();
        // Both chains reach the 1024-block cap and share the first 600.
        let a = chain(1, 600, 1024);
        let b = chain(2, 600, 1024);
        sessions.served(0, &a, Some(600_000), Some(0), now);
        sessions.served(0, &b, Some(650_000), Some(0), now);
        assert_eq!(sessions.resident_tokens(), vec![1_250_000, 0]);
        // A's next turn is the same truncated chain.
        sessions.served(0, &a, Some(610_000), Some(0), now);
        assert_eq!(sessions.resident_tokens(), vec![1_260_000, 0]);
    }

    #[test]
    fn a_continuation_tolerates_a_rewritten_tail() {
        let sessions = sessions(1);
        let now = Instant::now();
        let a = chain(1, 25, 700);
        sessions.served(0, &a, Some(350_000), Some(0), now);
        // The next turn rewrites the recorded turn's last block.
        let mut next = a[..699].to_vec();
        next.extend(chain(9, 0, 30));
        sessions.served(0, &next, Some(365_000), Some(0), now);
        assert_eq!(sessions.resident_tokens(), vec![365_000]);
    }

    #[test]
    fn every_duplicate_record_of_a_conversation_is_replaced() {
        let sessions = LongSessions::new(
            LongSessionConfig {
                threshold_tokens: 100_000,
                capacity_tokens: vec![Some(5_000_000)],
                ttl: Duration::from_mins(30),
            },
            1,
            1024,
        );
        let start = Instant::now();
        let at = |seconds| start + Duration::from_secs(seconds);
        let a = chain(1, 25, 600);
        // Inject two records of A, as an earlier alias could leave.
        {
            let mut state = sessions.state.lock();
            for (tokens, seconds) in [(300_000, 0), (290_000, 1)] {
                state.residents[0].push(Resident {
                    fingerprints: a.clone(),
                    tokens,
                    last_served: at(seconds),
                });
            }
        }
        assert_eq!(sessions.resident_tokens(), vec![590_000]);
        sessions.served(0, &chain(1, 25, 610), Some(305_000), Some(300_000), at(2));
        assert_eq!(sessions.resident_tokens(), vec![305_000]);
        assert_eq!(sessions.state.lock().residents[0].len(), 1);
    }

    #[test]
    fn served_prunes_expired_residents() {
        let sessions = sessions(1);
        let start = Instant::now();
        sessions.served(0, &chain(1, 25, 700), Some(350_000), Some(0), start);
        let later = start + Duration::from_secs(1801);
        // A expired: B fits beside nothing, so neither capacity eviction nor
        // a stale total reports A.
        let evicted = sessions.served(0, &chain(2, 25, 700), Some(350_000), Some(0), later);
        assert!(evicted.is_empty());
        assert_eq!(sessions.resident_tokens(), vec![350_000]);
    }

    #[test]
    fn tokens_per_block_is_calibrated_per_upstream() {
        let sessions = sessions(3);
        let now = Instant::now();
        // Upstream 1 serves a model at 250 tokens per block; upstreams 0 and
        // 2 serve another model that never completed a long prompt.
        for id in 0..20 {
            sessions.served(1, &chain(100 + id, 0, 400), Some(100_000), Some(0), now);
        }
        let other_model = sessions.place(
            &mut decision(&[0, 2], &[true, true, true]),
            &chain(1, 0, 500),
            now,
        );
        assert_eq!(other_model.estimated_ktokens, 256);
        let same_model = sessions.place(
            &mut decision(&[1], &[true, true, true]),
            &chain(2, 0, 500),
            now,
        );
        assert_eq!(same_model.estimated_ktokens, 125);
    }
}
