//! Observation-only comparison between approximate routing and exact KV state.
//!
//! The selected engine is scored with its response-reported pre-request cache
//! hit. Alternative engines are queried only if their fenced inventory has not
//! changed since the approximate decision. This prevents the completed request
//! from teaching the shadow scorer the answer after the fact.

use std::{sync::Arc, time::Instant};

#[cfg(test)]
use crate::kv_consumer::SharedFencedInventory;
use crate::{
    config::AffinityBasis,
    exact_route_inventory::{ExactInventoryLookupError, ExactInventoryMarker, ExactRouteInventory},
    metrics::Metrics,
    router::{
        CandidateState, Decision, Outcome, PeerOverlap, RequestLoadEstimator, credited_affinity,
    },
    shims::Endpoint,
};

#[derive(Clone)]
pub struct ExactRouteShadow {
    inventories: Arc<[ExactRouteInventory]>,
    metrics: Arc<Metrics>,
    scoring: ExactScoring,
    load_estimator: RequestLoadEstimator,
}

/// The live router's scoring policy, re-applied to exact overlap.
///
/// Exact placement only replaces the cache term of the router's score, so
/// every other term must be the one the router used: the affinity basis and
/// its peer groups, the overlap cap, alpha, and whether the candidate's own
/// request load is projected onto its queue. Scoring with a different policy
/// would report disagreements that are artifacts of the comparison itself.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ExactScoring {
    pub(crate) alpha: f64,
    pub(crate) max_overlap_units: usize,
    pub(crate) affinity_basis: AffinityBasis,
    /// Peer group per upstream, as in `RouterConfig::affinity_groups`. Empty
    /// means one group.
    pub(crate) affinity_groups: Arc<[usize]>,
    pub(crate) projected_load: bool,
}

impl ExactScoring {
    /// Absolute affinity without projected load: the router's defaults.
    #[must_use]
    pub(crate) fn absolute(alpha: f64, max_overlap_units: usize) -> Self {
        Self {
            alpha,
            max_overlap_units,
            affinity_basis: AffinityBasis::Absolute,
            affinity_groups: Arc::from([]),
            projected_load: false,
        }
    }

    fn group(&self, index: usize) -> usize {
        self.affinity_groups.get(index).copied().unwrap_or(0)
    }

    /// Exact score per healthy candidate whose overlap is known, in
    /// `candidate_state` order: `(candidate, exact tokens, credited affinity
    /// units, weighted score)`.
    fn score<'a>(
        &self,
        prompt_tokens: usize,
        overlaps: &[Option<usize>],
        decision: &'a Decision,
    ) -> Vec<ExactCandidateScore<'a>> {
        let eligible = decision
            .candidate_state
            .iter()
            .filter(|candidate| candidate.healthy)
            .filter_map(|candidate| {
                overlaps
                    .get(candidate.index)
                    .copied()
                    .flatten()
                    .map(|tokens| {
                        (
                            candidate,
                            tokens,
                            overlap_units(tokens, prompt_tokens, decision.total_blocks),
                        )
                    })
            })
            .collect::<Vec<_>>();
        let mut peers: Vec<Option<PeerOverlap>> = Vec::new();
        if self.affinity_basis != AffinityBasis::Absolute {
            for (candidate, _, units) in &eligible {
                let key = self.group(candidate.index);
                if peers.len() <= key {
                    peers.resize(key + 1, None);
                }
                peers[key] = Some(peers[key].map_or(
                    PeerOverlap {
                        floor: *units,
                        lead: *units,
                    },
                    |known| PeerOverlap {
                        floor: known.floor.min(*units),
                        lead: known.lead.max(*units),
                    },
                ));
            }
        }
        eligible
            .into_iter()
            .map(|(candidate, tokens, units)| {
                let peer = peers
                    .get(self.group(candidate.index))
                    .copied()
                    .flatten()
                    .unwrap_or_default();
                let affinity =
                    credited_affinity(self.affinity_basis, units, peer, self.max_overlap_units);
                let additional_load = if self.projected_load {
                    candidate.request_load_units.saturating_sub(1)
                } else {
                    0
                };
                let load = candidate.load_units.saturating_add(additional_load);
                #[allow(clippy::cast_precision_loss)]
                let weighted = affinity as f64 - self.alpha * load as f64;
                ExactCandidateScore {
                    candidate,
                    tokens,
                    affinity,
                    weighted,
                }
            })
            .collect()
    }
}

#[derive(Clone, Copy, Debug)]
struct ExactCandidateScore<'a> {
    candidate: &'a CandidateState,
    tokens: usize,
    affinity: usize,
    weighted: f64,
}

