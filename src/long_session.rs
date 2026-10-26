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
//! A conversation is identified by its fingerprint chain: a later turn whose
//! chain shares at least half of a recorded chain is the same conversation.
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
    fn continued_by(&self, fingerprints: &[u64]) -> bool {
        let shared = self
            .fingerprints
            .iter()
            .zip(fingerprints)
            .take_while(|(left, right)| left == right)
            .count();
        !self.fingerprints.is_empty() && shared * 2 >= self.fingerprints.len()
    }
}

struct State {
    residents: Vec<Vec<Resident>>,
    tokens_per_block: f64,
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
                tokens_per_block: DEFAULT_TOKENS_PER_BLOCK,
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
        let ttl = self.config.ttl;
        for residents in &mut state.residents {
            residents.retain(|resident| now.saturating_duration_since(resident.last_served) <= ttl);
        }
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
        let estimated = recorded.max((fingerprints.len() as f64 * state.tokens_per_block) as u64);
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
        if fingerprints.len() < self.max_blocks {
            #[allow(clippy::cast_precision_loss)]
            let observed = prompt as f64 / fingerprints.len() as f64;
            if prompt >= self.config.threshold_tokens && observed.is_finite() && observed > 0.0 {
                state.tokens_per_block = 0.8f64.mul_add(state.tokens_per_block, 0.2 * observed);
            }
        }
        if prompt < self.config.threshold_tokens {
            return Vec::new();
        }
        let mut evicted = Vec::new();
        // The conversation now lives here; a copy elsewhere is the LRU's to
        // drop and must not crowd out the replica it left.
        for (index, residents) in state.residents.iter_mut().enumerate() {
            if index != upstream {
                residents.retain(|resident| !resident.continued_by(fingerprints));
            }
        }
        let residents = &mut state.residents[upstream];
        let previous = residents
            .iter()
            .position(|resident| resident.continued_by(fingerprints))
            .map(|position| residents.remove(position));
        // A miss for a conversation the model held resident means the replica
        // churned more than the model saw: everything older than its last turn
        // is gone too.
        if let Some(previous) = &previous
            && cached_tokens.unwrap_or(0).saturating_mul(2) < previous.tokens
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
}
