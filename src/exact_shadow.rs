//! Observation-only comparison between approximate routing and exact KV state.
//!
//! The selected engine is scored with its response-reported pre-request cache
//! hit. Alternative engines are queried only if their fenced inventory has not
//! changed since the approximate decision. This prevents the completed request
//! from teaching the shadow scorer the answer after the fact.

use std::{sync::Arc, time::Instant};

use crate::{
    kv_consumer::SharedFencedInventory,
    metrics::Metrics,
    router::{CandidateState, Decision, Outcome},
    shims::Endpoint,
};

#[derive(Clone)]
pub struct ExactRouteShadow {
    inventories: Arc<[SharedFencedInventory]>,
    metrics: Arc<Metrics>,
    alpha: f64,
    max_overlap_units: usize,
}

#[derive(Clone, Debug)]
pub struct ExactRouteSnapshot {
    decision: Decision,
    markers: Vec<Option<InventoryMarker>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExactPlacementPolicy {
    pub min_gain_tokens: usize,
    pub max_load_delta: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExactPlacementMode {
    Shadow,
    Placement,
}

impl ExactPlacementMode {
    const fn label(self) -> &'static str {
        match self {
            Self::Shadow => "shadow",
            Self::Placement => "placement",
        }
    }

    const fn applies(self) -> bool {
        matches!(self, Self::Placement)
    }
}

#[derive(Clone, Debug)]
struct PreRouteResult {
    shadow: ShadowResult,
    overlaps: Vec<Option<usize>>,
    winner: Option<usize>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PlacementOutcome {
    Move(usize),
    KeptAgree,
    KeptTie,
    KeptAllZero,
    KeptAmbiguous,
    KeptGainGate,
    KeptLoadGate,
    Fallback,
}

impl PlacementOutcome {
    const fn label(self, active: bool) -> &'static str {
        match self {
            Self::Move(_) if active => "moved",
            Self::Move(_) => "would_move",
            Self::KeptAgree => "kept_agree",
            Self::KeptTie => "kept_tie",
            Self::KeptAllZero => "kept_all_zero",
            Self::KeptAmbiguous => "kept_ambiguous",
            Self::KeptGainGate => "kept_gain_gate",
            Self::KeptLoadGate => "kept_load_gate",
            Self::Fallback => "fallback",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct InventoryMarker {
    generation: u64,
    revision: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ShadowOutcome {
    Agree,
    WouldMove,
    Tie,
    AllZero,
    Single,
    NoInventories,
    CandidateMismatch,
    Failover,
    MissingCachedUsage,
    InvalidCachedUsage,
    InventoryUntrusted,
    InventoryChanged,
    LookupError,
}

impl ShadowOutcome {
    const fn label(self) -> &'static str {
        match self {
            Self::Agree => "agree",
            Self::WouldMove => "would_move",
            Self::Tie => "tie",
            Self::AllZero => "all_zero",
            Self::Single => "single",
            Self::NoInventories => "no_inventories",
            Self::CandidateMismatch => "candidate_mismatch",
            Self::Failover => "failover",
            Self::MissingCachedUsage => "missing_cached_usage",
            Self::InvalidCachedUsage => "invalid_cached_usage",
            Self::InventoryUntrusted => "inventory_untrusted",
            Self::InventoryChanged => "inventory_changed",
            Self::LookupError => "lookup_error",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ShadowResult {
    outcome: ShadowOutcome,
    selected_tokens: usize,
    best_tokens: usize,
}

impl ExactRouteShadow {
    #[must_use]
    pub fn new(
        inventories: Arc<[SharedFencedInventory]>,
        metrics: Arc<Metrics>,
        alpha: f64,
        max_overlap_units: usize,
    ) -> Self {
        Self {
            inventories,
            metrics,
            alpha,
            max_overlap_units,
        }
    }

    /// Whether every configured inventory is currently authoritative.
    #[must_use]
    pub fn ready(&self) -> bool {
        !self.inventories.is_empty()
            && self
                .inventories
                .iter()
                .all(|inventory| inventory.read().trusted())
    }

    /// Captures the exact inventory generation visible at approximate routing
    /// time without retaining any locks across request I/O.
    #[must_use]
    pub fn capture(&self, decision: &Decision) -> ExactRouteSnapshot {
        let markers = self
            .inventories
            .iter()
            .map(|inventory| {
                let inventory = inventory.read();
                inventory.trusted().then(|| InventoryMarker {
                    generation: inventory.generation(),
                    revision: inventory.revision(),
                })
            })
            .collect();
        ExactRouteSnapshot {
            decision: decision.clone(),
            markers,
        }
    }

    /// Records one exact-token counterfactual. No result is returned to, or
    /// consulted by, the request router.
    pub fn observe(
        &self,
        backend: &str,
        endpoint: Endpoint,
        routed_upstream: usize,
        cached_tokens: Option<usize>,
        token_ids: &[u32],
        snapshot: &ExactRouteSnapshot,
    ) {
        let result = self.evaluate(routed_upstream, cached_tokens, token_ids, snapshot);
        self.metrics
            .exact_route_shadow
            .with_label_values(&[backend, endpoint.label(), result.outcome.label()])
            .inc();
        self.record_result(endpoint, &result);
    }

    /// Compare the approximate decision with one revision-stable exact lookup
    /// before the selected engine can mutate its cache. Placement remains
    /// unchanged in shadow mode while the same conservative placement policy
    /// is still evaluated for telemetry.
    pub fn route_pre_route(
        &self,
        endpoint: Endpoint,
        token_ids: &[u32],
        decision: &mut Decision,
        policy: ExactPlacementPolicy,
        mode: ExactPlacementMode,
    ) {
        let started = Instant::now();
        let result = self.evaluate_pre_route(token_ids, decision);
        self.metrics
            .exact_route_preroute_duration
            .with_label_values(&[endpoint.label(), "lookup"])
            .observe(started.elapsed().as_secs_f64());
        self.metrics
            .exact_route_preroute
            .with_label_values(&[endpoint.label(), result.shadow.outcome.label()])
            .inc();
        self.record_result(endpoint, &result.shadow);
        let placement = evaluate_placement(decision, &result, policy);
        if mode.applies()
            && let PlacementOutcome::Move(winner) = placement
        {
            apply_placement(
                winner,
                token_ids.len(),
                decision,
                &result,
                self.max_overlap_units,
            );
        }
        self.metrics
            .exact_route_placement
            .with_label_values(&[
                mode.label(),
                endpoint.label(),
                placement.label(mode.applies()),
            ])
            .inc();
    }

    fn record_result(&self, endpoint: Endpoint, result: &ShadowResult) {
        if matches!(
            result.outcome,
            ShadowOutcome::Agree
                | ShadowOutcome::WouldMove
                | ShadowOutcome::Tie
                | ShadowOutcome::AllZero
        ) {
            self.metrics
                .exact_route_overlap
                .with_label_values(&[endpoint.label(), "selected"])
                .observe(usize_to_f64(result.selected_tokens));
            self.metrics
                .exact_route_overlap
                .with_label_values(&[endpoint.label(), "best"])
                .observe(usize_to_f64(result.best_tokens));
            self.metrics
                .exact_route_gain
                .with_label_values(&[endpoint.label()])
                .observe(usize_to_f64(
                    result.best_tokens.saturating_sub(result.selected_tokens),
                ));
        }
    }

    fn evaluate_pre_route(&self, token_ids: &[u32], decision: &Decision) -> PreRouteResult {
        let failure = |outcome| PreRouteResult {
            shadow: ShadowResult {
                outcome,
                selected_tokens: 0,
                best_tokens: 0,
            },
            overlaps: Vec::new(),
            winner: None,
        };
        if self.inventories.is_empty() {
            return failure(ShadowOutcome::NoInventories);
        }
        let Some(&selected) = decision.candidates.first() else {
            return failure(ShadowOutcome::CandidateMismatch);
        };
        if decision.candidate_state.len() != self.inventories.len() {
            return failure(ShadowOutcome::CandidateMismatch);
        }
        let markers = self
            .inventories
            .iter()
            .map(|inventory| {
                let inventory = inventory.read();
                inventory.trusted().then(|| InventoryMarker {
                    generation: inventory.generation(),
                    revision: inventory.revision(),
                })
            })
            .collect::<Vec<_>>();
        let mut seen = vec![false; self.inventories.len()];
        let mut overlaps = vec![None; self.inventories.len()];
        for candidate in &decision.candidate_state {
            if candidate.index >= self.inventories.len() || seen[candidate.index] {
                return failure(ShadowOutcome::CandidateMismatch);
            }
            seen[candidate.index] = true;
            if !candidate.healthy {
                continue;
            }
            let Some(marker) = markers[candidate.index] else {
                return failure(ShadowOutcome::InventoryUntrusted);
            };
            let inventory = self.inventories[candidate.index].read();
            if !inventory.trusted() {
                return failure(ShadowOutcome::InventoryUntrusted);
            }
            if inventory.generation() != marker.generation
                || inventory.revision() != marker.revision
            {
                return failure(ShadowOutcome::InventoryChanged);
            }
            let Ok(exact_match) = inventory.find_longest(token_ids) else {
                return failure(ShadowOutcome::LookupError);
            };
            let Some(exact_match) = exact_match else {
                return failure(ShadowOutcome::InventoryUntrusted);
            };
            overlaps[candidate.index] = Some(exact_match.token_ids);
        }
        if seen.iter().any(|seen| !seen) {
            return failure(ShadowOutcome::CandidateMismatch);
        }
        for candidate in decision
            .candidate_state
            .iter()
            .filter(|candidate| candidate.healthy)
        {
            let Some(marker) = markers[candidate.index] else {
                return failure(ShadowOutcome::InventoryUntrusted);
            };
            let inventory = self.inventories[candidate.index].read();
            if !inventory.trusted() {
                return failure(ShadowOutcome::InventoryUntrusted);
            }
            if inventory.generation() != marker.generation
                || inventory.revision() != marker.revision
            {
                return failure(ShadowOutcome::InventoryChanged);
            }
        }
        let shadow = classify(
            selected,
            token_ids.len(),
            &overlaps,
            decision,
            self.alpha,
            self.max_overlap_units,
        );
        let winner = unique_winner(
            token_ids.len(),
            &overlaps,
            decision,
            self.alpha,
            self.max_overlap_units,
        );
        PreRouteResult {
            shadow,
            overlaps,
            winner,
        }
    }

    fn evaluate(
        &self,
        routed_upstream: usize,
        cached_tokens: Option<usize>,
        token_ids: &[u32],
        snapshot: &ExactRouteSnapshot,
    ) -> ShadowResult {
        let failure = |outcome| ShadowResult {
            outcome,
            selected_tokens: 0,
            best_tokens: 0,
        };
        if self.inventories.is_empty() {
            return failure(ShadowOutcome::NoInventories);
        }
        let Some(&selected) = snapshot.decision.candidates.first() else {
            return failure(ShadowOutcome::CandidateMismatch);
        };
        if routed_upstream != selected {
            return failure(ShadowOutcome::Failover);
        }
        let Some(selected_tokens) = cached_tokens else {
            return failure(ShadowOutcome::MissingCachedUsage);
        };
        if selected_tokens > token_ids.len() {
            return failure(ShadowOutcome::InvalidCachedUsage);
        }
        if snapshot.markers.len() != self.inventories.len()
            || snapshot.decision.candidate_state.len() != self.inventories.len()
        {
            return failure(ShadowOutcome::CandidateMismatch);
        }

        let mut seen = vec![false; self.inventories.len()];
        let mut overlaps = vec![None; self.inventories.len()];
        for candidate in &snapshot.decision.candidate_state {
            if candidate.index >= self.inventories.len() || seen[candidate.index] {
                return failure(ShadowOutcome::CandidateMismatch);
            }
            seen[candidate.index] = true;
            if !candidate.healthy {
                continue;
            }
            let Some(marker) = snapshot.markers[candidate.index] else {
                return failure(ShadowOutcome::InventoryUntrusted);
            };
            if candidate.index == selected {
                overlaps[candidate.index] = Some(selected_tokens);
                continue;
            }
            let inventory = self.inventories[candidate.index].read();
            if !inventory.trusted() {
                return failure(ShadowOutcome::InventoryUntrusted);
            }
            if inventory.generation() != marker.generation
                || inventory.revision() != marker.revision
            {
                return failure(ShadowOutcome::InventoryChanged);
            }
            let Ok(exact_match) = inventory.find_longest(token_ids) else {
                return failure(ShadowOutcome::LookupError);
            };
            let Some(exact_match) = exact_match else {
                return failure(ShadowOutcome::InventoryUntrusted);
            };
            overlaps[candidate.index] = Some(exact_match.token_ids);
        }
        if seen.iter().any(|seen| !seen) {
            return failure(ShadowOutcome::CandidateMismatch);
        }

        classify(
            selected,
            token_ids.len(),
            &overlaps,
            &snapshot.decision,
            self.alpha,
            self.max_overlap_units,
        )
    }
}

fn classify(
    selected: usize,
    prompt_tokens: usize,
    overlaps: &[Option<usize>],
    decision: &Decision,
    alpha: f64,
    max_overlap_units: usize,
) -> ShadowResult {
    let eligible = decision
        .candidate_state
        .iter()
        .filter(|candidate| candidate.healthy)
        .filter_map(|candidate| {
            overlaps
                .get(candidate.index)
                .copied()
                .flatten()
                .map(|tokens| (candidate, tokens))
        })
        .collect::<Vec<_>>();
    let selected_tokens = overlaps.get(selected).copied().flatten().unwrap_or(0);
    let best_tokens = eligible
        .iter()
        .map(|(_, tokens)| *tokens)
        .max()
        .unwrap_or(0);
    if eligible.len() <= 1 {
        return ShadowResult {
            outcome: ShadowOutcome::Single,
            selected_tokens,
            best_tokens,
        };
    }
    if best_tokens == 0 {
        return ShadowResult {
            outcome: ShadowOutcome::AllZero,
            selected_tokens,
            best_tokens,
        };
    }
    let weighted = |candidate: &CandidateState, tokens: usize| {
        #[allow(clippy::cast_precision_loss)]
        let affinity = overlap_units(tokens, prompt_tokens, decision.total_blocks)
            .min(max_overlap_units) as f64;
        #[allow(clippy::cast_precision_loss)]
        let load = candidate.load_units as f64;
        affinity - alpha * load
    };
    let best_score = eligible
        .iter()
        .map(|(candidate, tokens)| weighted(candidate, *tokens))
        .fold(f64::NEG_INFINITY, f64::max);
    let Some((selected_candidate, _)) = eligible
        .iter()
        .find(|(candidate, _)| candidate.index == selected)
    else {
        return ShadowResult {
            outcome: ShadowOutcome::CandidateMismatch,
            selected_tokens,
            best_tokens,
        };
    };
    let selected_score = weighted(selected_candidate, selected_tokens);
    #[allow(clippy::float_cmp)] // Exact equality mirrors the live router policy.
    let tied = eligible
        .iter()
        .filter(|(candidate, tokens)| weighted(candidate, *tokens) == best_score)
        .count();
    #[allow(clippy::float_cmp)]
    let outcome = if selected_score != best_score {
        ShadowOutcome::WouldMove
    } else if tied > 1 {
        ShadowOutcome::Tie
    } else {
        ShadowOutcome::Agree
    };
    ShadowResult {
        outcome,
        selected_tokens,
        best_tokens,
    }
}

fn unique_winner(
    prompt_tokens: usize,
    overlaps: &[Option<usize>],
    decision: &Decision,
    alpha: f64,
    max_overlap_units: usize,
) -> Option<usize> {
    let eligible = decision
        .candidate_state
        .iter()
        .filter(|candidate| candidate.healthy)
        .filter_map(|candidate| {
            overlaps
                .get(candidate.index)
                .copied()
                .flatten()
                .map(|tokens| (candidate, tokens))
        })
        .collect::<Vec<_>>();
    let best_score = eligible
        .iter()
        .map(|(candidate, tokens)| {
            exact_score(
                candidate,
                *tokens,
                prompt_tokens,
                decision.total_blocks,
                alpha,
                max_overlap_units,
            )
        })
        .fold(f64::NEG_INFINITY, f64::max);
    let best_tokens = eligible
        .iter()
        .filter(|(candidate, tokens)| {
            #[allow(clippy::float_cmp)]
            {
                exact_score(
                    candidate,
                    *tokens,
                    prompt_tokens,
                    decision.total_blocks,
                    alpha,
                    max_overlap_units,
                ) == best_score
            }
        })
        .map(|(_, tokens)| *tokens)
        .max()?;
    let mut winners = eligible.iter().filter(|(candidate, tokens)| {
        #[allow(clippy::float_cmp)]
        {
            exact_score(
                candidate,
                *tokens,
                prompt_tokens,
                decision.total_blocks,
                alpha,
                max_overlap_units,
            ) == best_score
                && *tokens == best_tokens
        }
    });
    let winner = winners.next()?.0.index;
    winners.next().is_none().then_some(winner)
}

fn exact_score(
    candidate: &CandidateState,
    tokens: usize,
    prompt_tokens: usize,
    total_units: usize,
    alpha: f64,
    max_overlap_units: usize,
) -> f64 {
    #[allow(clippy::cast_precision_loss)]
    let affinity = overlap_units(tokens, prompt_tokens, total_units).min(max_overlap_units) as f64;
    #[allow(clippy::cast_precision_loss)]
    let load = candidate.load_units as f64;
    affinity - alpha * load
}

fn evaluate_placement(
    decision: &Decision,
    exact: &PreRouteResult,
    policy: ExactPlacementPolicy,
) -> PlacementOutcome {
    if exact.shadow.outcome != ShadowOutcome::WouldMove {
        return match exact.shadow.outcome {
            ShadowOutcome::Agree => PlacementOutcome::KeptAgree,
            ShadowOutcome::Tie => PlacementOutcome::KeptTie,
            ShadowOutcome::AllZero => PlacementOutcome::KeptAllZero,
            _ => PlacementOutcome::Fallback,
        };
    }
    let Some(selected) = decision.candidates.first().copied() else {
        return PlacementOutcome::Fallback;
    };
    let Some(winner) = exact.winner else {
        return PlacementOutcome::KeptAmbiguous;
    };
    if winner == selected {
        return PlacementOutcome::KeptAgree;
    }
    let selected_tokens = exact.overlaps.get(selected).copied().flatten().unwrap_or(0);
    let winner_tokens = exact.overlaps.get(winner).copied().flatten().unwrap_or(0);
    if winner_tokens.saturating_sub(selected_tokens) < policy.min_gain_tokens {
        return PlacementOutcome::KeptGainGate;
    }
    let Some(selected_state) = decision
        .candidate_state
        .iter()
        .find(|candidate| candidate.index == selected)
    else {
        return PlacementOutcome::Fallback;
    };
    let Some(winner_state) = decision
        .candidate_state
        .iter()
        .find(|candidate| candidate.index == winner)
    else {
        return PlacementOutcome::Fallback;
    };
    if winner_state.load_units
        > selected_state
            .load_units
            .saturating_add(policy.max_load_delta)
    {
        return PlacementOutcome::KeptLoadGate;
    }

    PlacementOutcome::Move(winner)
}

fn apply_placement(
    winner: usize,
    prompt_tokens: usize,
    decision: &mut Decision,
    exact: &PreRouteResult,
    max_overlap_units: usize,
) {
    decision.candidates.retain(|candidate| *candidate != winner);
    decision.candidates.insert(0, winner);
    let candidate_order = decision.candidates.clone();
    decision.candidate_state.sort_by_key(|candidate| {
        candidate_order
            .iter()
            .position(|index| *index == candidate.index)
            .unwrap_or(usize::MAX)
    });
    for (rank, candidate) in decision.candidate_state.iter_mut().enumerate() {
        candidate.rank = rank;
        let exact_tokens = exact
            .overlaps
            .get(candidate.index)
            .copied()
            .flatten()
            .unwrap_or(0);
        candidate.overlap_blocks =
            overlap_units(exact_tokens, prompt_tokens, decision.total_blocks);
        candidate.affinity_blocks = candidate.overlap_blocks.min(max_overlap_units);
    }
    let winner_state = &mut decision.candidate_state[0];
    decision.overlap_blocks = winner_state.overlap_blocks;
    decision.affinity_blocks = winner_state.affinity_blocks;
    decision.load_units = winner_state.request_load_units;
    decision.outcome = Outcome::Exact;
}

fn overlap_units(tokens: usize, prompt_tokens: usize, total_units: usize) -> usize {
    if tokens == 0 || prompt_tokens == 0 || total_units == 0 {
        return 0;
    }
    let numerator = (tokens as u128).saturating_mul(total_units as u128);
    let units = numerator.div_ceil(prompt_tokens as u128);
    usize::try_from(units).unwrap_or(usize::MAX)
}

#[allow(clippy::cast_precision_loss)]
fn usize_to_f64(value: usize) -> f64 {
    value as f64
}

#[cfg(test)]
mod tests {
    use prometheus::Registry;

    use super::*;
    use crate::{
        exact_index::{ExactIndexLimits, FencedExactKvInventory},
        kv_wire::{BlockStored, ExternalBlockHash, KvEvent, KvEventBatch},
        router::{CandidateState, Outcome},
    };

    fn candidate(index: usize, rank: usize, load_units: usize) -> CandidateState {
        CandidateState {
            index,
            rank,
            overlap_blocks: 0,
            affinity_blocks: 0,
            load_units,
            request_load_units: 1,
            healthy: true,
        }
    }

    fn decision() -> Decision {
        Decision {
            candidates: vec![0, 1],
            candidate_state: vec![candidate(0, 0, 0), candidate(1, 1, 0)],
            overlap_blocks: 0,
            total_blocks: 8,
            affinity_blocks: 0,
            load_units: 1,
            rotation: 0,
            outcome: Outcome::RoundRobin,
        }
    }

    fn batch(events: Vec<KvEvent>) -> KvEventBatch {
        KvEventBatch {
            timestamp: 1.0,
            events,
            data_parallel_rank: Some(0),
        }
    }

    fn store(tokens: &[u32]) -> KvEvent {
        KvEvent::BlockStored(BlockStored {
            block_hashes: vec![ExternalBlockHash::Unsigned(7)],
            parent_block_hash: None,
            token_ids: tokens.to_vec(),
            block_size: tokens.len(),
            group_idx: Some(0),
            kv_cache_spec_kind: None,
            kv_cache_spec_sliding_window: None,
            medium: Some("GPU".to_owned()),
            locality: Some("LOCAL".to_owned()),
            lora_name: None,
            cache_namespace: None,
            has_extra_keys: false,
        })
    }

    fn trusted_inventory(events: Vec<KvEvent>) -> SharedFencedInventory {
        let mut inventory = FencedExactKvInventory::new(8, ExactIndexLimits::default());
        inventory.ingest_live(0, &batch(events)).unwrap();
        Arc::new(parking_lot::RwLock::new(inventory))
    }

    #[test]
    fn exact_overlap_replaces_only_the_cache_term() {
        let mut route = decision();
        route.candidate_state[1].load_units = 1;
        let result = classify(0, 8, &[Some(0), Some(8)], &route, 1.0, 8);
        assert_eq!(result.outcome, ShadowOutcome::WouldMove);
        assert_eq!(result.best_tokens, 8);

        route.candidate_state[1].load_units = 8;
        let result = classify(0, 8, &[Some(0), Some(8)], &route, 1.0, 8);
        assert_eq!(result.outcome, ShadowOutcome::Tie);
    }

    #[test]
    fn selected_engine_self_update_does_not_bias_the_comparison() {
        let selected = trusted_inventory(Vec::new());
        let alternative = trusted_inventory(vec![store(&[1, 2, 3, 4])]);
        let registry = Registry::new();
        let metrics = Arc::new(Metrics::new(&registry).unwrap());
        let shadow = ExactRouteShadow::new(
            Arc::from([selected.clone(), alternative]),
            Arc::clone(&metrics),
            1.0,
            8,
        );
        let snapshot = shadow.capture(&decision());

        selected
            .write()
            .ingest_live(1, &batch(vec![store(&[1, 2, 3, 4])]))
            .unwrap();
        shadow.observe(
            "remote",
            Endpoint::Chat,
            0,
            Some(0),
            &[1, 2, 3, 4],
            &snapshot,
        );
        assert!(
            (metrics
                .exact_route_shadow
                .with_label_values(&["remote", "chat", "would_move"])
                .get()
                - 1.0)
                .abs()
                < f64::EPSILON
        );
    }

    #[test]
    fn changing_alternative_inventory_fails_closed() {
        let selected = trusted_inventory(Vec::new());
        let alternative = trusted_inventory(Vec::new());
        let registry = Registry::new();
        let metrics = Arc::new(Metrics::new(&registry).unwrap());
        let shadow = ExactRouteShadow::new(
            Arc::from([selected, alternative.clone()]),
            Arc::clone(&metrics),
            1.0,
            8,
        );
        let snapshot = shadow.capture(&decision());
        alternative
            .write()
            .ingest_live(1, &batch(vec![store(&[1, 2])]))
            .unwrap();
        shadow.observe("remote", Endpoint::Chat, 0, Some(0), &[1, 2], &snapshot);
        assert!(
            (metrics
                .exact_route_shadow
                .with_label_values(&["remote", "chat", "inventory_changed"])
                .get()
                - 1.0)
                .abs()
                < f64::EPSILON
        );
    }

    #[test]
    fn pre_route_shadow_observes_both_inventories_before_mutation() {
        let selected = trusted_inventory(Vec::new());
        let alternative = trusted_inventory(vec![store(&[1, 2, 3, 4])]);
        let registry = Registry::new();
        let metrics = Arc::new(Metrics::new(&registry).unwrap());
        let shadow = ExactRouteShadow::new(
            Arc::from([selected, alternative]),
            Arc::clone(&metrics),
            1.0,
            8,
        );
        let mut route = decision();
        let original = route.clone();
        shadow.route_pre_route(
            Endpoint::Chat,
            &[1, 2, 3, 4],
            &mut route,
            ExactPlacementPolicy {
                min_gain_tokens: 4,
                max_load_delta: 0,
            },
            ExactPlacementMode::Shadow,
        );
        assert_eq!(route, original);
        assert!(
            (metrics
                .exact_route_preroute
                .with_label_values(&["chat", "would_move"])
                .get()
                - 1.0)
                .abs()
                < f64::EPSILON
        );
        assert!(
            (metrics
                .exact_route_placement
                .with_label_values(&["shadow", "chat", "would_move"])
                .get()
                - 1.0)
                .abs()
                < f64::EPSILON
        );
    }

    #[test]
    fn placement_moves_only_for_a_unique_gated_exact_win() {
        let selected = trusted_inventory(Vec::new());
        let alternative = trusted_inventory(vec![store(&[1, 2, 3, 4])]);
        let metrics = Arc::new(Metrics::new(&Registry::new()).unwrap());
        let shadow = ExactRouteShadow::new(
            Arc::from([selected, alternative]),
            Arc::clone(&metrics),
            1.0,
            8,
        );
        let mut route = decision();
        shadow.route_pre_route(
            Endpoint::Chat,
            &[1, 2, 3, 4],
            &mut route,
            ExactPlacementPolicy {
                min_gain_tokens: 4,
                max_load_delta: 0,
            },
            ExactPlacementMode::Placement,
        );
        assert_eq!(route.candidates, [1, 0]);
        assert_eq!(route.candidate_state[0].index, 1);
        assert_eq!(route.candidate_state[0].rank, 0);
        assert_eq!(route.outcome, Outcome::Exact);
        assert!(
            (metrics
                .exact_route_placement
                .with_label_values(&["placement", "chat", "moved"])
                .get()
                - 1.0)
                .abs()
                < f64::EPSILON
        );
    }

    #[test]
    fn placement_gain_and_load_gates_keep_the_approximate_choice() {
        let selected = trusted_inventory(Vec::new());
        let alternative = trusted_inventory(vec![store(&[1, 2, 3, 4])]);
        let metrics = Arc::new(Metrics::new(&Registry::new()).unwrap());
        let shadow = ExactRouteShadow::new(
            Arc::from([selected, alternative]),
            Arc::clone(&metrics),
            1.0,
            8,
        );
        let mut gain_gated = decision();
        shadow.route_pre_route(
            Endpoint::Chat,
            &[1, 2, 3, 4],
            &mut gain_gated,
            ExactPlacementPolicy {
                min_gain_tokens: 5,
                max_load_delta: 0,
            },
            ExactPlacementMode::Placement,
        );
        assert_eq!(gain_gated.candidates, [0, 1]);

        let mut load_gated = decision();
        load_gated.candidate_state[1].load_units = 1;
        shadow.route_pre_route(
            Endpoint::Chat,
            &[1, 2, 3, 4],
            &mut load_gated,
            ExactPlacementPolicy {
                min_gain_tokens: 4,
                max_load_delta: 0,
            },
            ExactPlacementMode::Placement,
        );
        assert_eq!(load_gated.candidates, [0, 1]);
        assert!(
            (metrics
                .exact_route_placement
                .with_label_values(&["placement", "chat", "kept_gain_gate"])
                .get()
                - 1.0)
                .abs()
                < f64::EPSILON
        );
        assert!(
            (metrics
                .exact_route_placement
                .with_label_values(&["placement", "chat", "kept_load_gate"])
                .get()
                - 1.0)
                .abs()
                < f64::EPSILON
        );
    }

    #[test]
    fn shadow_policy_records_load_gate_without_changing_route() {
        let selected = trusted_inventory(Vec::new());
        let alternative = trusted_inventory(vec![store(&[1, 2, 3, 4])]);
        let metrics = Arc::new(Metrics::new(&Registry::new()).unwrap());
        let shadow = ExactRouteShadow::new(
            Arc::from([selected, alternative]),
            Arc::clone(&metrics),
            1.0,
            8,
        );
        let mut route = decision();
        route.candidate_state[1].load_units = 1;
        let original = route.clone();
        shadow.route_pre_route(
            Endpoint::Chat,
            &[1, 2, 3, 4],
            &mut route,
            ExactPlacementPolicy {
                min_gain_tokens: 4,
                max_load_delta: 0,
            },
            ExactPlacementMode::Shadow,
        );
        assert_eq!(route, original);
        assert!(
            (metrics
                .exact_route_placement
                .with_label_values(&["shadow", "chat", "kept_load_gate"])
                .get()
                - 1.0)
                .abs()
                < f64::EPSILON
        );
    }

    #[test]
    fn readiness_requires_every_inventory_to_be_authoritative() {
        let metrics = Arc::new(Metrics::new(&Registry::new()).unwrap());
        let trusted = trusted_inventory(Vec::new());
        let untrusted = Arc::new(parking_lot::RwLock::new(FencedExactKvInventory::new(
            8,
            ExactIndexLimits::default(),
        )));
        assert!(
            ExactRouteShadow::new(Arc::from([trusted.clone()]), Arc::clone(&metrics), 1.0, 8)
                .ready()
        );
        assert!(
            !ExactRouteShadow::new(
                Arc::from([trusted, untrusted]),
                Arc::clone(&metrics),
                1.0,
                8
            )
            .ready()
        );
    }
}