/// The exact policy's preferred score: the highest weighted score, then the
/// most exact tokens among candidates at that score. This mirrors the live
/// router's ordering, which breaks a score tie on overlap before rotation, so
/// two replicas saturating the overlap cap at equal load are still separated
/// by whichever holds more of the prompt.
fn best_key(scores: &[ExactCandidateScore<'_>]) -> Option<(f64, usize)> {
    let best_score = scores
        .iter()
        .map(|score| score.weighted)
        .fold(f64::NEG_INFINITY, f64::max);
    #[allow(clippy::float_cmp)] // Exact equality mirrors the live router policy.
    let best_tokens = scores
        .iter()
        .filter(|score| score.weighted == best_score)
        .map(|score| score.tokens)
        .max()?;
    Some((best_score, best_tokens))
}

#[allow(clippy::float_cmp)] // Exact equality mirrors the live router policy.
fn is_best(score: &ExactCandidateScore<'_>, (best_score, best_tokens): (f64, usize)) -> bool {
    score.weighted == best_score && score.tokens == best_tokens
}

#[derive(Clone, Debug)]
pub struct ExactRouteSnapshot {
    decision: Decision,
    markers: Vec<Option<ExactInventoryMarker>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExactPlacementPolicy {
    pub min_gain_tokens: usize,
    pub max_load_delta: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExactPlacementMode {
    Shadow,
    Control,
    Placement,
}

impl ExactPlacementMode {
    const fn label(self) -> &'static str {
        match self {
            Self::Shadow => "shadow",
            Self::Control => "control",
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
    resident_tokens: Vec<Option<usize>>,
    prompt_tokens: usize,
    winner: Option<usize>,
}

/// Content-free result of one revision-fenced exact comparison.
///
/// This deliberately owns the private lookup result so diagnostics can sweep
/// bounded placement policies without repeating the exact inventory reads.
pub(crate) struct ExactRouteEvaluation {
    result: PreRouteResult,
}

impl ExactRouteEvaluation {
    pub(crate) const fn outcome_label(&self) -> &'static str {
        self.result.shadow.outcome.label()
    }

    pub(crate) const fn stable(&self) -> bool {
        matches!(
            self.result.shadow.outcome,
            ShadowOutcome::Agree
                | ShadowOutcome::WouldMove
                | ShadowOutcome::Tie
                | ShadowOutcome::AllZero
        )
    }

    pub(crate) fn placement_label(
        &self,
        decision: &Decision,
        policy: ExactPlacementPolicy,
    ) -> &'static str {
        evaluate_placement(decision, &self.result, policy).label(false)
    }

    pub(crate) fn projected_balance_label(
        &self,
        decision: &Decision,
        policy: ExactPlacementPolicy,
    ) -> &'static str {
        if self.result.shadow.outcome != ShadowOutcome::AllZero {
            return "not_cold";
        }
        evaluate_projected_cold_balance(decision, &self.result, policy).label()
    }

    pub(crate) const fn selected_tokens(&self) -> usize {
        self.result.shadow.selected_tokens
    }

    pub(crate) const fn best_tokens(&self) -> usize {
        self.result.shadow.best_tokens
    }
}

impl PreRouteResult {
    fn failure(outcome: ShadowOutcome) -> Self {
        Self {
            shadow: ShadowResult {
                outcome,
                selected_tokens: 0,
                best_tokens: 0,
            },
            overlaps: Vec::new(),
            resident_tokens: Vec::new(),
            prompt_tokens: 0,
            winner: None,
        }
    }
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
    WouldBalance { delta_tokens: usize },
    KeptBalanceDeltaGate,
    KeptBalanceLoadGate,
    Fallback,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProjectedBalanceOutcome {
    KeptSelected,
    WouldBalance { delta_tokens: usize },
    KeptDeltaGate,
    KeptLoadGate,
    Fallback,
}

impl ProjectedBalanceOutcome {
    const fn label(self) -> &'static str {
        match self {
            Self::KeptSelected => "kept_selected",
            Self::WouldBalance { .. } => "would_balance",
            Self::KeptDeltaGate => "kept_delta_gate",
            Self::KeptLoadGate => "kept_load_gate",
            Self::Fallback => "fallback",
        }
    }
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
            Self::WouldBalance { .. } => "would_balance",
            Self::KeptBalanceDeltaGate => "kept_balance_delta_gate",
            Self::KeptBalanceLoadGate => "kept_balance_load_gate",
            Self::Fallback => "fallback",
        }
    }
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
    #[cfg(test)]
    #[must_use]
    #[allow(clippy::needless_pass_by_value)]
    pub(crate) fn new(
        inventories: Arc<[SharedFencedInventory]>,
        metrics: Arc<Metrics>,
        alpha: f64,
        max_overlap_units: usize,
        load_estimator: RequestLoadEstimator,
    ) -> Self {
        Self::with_inventories(
            inventories
                .iter()
                .cloned()
                .map(ExactRouteInventory::direct)
                .collect(),
            metrics,
            alpha,
            max_overlap_units,
            load_estimator,
        )
    }

    #[must_use]
    pub(crate) fn with_inventories(
        inventories: Arc<[ExactRouteInventory]>,
        metrics: Arc<Metrics>,
        alpha: f64,
        max_overlap_units: usize,
        load_estimator: RequestLoadEstimator,
    ) -> Self {
        Self {
            inventories,
            metrics,
            scoring: ExactScoring::absolute(alpha, max_overlap_units),
            load_estimator,
        }
    }

    /// Scores exact overlap with the live router's affinity basis, peer
    /// groups, and projected-load setting rather than the absolute defaults.
    #[must_use]
    pub(crate) fn with_router_scoring(
        mut self,
        affinity_basis: AffinityBasis,
        affinity_groups: &[usize],
        projected_load: bool,
    ) -> Self {
        self.scoring.affinity_basis = affinity_basis;
        self.scoring.affinity_groups = Arc::from(affinity_groups);
        self.scoring.projected_load = projected_load;
        self
    }

    /// Whether every configured inventory is currently authoritative.
    #[must_use]
    pub fn ready(&self) -> bool {
        !self.inventories.is_empty() && self.inventories.iter().all(ExactRouteInventory::ready)
    }

    /// Whether tokenization can produce a useful exact result for at least one
    /// inventory. The route-specific gate below still requires every healthy
    /// candidate to be authoritative before placement can change the route.
    /// This distinction lets an intentionally inactive adaptive profile stay
    /// untrusted without disabling exact placement for the serving topology.
    #[must_use]
    pub fn tokenization_ready(&self) -> bool {
        self.inventories.iter().any(ExactRouteInventory::ready)
    }

    fn placement_ready_for(&self, decision: &Decision) -> bool {
        if !self.valid_candidate_set(decision) {
            return false;
        }
        let mut any_healthy = false;
        for candidate in &decision.candidate_state {
            if !candidate.healthy {
                continue;
            }
            any_healthy = true;
            let inventory = &self.inventories[candidate.index];
            if !inventory.supports_placement() || !inventory.placement_ready() {
                return false;
            }
        }
        any_healthy
    }

    /// A decision is comparable when its candidate set is non-empty, every
    /// candidate names a configured inventory exactly once, and the selected
    /// upstream is one of them. Candidate state may cover only part of the
    /// fleet: a model-restricted or lane-confined decision is still exact over
    /// the replicas it may use, and an inventory outside that set is neither
    /// read nor allowed to influence the comparison.
    fn valid_candidate_set(&self, decision: &Decision) -> bool {
        let Some(&selected) = decision.candidates.first() else {
            return false;
        };
        if decision.candidate_state.is_empty() {
            return false;
        }
        let mut seen = vec![false; self.inventories.len()];
        for candidate in &decision.candidate_state {
            if candidate.index >= self.inventories.len() || seen[candidate.index] {
                return false;
            }
            seen[candidate.index] = true;
        }
        seen.get(selected).copied().unwrap_or(false)
    }

    /// Captures the exact inventory generation visible at approximate routing
    /// time without retaining any locks across request I/O.
    #[must_use]
    pub fn capture(&self, decision: &Decision) -> ExactRouteSnapshot {
        let markers = self
            .inventories
            .iter()
            .map(ExactRouteInventory::marker)
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
    pub(crate) fn route_pre_route(
        &self,
        endpoint: Endpoint,
        token_ids: &[u32],
        request_bytes: usize,
        decision: &mut Decision,
        policy: ExactPlacementPolicy,
        mode: ExactPlacementMode,
    ) {
        // Capability is enforced here as well as by configuration parsing so
        // library callers and future wiring cannot make compact snapshots
        // serving-authoritative before qualification.
        let placement_ready = self.placement_ready_for(decision);
        let mode = if mode.applies() && !placement_ready {
            ExactPlacementMode::Shadow
        } else {
            mode
        };
        let started = Instant::now();
        let evaluation = self.evaluate_pre_route_diagnostic(token_ids, decision);
        let result = &evaluation.result;
        self.metrics
            .exact_route_preroute_duration
            .with_label_values(&[endpoint.label(), "lookup"])
            .observe(started.elapsed().as_secs_f64());
        self.metrics
            .exact_route_preroute
            .with_label_values(&[endpoint.label(), result.shadow.outcome.label()])
            .inc();
        self.record_result(endpoint, &result.shadow);
        let placement = evaluate_placement(decision, result, policy);
        if let PlacementOutcome::WouldBalance { delta_tokens, .. } = placement {
            self.metrics
                .exact_route_residency_delta
                .with_label_values(&[endpoint.label()])
                .observe(usize_to_f64(delta_tokens));
        }
        if result.shadow.outcome == ShadowOutcome::AllZero {
            let projected = evaluate_projected_cold_balance(decision, result, policy);
            if let ProjectedBalanceOutcome::WouldBalance { delta_tokens } = projected {
                self.metrics
                    .exact_route_projected_residency_delta
                    .with_label_values(&[endpoint.label()])
                    .observe(usize_to_f64(delta_tokens));
            }
            self.metrics
                .exact_route_projected_balance
                .with_label_values(&[endpoint.label(), projected.label()])
                .inc();
        }
        if mode.applies()
            && let PlacementOutcome::Move(winner) = placement
        {
            apply_placement(
                winner,
                request_bytes,
                token_ids.len(),
                decision,
                result,
                &self.scoring,
                self.load_estimator,
            );
        } else if mode.applies() {
            recompute_exact_reservations(request_bytes, decision, result, self.load_estimator);
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

    /// Evaluate one exact/approximate comparison without metrics or route
    /// mutation. Inventory markers are checked before, during, and after the
    /// lookups; callers must independently fence tokenizer attestation.
    #[must_use]
    pub(crate) fn evaluate_pre_route_diagnostic(
        &self,
        token_ids: &[u32],
        decision: &Decision,
    ) -> ExactRouteEvaluation {
        ExactRouteEvaluation {
            result: self.evaluate_pre_route(token_ids, decision),
        }
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
        if self.inventories.is_empty() {
            return PreRouteResult::failure(ShadowOutcome::NoInventories);
        }
        if !self.valid_candidate_set(decision) {
            return PreRouteResult::failure(ShadowOutcome::CandidateMismatch);
        }
        let selected = decision.candidates[0];
        let markers = self
            .inventories
            .iter()
            .map(ExactRouteInventory::marker)
            .collect::<Vec<_>>();
        let mut overlaps = vec![None; self.inventories.len()];
        let mut resident_tokens = vec![None; self.inventories.len()];
        for candidate in &decision.candidate_state {
            if !candidate.healthy {
                continue;
            }
            let Some(marker) = markers[candidate.index] else {
                return PreRouteResult::failure(ShadowOutcome::InventoryUntrusted);
            };
            let lookup = match self.inventories[candidate.index].lookup(token_ids) {
                Ok(lookup) => lookup,
                Err(ExactInventoryLookupError::Untrusted) => {
                    return PreRouteResult::failure(ShadowOutcome::InventoryUntrusted);
                }
                Err(ExactInventoryLookupError::Lookup) => {
                    return PreRouteResult::failure(ShadowOutcome::LookupError);
                }
            };
            if lookup.marker != marker {
                return PreRouteResult::failure(ShadowOutcome::InventoryChanged);
            }
            overlaps[candidate.index] = Some(lookup.overlap_tokens);
            resident_tokens[candidate.index] = Some(lookup.resident_tokens);
        }
        for candidate in decision
            .candidate_state
            .iter()
            .filter(|candidate| candidate.healthy)
        {
            let Some(marker) = markers[candidate.index] else {
                return PreRouteResult::failure(ShadowOutcome::InventoryUntrusted);
            };
            if !self.inventories[candidate.index].unchanged(marker) {
                return PreRouteResult::failure(ShadowOutcome::InventoryChanged);
            }
        }
        let shadow = classify(
            selected,
            token_ids.len(),
            &overlaps,
            decision,
            &self.scoring,
        );
        let winner = unique_winner(token_ids.len(), &overlaps, decision, &self.scoring);
        PreRouteResult {
            shadow,
            overlaps,
            resident_tokens,
            prompt_tokens: token_ids.len(),
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
        if snapshot.markers.len() != self.inventories.len()
            || !self.valid_candidate_set(&snapshot.decision)
        {
            return failure(ShadowOutcome::CandidateMismatch);
        }
        let selected = snapshot.decision.candidates[0];
        if routed_upstream != selected {
            return failure(ShadowOutcome::Failover);
        }
        let Some(selected_tokens) = cached_tokens else {
            return failure(ShadowOutcome::MissingCachedUsage);
        };
        if selected_tokens > token_ids.len() {
            return failure(ShadowOutcome::InvalidCachedUsage);
        }

        let mut overlaps = vec![None; self.inventories.len()];
        for candidate in &snapshot.decision.candidate_state {
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
            let lookup = match self.inventories[candidate.index].lookup(token_ids) {
                Ok(lookup) => lookup,
                Err(ExactInventoryLookupError::Untrusted) => {
                    return failure(ShadowOutcome::InventoryUntrusted);
                }
                Err(ExactInventoryLookupError::Lookup) => {
                    return failure(ShadowOutcome::LookupError);
                }
            };
            if lookup.marker != marker {
                return failure(ShadowOutcome::InventoryChanged);
            }
            overlaps[candidate.index] = Some(lookup.overlap_tokens);
        }

        classify(
            selected,
            token_ids.len(),
            &overlaps,
            &snapshot.decision,
            &self.scoring,
        )
    }
}

fn classify(
    selected: usize,
    prompt_tokens: usize,
    overlaps: &[Option<usize>],
    decision: &Decision,
    scoring: &ExactScoring,
) -> ShadowResult {
    let scores = scoring.score(prompt_tokens, overlaps, decision);
    let selected_tokens = overlaps.get(selected).copied().flatten().unwrap_or(0);
    let best_tokens = scores.iter().map(|score| score.tokens).max().unwrap_or(0);
    let result = |outcome| ShadowResult {
        outcome,
        selected_tokens,
        best_tokens,
    };
    if scores.len() <= 1 {
        return result(ShadowOutcome::Single);
    }
    if best_tokens == 0 {
        return result(ShadowOutcome::AllZero);
    }
    let Some(selected_score) = scores
        .iter()
        .find(|score| score.candidate.index == selected)
    else {
        return result(ShadowOutcome::CandidateMismatch);
    };
    let Some(best) = best_key(&scores) else {
        return result(ShadowOutcome::CandidateMismatch);
    };
    let tied = scores.iter().filter(|score| is_best(score, best)).count();
    let outcome = if !is_best(selected_score, best) {
        ShadowOutcome::WouldMove
    } else if tied > 1 {
        ShadowOutcome::Tie
    } else {
        ShadowOutcome::Agree
    };
    result(outcome)
}

fn unique_winner(
    prompt_tokens: usize,
    overlaps: &[Option<usize>],
    decision: &Decision,
    scoring: &ExactScoring,
) -> Option<usize> {
    let scores = scoring.score(prompt_tokens, overlaps, decision);
    let best = best_key(&scores)?;
    let mut winners = scores.iter().filter(|score| is_best(score, best));
    let winner = winners.next()?.candidate.index;
    winners.next().is_none().then_some(winner)
}

fn evaluate_placement(
    decision: &Decision,
    exact: &PreRouteResult,
    policy: ExactPlacementPolicy,
) -> PlacementOutcome {
    if exact.shadow.outcome == ShadowOutcome::AllZero {
        return evaluate_cold_balance(decision, exact, policy);
    }
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

fn evaluate_cold_balance(
    decision: &Decision,
    exact: &PreRouteResult,
    policy: ExactPlacementPolicy,
) -> PlacementOutcome {
    let Some(selected) = decision.candidates.first().copied() else {
        return PlacementOutcome::Fallback;
    };
    let Some(selected_resident) = exact.resident_tokens.get(selected).copied().flatten() else {
        return PlacementOutcome::Fallback;
    };
    let mut eligible = decision
        .candidate_state
        .iter()
        .filter(|candidate| candidate.healthy)
        .filter_map(|candidate| {
            exact
                .resident_tokens
                .get(candidate.index)
                .copied()
                .flatten()
                .map(|resident| (candidate, resident))
        })
        .collect::<Vec<_>>();
    eligible.sort_by_key(|(candidate, resident)| (*resident, candidate.index));
    let Some((winner_state, winner_resident)) = eligible.first().copied() else {
        return PlacementOutcome::Fallback;
    };
    if winner_state.index == selected {
        return PlacementOutcome::KeptAllZero;
    }
    let delta_tokens = selected_resident.saturating_sub(winner_resident);
    if delta_tokens < exact.prompt_tokens.max(1) {
        return PlacementOutcome::KeptBalanceDeltaGate;
    }
    let Some(selected_state) = decision
        .candidate_state
        .iter()
        .find(|candidate| candidate.index == selected)
    else {
        return PlacementOutcome::Fallback;
    };
    if winner_state.load_units
        > selected_state
            .load_units
            .saturating_add(policy.max_load_delta)
    {
        return PlacementOutcome::KeptBalanceLoadGate;
    }
    PlacementOutcome::WouldBalance { delta_tokens }
}

/// Compare cold placement after conservatively translating each replica's
/// bounded active load into current-request-equivalent token pressure. This is
/// observation-only: load units may include decode work and are not asserted
/// to be future resident KV state.
fn evaluate_projected_cold_balance(
    decision: &Decision,
    exact: &PreRouteResult,
    policy: ExactPlacementPolicy,
) -> ProjectedBalanceOutcome {
    let Some(selected) = decision.candidates.first().copied() else {
        return ProjectedBalanceOutcome::Fallback;
    };
    let mut eligible = Vec::new();
    for candidate in decision
        .candidate_state
        .iter()
        .filter(|candidate| candidate.healthy)
    {
        let Some(resident) = exact
            .resident_tokens
            .get(candidate.index)
            .copied()
            .flatten()
        else {
            return ProjectedBalanceOutcome::Fallback;
        };
        let Some(pressure) = load_equivalent_token_pressure(candidate, exact.prompt_tokens) else {
            return ProjectedBalanceOutcome::Fallback;
        };
        eligible.push((candidate, resident.saturating_add(pressure)));
    }
    eligible.sort_by_key(|(candidate, projected)| (*projected, candidate.index));
    let Some((winner_state, winner_projected)) = eligible.first().copied() else {
        return ProjectedBalanceOutcome::Fallback;
    };
    let Some((selected_state, selected_projected)) = eligible
        .iter()
        .copied()
        .find(|(candidate, _)| candidate.index == selected)
    else {
        return ProjectedBalanceOutcome::Fallback;
    };
    if winner_state.index == selected {
        return ProjectedBalanceOutcome::KeptSelected;
    }
    let delta_tokens = selected_projected.saturating_sub(winner_projected);
    if delta_tokens < exact.prompt_tokens.max(1) {
        return ProjectedBalanceOutcome::KeptDeltaGate;
    }
    if winner_state.load_units
        > selected_state
            .load_units
            .saturating_add(policy.max_load_delta)
    {
        return ProjectedBalanceOutcome::KeptLoadGate;
    }
    ProjectedBalanceOutcome::WouldBalance { delta_tokens }
}

fn load_equivalent_token_pressure(
    candidate: &CandidateState,
    prompt_tokens: usize,
) -> Option<usize> {
    let request_units = u128::try_from(candidate.request_load_units).ok()?;
    if request_units == 0 {
        return None;
    }
    let numerator = (candidate.load_units as u128).saturating_mul(prompt_tokens as u128);
    usize::try_from(numerator.div_ceil(request_units)).ok()
}

fn apply_placement(
    winner: usize,
    request_bytes: usize,
    prompt_tokens: usize,
    decision: &mut Decision,
    exact: &PreRouteResult,
    scoring: &ExactScoring,
    load_estimator: RequestLoadEstimator,
) {
    let credited = scoring
        .score(prompt_tokens, &exact.overlaps, decision)
        .into_iter()
        .map(|score| (score.candidate.index, score.affinity))
        .collect::<Vec<_>>();
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
        candidate.affinity_blocks = credited
            .iter()
            .find_map(|(index, affinity)| (*index == candidate.index).then_some(*affinity))
            .unwrap_or_else(|| candidate.overlap_blocks.min(scoring.max_overlap_units));
    }
    let winner_state = &mut decision.candidate_state[0];
    decision.overlap_blocks = winner_state.overlap_blocks;
    decision.affinity_blocks = winner_state.affinity_blocks;
    decision.load_units = winner_state.request_load_units;
    decision.outcome = Outcome::Exact;
    recompute_exact_reservations(request_bytes, decision, exact, load_estimator);
}

/// Rescale admission reservations onto the exact warm-prefix overlap.
///
/// The denominator is read from `exact` rather than accepted from the caller so
/// it cannot drift from the `overlaps` it scales: a mismatched prompt length
/// would silently reserve the wrong capacity with nothing to fence it. A
/// failure result carries `prompt_tokens: 0` and empty overlaps, which both
/// fail closed here.
fn recompute_exact_reservations(
    request_bytes: usize,
    decision: &mut Decision,
    exact: &PreRouteResult,
    load_estimator: RequestLoadEstimator,
) {
    let Some(selected) = decision.candidates.first().copied() else {
        return;
    };
    let mut updates = Vec::with_capacity(decision.candidate_state.len());
    for candidate in decision
        .candidate_state
        .iter()
        .filter(|candidate| candidate.healthy)
    {
        let Some(overlap_tokens) = exact.overlaps.get(candidate.index).copied().flatten() else {
            return;
        };
        let Some(units) = load_estimator.estimate_exact_tokens(
            request_bytes,
            overlap_tokens,
            exact.prompt_tokens,
        ) else {
            return;
        };
        updates.push((candidate.index, units));
    }
    let Some(selected_units) = updates
        .iter()
        .find_map(|(index, units)| (*index == selected).then_some(*units))
    else {
        return;
    };
    for candidate in &mut decision.candidate_state {
        if let Some((_, units)) = updates.iter().find(|(index, _)| *index == candidate.index) {
            candidate.request_load_units = *units;
        }
    }
    decision.load_units = selected_units;
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
    use parking_lot::Mutex;
    use prometheus::Registry;

    use super::*;
    use crate::{
        exact_index::{ExactIndexLimits, FencedExactKvInventory},
        kv_wire::{BlockStored, ExternalBlockHash, KvEvent, KvEventBatch},
        router::{CandidateState, Outcome},
        snapshot_actor::{SnapshotActorLimits, SnapshotBootstrapActor},
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
            stale_blocks: 0,
            horizon_ms: None,
            overlap_ages_ms: Vec::new(),
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

    fn set_reservations(decision: &mut Decision, units: [usize; 2]) {
        for candidate in &mut decision.candidate_state {
            candidate.request_load_units = units[candidate.index];
        }
        decision.load_units = units[decision.candidates[0]];
    }

    fn estimator() -> RequestLoadEstimator {
        RequestLoadEstimator::new(2, 128, 8)
    }

    fn batch(events: Vec<KvEvent>) -> KvEventBatch {
        KvEventBatch {
            timestamp: 1.0,
            events,
            data_parallel_rank: Some(0),
        }
    }

    fn store(tokens: &[u32]) -> KvEvent {
        store_hash(7, tokens)
    }

    fn store_hash(hash: u64, tokens: &[u32]) -> KvEvent {
        KvEvent::BlockStored(BlockStored {
            block_hashes: vec![ExternalBlockHash::Unsigned(hash)],
            parent_block_hash: None,
            token_ids: tokens.to_vec(),
            block_size: tokens.len(),
            group_idx: Some(0),
            kv_cache_spec_kind: Some("full_attention".to_owned()),
            kv_cache_spec_sliding_window: None,
            medium: Some("GPU".to_owned()),
            locality: Some("LOCAL".to_owned()),
            lora_name: None,
            cache_namespace: None,
            has_extra_keys: false,
        })
    }

    fn store_unlearned(hash: u64, tokens: &[u32]) -> KvEvent {
        let KvEvent::BlockStored(mut stored) = store_hash(hash, tokens) else {
            unreachable!("store_hash always constructs a store event");
        };
        stored.kv_cache_spec_kind = None;
        KvEvent::BlockStored(stored)
    }

    fn trusted_inventory(events: Vec<KvEvent>) -> SharedFencedInventory {
        let mut inventory = FencedExactKvInventory::new(8, ExactIndexLimits::default());
        inventory.ingest_live(0, &batch(events)).unwrap();
        Arc::new(parking_lot::RwLock::new(inventory))
    }

    fn assert_one(value: f64) {
        assert!(
            (value - 1.0).abs() < f64::EPSILON,
            "expected one, got {value}"
        );
    }

    #[test]
    fn diagnostic_evaluation_is_revision_fenced_and_route_immutable() {
        let selected = trusted_inventory(Vec::new());
        let alternative = trusted_inventory(vec![store(&[1, 2, 3, 4])]);
        let shadow = ExactRouteShadow::new(
            Arc::from([selected, alternative]),
            Arc::new(Metrics::new(&Registry::new()).unwrap()),
            1.0,
            8,
            estimator(),
        );
        let route = decision();
        let original = route.clone();
        let evaluation = shadow.evaluate_pre_route_diagnostic(&[1, 2, 3, 4], &route);
        assert_eq!(route, original);
        assert!(evaluation.stable());
        assert_eq!(evaluation.outcome_label(), "would_move");
        assert_eq!(evaluation.selected_tokens(), 0);
        assert_eq!(evaluation.best_tokens(), 4);
        assert_eq!(
            evaluation.placement_label(
                &route,
                ExactPlacementPolicy {
                    min_gain_tokens: 1,
                    max_load_delta: 0,
                },
            ),
            "would_move"
        );
        assert_eq!(
            evaluation.projected_balance_label(
                &route,
                ExactPlacementPolicy {
                    min_gain_tokens: 1,
                    max_load_delta: 0,
                },
            ),
            "not_cold"
        );
    }

    #[test]
    fn exact_overlap_replaces_only_the_cache_term() {
        let mut route = decision();
        route.candidate_state[1].load_units = 1;
        let scoring = ExactScoring::absolute(1.0, 8);
        let result = classify(0, 8, &[Some(0), Some(8)], &route, &scoring);
        assert_eq!(result.outcome, ShadowOutcome::WouldMove);
        assert_eq!(result.best_tokens, 8);

        // Equal weighted scores are separated by exact tokens, exactly as the
        // live router separates them by overlap.
        route.candidate_state[1].load_units = 8;
        let result = classify(0, 8, &[Some(0), Some(8)], &route, &scoring);
        assert_eq!(result.outcome, ShadowOutcome::WouldMove);
        assert_eq!(
            unique_winner(8, &[Some(0), Some(8)], &route, &scoring),
            Some(1)
        );

        route.candidate_state[1].load_units = 9;
        let result = classify(0, 8, &[Some(0), Some(8)], &route, &scoring);
        assert_eq!(result.outcome, ShadowOutcome::Agree);
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
            estimator(),
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
            estimator(),
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
            estimator(),
        );
        let mut route = decision();
        let original = route.clone();
        for mode in [ExactPlacementMode::Shadow, ExactPlacementMode::Control] {
            shadow.route_pre_route(
                Endpoint::Chat,
                &[1, 2, 3, 4],
                1_024,
                &mut route,
                ExactPlacementPolicy {
                    min_gain_tokens: 4,
                    max_load_delta: 0,
                },
                mode,
            );
        }
        assert_eq!(route, original);
        assert!(
            (metrics
                .exact_route_preroute
                .with_label_values(&["chat", "would_move"])
                .get()
                - 2.0)
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
        assert!(
            (metrics
                .exact_route_placement
                .with_label_values(&["control", "chat", "would_move"])
                .get()
                - 1.0)
                .abs()
                < f64::EPSILON
        );
    }

    #[test]
    fn snapshot_representation_forces_shadow_at_the_routing_boundary() {
        let inventories = (0..2)
            .map(|_| {
                ExactRouteInventory::snapshot(Arc::new(Mutex::new(
                    SnapshotBootstrapActor::new(SnapshotActorLimits::default()).unwrap(),
                )))
            })
            .collect();
        let registry = Registry::new();
        let metrics = Arc::new(Metrics::new(&registry).unwrap());
        let shadow = ExactRouteShadow::with_inventories(
            inventories,
            Arc::clone(&metrics),
            1.0,
            8,
            estimator(),
        );
        let mut route = decision();
        let original = route.clone();

        shadow.route_pre_route(
            Endpoint::Chat,
            &[1, 2, 3, 4],
            1_024,
            &mut route,
            ExactPlacementPolicy {
                min_gain_tokens: 0,
                max_load_delta: usize::MAX,
            },
            ExactPlacementMode::Placement,
        );

        assert_eq!(route, original);
        assert_one(
            metrics
                .exact_route_placement
                .with_label_values(&["shadow", "chat", "fallback"])
                .get(),
        );
        assert!(
            metrics
                .exact_route_placement
                .with_label_values(&["placement", "chat", "fallback"])
                .get()
                .abs()
                < f64::EPSILON
        );
    }

    #[test]
    fn unlearned_hybrid_group_forces_shadow_without_hiding_exact_overlap() {
        let selected = trusted_inventory(Vec::new());
        let alternative = trusted_inventory(vec![store_unlearned(7, &[1, 2, 3, 4])]);
        let metrics = Arc::new(Metrics::new(&Registry::new()).unwrap());
        let shadow = ExactRouteShadow::new(
            Arc::from([selected, alternative]),
            Arc::clone(&metrics),
            1.0,
            8,
            estimator(),
        );
        let mut route = decision();
        let original = route.clone();

        shadow.route_pre_route(
            Endpoint::Chat,
            &[1, 2, 3, 4],
            1_024,
            &mut route,
            ExactPlacementPolicy {
                min_gain_tokens: 4,
                max_load_delta: 0,
            },
            ExactPlacementMode::Placement,
        );

        assert_eq!(route, original);
        assert_one(
            metrics
                .exact_route_placement
                .with_label_values(&["shadow", "chat", "would_move"])
                .get(),
        );
    }

    #[test]
    fn cold_residency_balance_is_shadow_only_and_requires_one_prompt_delta() {
        let fuller = trusted_inventory(vec![store_hash(7, &[1, 2, 3, 4, 5, 6, 7, 8])]);
        let emptier = trusted_inventory(Vec::new());
        let registry = Registry::new();
        let metrics = Arc::new(Metrics::new(&registry).unwrap());
        let shadow = ExactRouteShadow::new(
            Arc::from([fuller, emptier]),
            Arc::clone(&metrics),
            1.0,
            8,
            estimator(),
        );
        let mut original = decision();
        set_reservations(&mut original, [8, 8]);
        for mode in [ExactPlacementMode::Shadow, ExactPlacementMode::Placement] {
            let mut route = original.clone();
            shadow.route_pre_route(
                Endpoint::Chat,
                &[9, 10, 11, 12],
                1_024,
                &mut route,
                ExactPlacementPolicy {
                    min_gain_tokens: 4,
                    max_load_delta: 0,
                },
                mode,
            );
            assert_eq!(route, original, "cold balance must remain shadow-only");
        }
        assert_one(
            metrics
                .exact_route_placement
                .with_label_values(&["shadow", "chat", "would_balance"])
                .get(),
        );
        assert_one(
            metrics
                .exact_route_placement
                .with_label_values(&["placement", "chat", "would_balance"])
                .get(),
        );
        assert_eq!(
            metrics
                .exact_route_residency_delta
                .with_label_values(&["chat"])
                .get_sample_count(),
            2
        );

        let slightly_fuller = trusted_inventory(vec![store_hash(8, &[1, 2])]);
        let empty = trusted_inventory(Vec::new());
        let registry = Registry::new();
        let metrics = Arc::new(Metrics::new(&registry).unwrap());
        let shadow = ExactRouteShadow::new(
            Arc::from([slightly_fuller, empty]),
            Arc::clone(&metrics),
            1.0,
            8,
            estimator(),
        );
        let mut route = decision();
        set_reservations(&mut route, [8, 8]);
        shadow.route_pre_route(
            Endpoint::Chat,
            &[9, 10, 11, 12],
            1_024,
            &mut route,
            ExactPlacementPolicy {
                min_gain_tokens: 4,
                max_load_delta: 0,
            },
            ExactPlacementMode::Shadow,
        );
        assert_one(
            metrics
                .exact_route_placement
                .with_label_values(&["shadow", "chat", "kept_balance_delta_gate"])
                .get(),
        );
    }

    #[test]
    fn cold_residency_balance_respects_the_existing_load_gate() {
        let fuller = trusted_inventory(vec![store_hash(7, &[1, 2, 3, 4, 5, 6, 7, 8])]);
        let emptier = trusted_inventory(Vec::new());
        let registry = Registry::new();
        let metrics = Arc::new(Metrics::new(&registry).unwrap());
        let shadow = ExactRouteShadow::new(
            Arc::from([fuller, emptier]),
            Arc::clone(&metrics),
            1.0,
            8,
            estimator(),
        );
        let mut route = decision();
        route.candidate_state[1].load_units = 1;
        shadow.route_pre_route(
            Endpoint::Chat,
            &[9, 10, 11, 12],
            1_024,
            &mut route,
            ExactPlacementPolicy {
                min_gain_tokens: 4,
                max_load_delta: 0,
            },
            ExactPlacementMode::Shadow,
        );
        assert_one(
            metrics
                .exact_route_placement
                .with_label_values(&["shadow", "chat", "kept_balance_load_gate"])
                .get(),
        );
        assert_one(
            metrics
                .exact_route_projected_balance
                .with_label_values(&["chat", "kept_load_gate"])
                .get(),
        );
    }

    #[test]
    fn projected_cold_balance_accounts_for_inflight_load_without_changing_route() {
        let fuller = trusted_inventory(vec![store_hash(7, &[1, 2, 3, 4, 5, 6, 7, 8])]);
        let emptier = trusted_inventory(Vec::new());
        let registry = Registry::new();
        let metrics = Arc::new(Metrics::new(&registry).unwrap());
        let shadow = ExactRouteShadow::new(
            Arc::from([fuller, emptier]),
            Arc::clone(&metrics),
            1.0,
            8,
            estimator(),
        );
        let mut route = decision();
        route.candidate_state[1].load_units = 2;
        let original = route.clone();

        shadow.route_pre_route(
            Endpoint::Chat,
            &[9, 10, 11, 12],
            1_024,
            &mut route,
            ExactPlacementPolicy {
                min_gain_tokens: 4,
                max_load_delta: 0,
            },
            ExactPlacementMode::Shadow,
        );

        assert_eq!(route, original);
        assert_one(
            metrics
                .exact_route_placement
                .with_label_values(&["shadow", "chat", "kept_balance_load_gate"])
                .get(),
        );
        assert_one(
            metrics
                .exact_route_projected_balance
                .with_label_values(&["chat", "kept_selected"])
                .get(),
        );
        assert_eq!(
            metrics
                .exact_route_projected_residency_delta
                .with_label_values(&["chat"])
                .get_sample_count(),
            0
        );
    }

    #[test]
    fn projected_cold_balance_records_only_a_full_prompt_delta() {
        let fuller = trusted_inventory(vec![store_hash(7, &[1, 2, 3, 4, 5, 6, 7, 8])]);
        let emptier = trusted_inventory(Vec::new());
        let metrics = Arc::new(Metrics::new(&Registry::new()).unwrap());
        let shadow = ExactRouteShadow::new(
            Arc::from([fuller, emptier]),
            Arc::clone(&metrics),
            1.0,
            8,
            estimator(),
        );
        let mut route = decision();

        shadow.route_pre_route(
            Endpoint::Chat,
            &[9, 10, 11, 12],
            1_024,
            &mut route,
            ExactPlacementPolicy {
                min_gain_tokens: 4,
                max_load_delta: 0,
            },
            ExactPlacementMode::Shadow,
        );

        assert_one(
            metrics
                .exact_route_projected_balance
                .with_label_values(&["chat", "would_balance"])
                .get(),
        );
        assert_eq!(
            metrics
                .exact_route_projected_residency_delta
                .with_label_values(&["chat"])
                .get_sample_count(),
            1
        );
        assert_eq!(
            load_equivalent_token_pressure(&candidate(0, 0, 3), 10),
            Some(30)
        );
        let mut invalid = candidate(0, 0, 3);
        invalid.request_load_units = 0;
        assert_eq!(load_equivalent_token_pressure(&invalid, 10), None);
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
            estimator(),
        );
        let mut route = decision();
        set_reservations(&mut route, [8, 8]);
        shadow.route_pre_route(
            Endpoint::Chat,
            &[1, 2, 3, 4],
            1_024,
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
        assert_eq!(route.candidate_state[0].request_load_units, 1);
        assert_eq!(route.candidate_state[1].request_load_units, 8);
        assert_eq!(route.load_units, 1);
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
    fn placement_recomputes_partial_to_warmer_reservations() {
        let selected = trusted_inventory(vec![store_hash(7, &[1, 2])]);
        let alternative = trusted_inventory(vec![store_hash(8, &[1, 2, 3, 4])]);
        let shadow = ExactRouteShadow::new(
            Arc::from([selected, alternative]),
            Arc::new(Metrics::new(&Registry::new()).unwrap()),
            1.0,
            8,
            estimator(),
        );
        let mut route = decision();
        set_reservations(&mut route, [8, 8]);

        shadow.route_pre_route(
            Endpoint::Chat,
            &[1, 2, 3, 4, 5, 6, 7, 8],
            1_024,
            &mut route,
            ExactPlacementPolicy {
                min_gain_tokens: 2,
                max_load_delta: 0,
            },
            ExactPlacementMode::Placement,
        );

        assert_eq!(route.candidates, [1, 0]);
        assert_eq!(route.candidate_state[0].request_load_units, 4);
        assert_eq!(route.candidate_state[1].request_load_units, 6);
        assert_eq!(route.load_units, 4);
    }

    #[test]
    fn shadow_mode_never_recomputes_reservations() {
        // Same warm-prefix shape that placement mode recomputes to [4, 6].
        // Observation mode must leave admission accounting untouched.
        let selected = trusted_inventory(vec![store_hash(7, &[1, 2])]);
        let alternative = trusted_inventory(vec![store_hash(8, &[1, 2, 3, 4])]);
        let shadow = ExactRouteShadow::new(
            Arc::from([selected, alternative]),
            Arc::new(Metrics::new(&Registry::new()).unwrap()),
            1.0,
            8,
            estimator(),
        );
        let mut route = decision();
        set_reservations(&mut route, [8, 8]);
        let original = route.clone();

        for mode in [ExactPlacementMode::Shadow, ExactPlacementMode::Control] {
            let mut observed = original.clone();
            shadow.route_pre_route(
                Endpoint::Chat,
                &[1, 2, 3, 4, 5, 6, 7, 8],
                1_024,
                &mut observed,
                ExactPlacementPolicy {
                    min_gain_tokens: 2,
                    max_load_delta: 0,
                },
                mode,
            );
            assert_eq!(observed, original, "{mode:?} must not alter admission");
        }
    }

    #[test]
    fn placement_preserves_an_unchanged_selected_reservation() {
        let selected = trusted_inventory(vec![store_hash(7, &[1, 2])]);
        let alternative = trusted_inventory(Vec::new());
        let shadow = ExactRouteShadow::new(
            Arc::from([selected, alternative]),
            Arc::new(Metrics::new(&Registry::new()).unwrap()),
            1.0,
            8,
            estimator(),
        );
        let mut route = decision();
        set_reservations(&mut route, [4, 8]);

        shadow.route_pre_route(
            Endpoint::Chat,
            &[1, 2, 3, 4],
            1_024,
            &mut route,
            ExactPlacementPolicy {
                min_gain_tokens: 1,
                max_load_delta: 0,
            },
            ExactPlacementMode::Placement,
        );

        assert_eq!(route.candidates, [0, 1]);
        assert_eq!(route.candidate_state[0].request_load_units, 4);
        assert_eq!(route.load_units, 4);
    }

    #[test]
    fn placement_keeps_cold_reservations_capped() {
        let shadow = ExactRouteShadow::new(
            Arc::from([trusted_inventory(Vec::new()), trusted_inventory(Vec::new())]),
            Arc::new(Metrics::new(&Registry::new()).unwrap()),
            1.0,
            8,
            estimator(),
        );
        let mut route = decision();
        set_reservations(&mut route, [8, 8]);

        shadow.route_pre_route(
            Endpoint::Chat,
            &[1, 2, 3, 4],
            4_096,
            &mut route,
            ExactPlacementPolicy {
                min_gain_tokens: 1,
                max_load_delta: 0,
            },
            ExactPlacementMode::Placement,
        );

        assert_eq!(route.candidate_state[0].request_load_units, 8);
        assert_eq!(route.candidate_state[1].request_load_units, 8);
        assert_eq!(route.load_units, 8);
    }

    #[test]
    fn placement_keeps_original_reservations_when_inventory_is_untrusted() {
        let untrusted = Arc::new(parking_lot::RwLock::new(FencedExactKvInventory::new(
            8,
            ExactIndexLimits::default(),
        )));
        let shadow = ExactRouteShadow::new(
            Arc::from([trusted_inventory(vec![store(&[1, 2])]), untrusted]),
            Arc::new(Metrics::new(&Registry::new()).unwrap()),
            1.0,
            8,
            estimator(),
        );
        let mut route = decision();
        set_reservations(&mut route, [5, 7]);
        let original = route.clone();

        shadow.route_pre_route(
            Endpoint::Chat,
            &[1, 2, 3, 4],
            1_024,
            &mut route,
            ExactPlacementPolicy {
                min_gain_tokens: 1,
                max_load_delta: 0,
            },
            ExactPlacementMode::Placement,
        );

        assert_eq!(route, original);
    }

    #[test]
    fn exact_reservation_update_is_atomic_on_missing_overlap() {
        let mut route = decision();
        set_reservations(&mut route, [3, 6]);
        let original = route.clone();
        let exact = PreRouteResult {
            shadow: ShadowResult {
                outcome: ShadowOutcome::Agree,
                selected_tokens: 2,
                best_tokens: 2,
            },
            overlaps: vec![Some(2), None],
            resident_tokens: vec![Some(2), Some(0)],
            prompt_tokens: 4,
            winner: Some(0),
        };

        recompute_exact_reservations(1_024, &mut route, &exact, estimator());

        assert_eq!(route, original);
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
            estimator(),
        );
        let mut gain_gated = decision();
        shadow.route_pre_route(
            Endpoint::Chat,
            &[1, 2, 3, 4],
            1_024,
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
            1_024,
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
            estimator(),
        );
        let mut route = decision();
        route.candidate_state[1].load_units = 1;
        let original = route.clone();
        shadow.route_pre_route(
            Endpoint::Chat,
            &[1, 2, 3, 4],
            1_024,
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
            ExactRouteShadow::new(
                Arc::from([trusted.clone()]),
                Arc::clone(&metrics),
                1.0,
                8,
                estimator()
            )
            .ready()
        );
        let partial = ExactRouteShadow::new(
            Arc::from([trusted, untrusted]),
            Arc::clone(&metrics),
            1.0,
            8,
            estimator(),
        );
        assert!(!partial.ready());
        assert!(partial.tokenization_ready());
    }

    #[test]
    fn inactive_untrusted_inventory_does_not_disable_active_placement() {
        let trusted = trusted_inventory(Vec::new());
        let untrusted = Arc::new(parking_lot::RwLock::new(FencedExactKvInventory::new(
            8,
            ExactIndexLimits::default(),
        )));
        let metrics = Arc::new(Metrics::new(&Registry::new()).unwrap());
        let shadow = ExactRouteShadow::new(
            Arc::from([trusted, untrusted]),
            Arc::clone(&metrics),
            1.0,
            8,
            estimator(),
        );
        let mut route = decision();
        route.candidate_state[1].healthy = false;

        shadow.route_pre_route(
            Endpoint::Chat,
            &[1, 2, 3, 4],
            1_024,
            &mut route,
            ExactPlacementPolicy {
                min_gain_tokens: 0,
                max_load_delta: usize::MAX,
            },
            ExactPlacementMode::Placement,
        );

        assert_one(
            metrics
                .exact_route_placement
                .with_label_values(&["placement", "chat", "fallback"])
                .get(),
        );
        assert!(
            metrics
                .exact_route_placement
                .with_label_values(&["shadow", "chat", "fallback"])
                .get()
                .abs()
                < f64::EPSILON
        );
    }
    fn store_child(hash: u64, parent: u64, tokens: &[u32]) -> KvEvent {
        let KvEvent::BlockStored(mut stored) = store_hash(hash, tokens) else {
            unreachable!("store_hash always constructs a store event");
        };
        stored.parent_block_hash = Some(ExternalBlockHash::Unsigned(parent));
        KvEvent::BlockStored(stored)
    }

    fn untrusted_inventory() -> SharedFencedInventory {
        Arc::new(parking_lot::RwLock::new(FencedExactKvInventory::new(
            8,
            ExactIndexLimits::default(),
        )))
    }

    fn policy() -> ExactPlacementPolicy {
        ExactPlacementPolicy {
            min_gain_tokens: 1,
            max_load_delta: 0,
        }
    }

    /// Replica 0 holds a 20k-token system prompt, replica 1 the same prompt
    /// plus 180k tokens of session history. Both saturate a 16-unit cap.
    fn saturated_route() -> (Decision, [Option<usize>; 2], usize) {
        let mut route = decision();
        route.total_blocks = 220;
        (route, [Some(20_000), Some(200_000)], 220_000)
    }

    #[test]
    fn saturated_cap_tie_is_broken_by_exact_tokens() {
        let (route, overlaps, prompt) = saturated_route();
        let scoring = ExactScoring::absolute(1.0, 16);
        let result = classify(0, prompt, &overlaps, &route, &scoring);
        assert_eq!(result.outcome, ShadowOutcome::WouldMove);
        assert_eq!(result.selected_tokens, 20_000);
        assert_eq!(result.best_tokens, 200_000);
        let winner = unique_winner(prompt, &overlaps, &route, &scoring);
        assert_eq!(winner, Some(1));
        let exact = PreRouteResult {
            shadow: result,
            overlaps: overlaps.to_vec(),
            resident_tokens: vec![Some(20_000), Some(200_000)],
            prompt_tokens: prompt,
            winner,
        };
        assert_eq!(
            evaluate_placement(&route, &exact, policy()),
            PlacementOutcome::Move(1)
        );

        // Already on the warmer replica: agreement, not a tie.
        let mut on_winner = route.clone();
        on_winner.candidates = vec![1, 0];
        let result = classify(1, prompt, &overlaps, &on_winner, &scoring);
        assert_eq!(result.outcome, ShadowOutcome::Agree);
    }

    #[test]
    fn identical_score_and_tokens_remain_a_tie() {
        let (route, _, prompt) = saturated_route();
        let overlaps = [Some(200_000), Some(200_000)];
        let scoring = ExactScoring::absolute(1.0, 16);
        let result = classify(0, prompt, &overlaps, &route, &scoring);
        assert_eq!(result.outcome, ShadowOutcome::Tie);
        assert_eq!(unique_winner(prompt, &overlaps, &route, &scoring), None);
        let exact = PreRouteResult {
            shadow: result,
            overlaps: overlaps.to_vec(),
            resident_tokens: vec![Some(0), Some(0)],
            prompt_tokens: prompt,
            winner: None,
        };
        assert_eq!(
            evaluate_placement(&route, &exact, policy()),
            PlacementOutcome::KeptTie
        );
    }

    #[test]
    fn saturated_cap_placement_moves_to_the_warmer_inventory() {
        let selected = trusted_inventory(vec![store_hash(1, &[1, 2, 3, 4])]);
        let alternative = trusted_inventory(vec![
            store_hash(1, &[1, 2, 3, 4]),
            store_child(2, 1, &[5, 6, 7, 8]),
        ]);
        let metrics = Arc::new(Metrics::new(&Registry::new()).unwrap());
        // A one-unit cap saturates on both replicas.
        let shadow = ExactRouteShadow::new(
            Arc::from([selected, alternative]),
            Arc::clone(&metrics),
            1.0,
            1,
            estimator(),
        );
        let mut route = decision();
        shadow.route_pre_route(
            Endpoint::Chat,
            &[1, 2, 3, 4, 5, 6, 7, 8],
            1_024,
            &mut route,
            policy(),
            ExactPlacementMode::Placement,
        );
        assert_eq!(route.candidates, [1, 0]);
        assert_eq!(route.outcome, Outcome::Exact);
        assert_one(
            metrics
                .exact_route_preroute
                .with_label_values(&["chat", "would_move"])
                .get(),
        );
        assert_one(
            metrics
                .exact_route_placement
                .with_label_values(&["placement", "chat", "moved"])
                .get(),
        );
    }

    /// A decision whose candidate state covers only replicas 0 and 1 of a
    /// three-inventory fleet, as a model- or lane-restricted route does.
    fn restricted_route() -> Decision {
        let mut route = decision();
        route.candidate_state = vec![candidate(0, 0, 0), candidate(1, 1, 0)];
        route
    }

    #[test]
    fn restricted_decision_is_compared_over_its_own_candidates() {
        let selected = trusted_inventory(Vec::new());
        let alternative = trusted_inventory(vec![store(&[1, 2, 3, 4])]);
        // The out-of-set inventory is untrusted: it must not be consulted.
        let shadow = ExactRouteShadow::new(
            Arc::from([selected, alternative, untrusted_inventory()]),
            Arc::new(Metrics::new(&Registry::new()).unwrap()),
            1.0,
            8,
            estimator(),
        );
        let route = restricted_route();
        let evaluation = shadow.evaluate_pre_route_diagnostic(&[1, 2, 3, 4], &route);
        assert_eq!(evaluation.outcome_label(), "would_move");
        assert_eq!(evaluation.best_tokens(), 4);
        assert_eq!(evaluation.placement_label(&route, policy()), "would_move");
        assert!(shadow.placement_ready_for(&route));

        let mut placed = route.clone();
        shadow.route_pre_route(
            Endpoint::Chat,
            &[1, 2, 3, 4],
            1_024,
            &mut placed,
            policy(),
            ExactPlacementMode::Placement,
        );
        assert_eq!(placed.candidates, [1, 0]);
    }

    #[test]
    fn restricted_decision_post_route_shadow_is_not_a_mismatch() {
        let selected = trusted_inventory(Vec::new());
        let alternative = trusted_inventory(vec![store(&[1, 2, 3, 4])]);
        let registry = Registry::new();
        let metrics = Arc::new(Metrics::new(&registry).unwrap());
        let shadow = ExactRouteShadow::new(
            Arc::from([selected, alternative, untrusted_inventory()]),
            Arc::clone(&metrics),
            1.0,
            8,
            estimator(),
        );
        let snapshot = shadow.capture(&restricted_route());
        shadow.observe(
            "remote",
            Endpoint::Chat,
            0,
            Some(0),
            &[1, 2, 3, 4],
            &snapshot,
        );
        assert_one(
            metrics
                .exact_route_shadow
                .with_label_values(&["remote", "chat", "would_move"])
                .get(),
        );
    }

    #[test]
    fn malformed_candidate_sets_still_fail_closed() {
        let shadow = ExactRouteShadow::new(
            Arc::from([
                trusted_inventory(Vec::new()),
                trusted_inventory(Vec::new()),
                trusted_inventory(Vec::new()),
            ]),
            Arc::new(Metrics::new(&Registry::new()).unwrap()),
            1.0,
            8,
            estimator(),
        );
        let mismatch = |route: &Decision| {
            assert_eq!(
                shadow
                    .evaluate_pre_route_diagnostic(&[1, 2, 3, 4], route)
                    .outcome_label(),
                "candidate_mismatch"
            );
            assert!(!shadow.placement_ready_for(route));
        };

        let mut duplicate = restricted_route();
        duplicate.candidate_state[1].index = 0;
        mismatch(&duplicate);

        let mut out_of_bounds = restricted_route();
        out_of_bounds.candidate_state[1].index = 3;
        mismatch(&out_of_bounds);

        let mut empty_state = restricted_route();
        empty_state.candidate_state.clear();
        mismatch(&empty_state);

        let mut no_candidates = restricted_route();
        no_candidates.candidates.clear();
        mismatch(&no_candidates);

        let mut selected_outside = restricted_route();
        selected_outside.candidates = vec![2, 0];
        mismatch(&selected_outside);
    }

    #[test]
    fn marginal_and_relative_bases_follow_the_router() {
        let (mut route, overlaps, prompt) = saturated_route();
        // The warmer replica is busier. Absolute affinity saturates on both
        // and keeps the request on the idle replica; a peer-relative basis
        // credits only the 180k tokens the idle replica would recompute.
        route.candidate_state[1].load_units = 5;
        let absolute = ExactScoring::absolute(1.0, 16);
        assert_eq!(
            classify(0, prompt, &overlaps, &route, &absolute).outcome,
            ShadowOutcome::Agree
        );
        for basis in [AffinityBasis::Marginal, AffinityBasis::Relative] {
            let scoring = ExactScoring {
                affinity_basis: basis,
                ..absolute.clone()
            };
            assert_eq!(
                classify(0, prompt, &overlaps, &route, &scoring).outcome,
                ShadowOutcome::WouldMove,
                "{basis:?}"
            );
            assert_eq!(unique_winner(prompt, &overlaps, &route, &scoring), Some(1));
        }
    }

    #[test]
    fn marginal_peer_floor_is_per_affinity_group() {
        let mut route = decision();
        route.total_blocks = 220;
        route.candidates = vec![0, 1, 2];
        route.candidate_state = vec![candidate(0, 0, 0), candidate(1, 1, 5), candidate(2, 2, 0)];
        let overlaps = [Some(20_000), Some(200_000), Some(0)];
        let ungrouped = ExactScoring {
            affinity_basis: AffinityBasis::Marginal,
            ..ExactScoring::absolute(1.0, 16)
        };
        // A cold replica serving another model drops a fleet-wide floor to
        // zero, which degrades marginal to absolute.
        assert_eq!(
            unique_winner(220_000, &overlaps, &route, &ungrouped),
            Some(0)
        );
        let grouped = ExactScoring {
            affinity_groups: Arc::from([0, 0, 1]),
            ..ungrouped
        };
        assert_eq!(unique_winner(220_000, &overlaps, &route, &grouped), Some(1));
    }

    #[test]
    fn projected_load_charges_each_candidates_own_request_work() {
        let mut route = decision();
        // Replica 0 is idle but would recompute the whole prompt; replica 1
        // is busier but already holds half of it.
        route.candidate_state[0].request_load_units = 8;
        route.candidate_state[1].load_units = 5;
        route.candidate_state[1].request_load_units = 1;
        let overlaps = [Some(0), Some(4)];
        let plain = ExactScoring::absolute(1.0, 8);
        assert_eq!(
            classify(0, 8, &overlaps, &route, &plain).outcome,
            ShadowOutcome::Agree
        );
        let projected = ExactScoring {
            projected_load: true,
            ..plain
        };
        assert_eq!(
            classify(0, 8, &overlaps, &route, &projected).outcome,
            ShadowOutcome::WouldMove
        );
        assert_eq!(unique_winner(8, &overlaps, &route, &projected), Some(1));
    }

    #[test]
    fn router_scoring_is_applied_by_the_shadow() {
        let selected = trusted_inventory(Vec::new());
        let alternative = trusted_inventory(vec![store(&[1, 2, 3, 4])]);
        let metrics = Arc::new(Metrics::new(&Registry::new()).unwrap());
        let shadow = ExactRouteShadow::new(
            Arc::from([selected, alternative]),
            Arc::clone(&metrics),
            1.0,
            8,
            estimator(),
        );
        let mut route = decision();
        route.candidate_state[0].request_load_units = 8;
        route.candidate_state[1].load_units = 5;
        assert_eq!(
            shadow
                .evaluate_pre_route_diagnostic(&[1, 2, 3, 4, 5, 6, 7, 8], &route)
                .outcome_label(),
            "agree"
        );
        let projected = shadow.with_router_scoring(AffinityBasis::Absolute, &[], true);
        assert_eq!(
            projected
                .evaluate_pre_route_diagnostic(&[1, 2, 3, 4, 5, 6, 7, 8], &route)
                .outcome_label(),
            "would_move"
        );
    }

    #[test]
    fn placement_credits_affinity_with_the_configured_basis() {
        let selected = trusted_inventory(vec![store_hash(1, &[1, 2, 3, 4])]);
        let alternative = trusted_inventory(vec![
            store_hash(1, &[1, 2, 3, 4]),
            store_child(2, 1, &[5, 6, 7, 8]),
        ]);
        let shadow = ExactRouteShadow::new(
            Arc::from([selected, alternative]),
            Arc::new(Metrics::new(&Registry::new()).unwrap()),
            1.0,
            8,
            estimator(),
        )
        .with_router_scoring(AffinityBasis::Marginal, &[], false);
        let mut route = decision();
        shadow.route_pre_route(
            Endpoint::Chat,
            &[1, 2, 3, 4, 5, 6, 7, 8],
            1_024,
            &mut route,
            policy(),
            ExactPlacementMode::Placement,
        );
        assert_eq!(route.candidates, [1, 0]);
        // Overlap stays absolute; affinity is credited above the peer floor.
        assert_eq!(route.candidate_state[0].overlap_blocks, 8);
        assert_eq!(route.candidate_state[0].affinity_blocks, 4);
        assert_eq!(route.candidate_state[1].overlap_blocks, 4);
        assert_eq!(route.candidate_state[1].affinity_blocks, 0);
        assert_eq!(route.affinity_blocks, 4);
    }
}
