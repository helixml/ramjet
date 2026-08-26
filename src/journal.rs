use std::{
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde::Serialize;

use crate::{
    config::Config,
    prepare::OutputLimitObservation,
    router::{CandidateState, Decision},
    session_affinity::SessionAffinityObservation,
    tokenizer::CanaryAssignment,
    usage::Accumulator,
};

const VERSION: u8 = 9;

pub struct RouteJournal {
    enabled: bool,
    sequence: AtomicU64,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct RouteAnnotations {
    pub(crate) served_chosen: Option<usize>,
    pub(crate) exact_canary: CanaryAssignment,
    pub(crate) session_affinity: Option<SessionAffinityObservation>,
    pub(crate) output_limit: OutputLimitObservation,
}

#[derive(Debug, Serialize)]
pub struct StartRecord<'a> {
    v: u8,
    event: &'static str,
    seq: u64,
    unix_ms: u128,
    endpoint: &'a str,
    request_bytes: usize,
    total_blocks: usize,
    chosen: Option<usize>,
    served_chosen: Option<usize>,
    outcome: &'static str,
    rotation: usize,
    alpha: f64,
    max_affinity_blocks: usize,
    chunk_bytes: usize,
    load_unit_bytes: usize,
    max_load_units: usize,
    phase_aware_load: bool,
    projected_load: bool,
    score_tie_break: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    exact_canary: Option<CanaryAssignment>,
    #[serde(skip_serializing_if = "Option::is_none")]
    session_affinity: Option<SessionAffinityObservation>,
    output_limit: OutputLimitObservation,
    candidates: &'a [CandidateState],
}

#[derive(Debug, Serialize)]
pub struct FinishRecord<'a> {
    v: u8,
    event: &'static str,
    seq: u64,
    unix_ms: u128,
    result: &'a str,
    upstream: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    request_load_units: Option<usize>,
    status: u16,
    duration_ms: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    first_byte_ms: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ttft_ms: Option<f64>,
    response_bytes: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt_tokens: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cached_tokens: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    completion_tokens: Option<f64>,
}

impl RouteJournal {
    #[must_use]
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            sequence: AtomicU64::new(0),
        }
    }

    pub(crate) fn start(
        &self,
        endpoint: &str,
        request_bytes: usize,
        decision: &Decision,
        config: &Config,
        annotations: RouteAnnotations,
    ) -> Option<u64> {
        if !self.enabled || endpoint == "other" {
            return None;
        }
        let sequence = self.sequence.fetch_add(1, Ordering::Relaxed) + 1;
        let record = Self::start_record(
            sequence,
            endpoint,
            request_bytes,
            decision,
            config,
            annotations,
        );
        emit(&record);
        Some(sequence)
    }

    #[must_use]
    pub(crate) fn start_record<'a>(
        sequence: u64,
        endpoint: &'a str,
        request_bytes: usize,
        decision: &'a Decision,
        config: &Config,
        annotations: RouteAnnotations,
    ) -> StartRecord<'a> {
        StartRecord {
            v: VERSION,
            event: "start",
            seq: sequence,
            unix_ms: unix_millis(),
            endpoint,
            request_bytes,
            total_blocks: decision.total_blocks,
            chosen: decision.candidate_state.first().map(|state| state.index),
            served_chosen: annotations.served_chosen,
            outcome: decision.outcome.label(),
            rotation: decision.rotation,
            alpha: config.route_alpha,
            max_affinity_blocks: config.route_max_overlap_blocks,
            chunk_bytes: config.route_chunk_bytes,
            load_unit_bytes: config.route_load_unit_bytes,
            max_load_units: config.route_max_load_units,
            phase_aware_load: config.route_phase_aware_load,
            projected_load: config.route_projected_load,
            score_tie_break: "overlap",
            exact_canary: (annotations.exact_canary != CanaryAssignment::NotApplicable)
                .then_some(annotations.exact_canary),
            session_affinity: annotations.session_affinity,
            output_limit: annotations.output_limit,
            candidates: &decision.candidate_state,
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn finish(
        &self,
        sequence: Option<u64>,
        elapsed: Duration,
        first_byte: Option<Duration>,
        first_token: Option<Duration>,
        result: &str,
        upstream: Option<usize>,
        request_load_units: Option<usize>,
        status: u16,
        response_bytes: usize,
        usage: &Accumulator,
    ) {
        let Some(sequence) = sequence.filter(|_| self.enabled) else {
            return;
        };
        let record = FinishRecord {
            v: VERSION,
            event: "finish",
            seq: sequence,
            unix_ms: unix_millis(),
            result,
            upstream,
            request_load_units,
            status,
            duration_ms: millis(elapsed),
            first_byte_ms: first_byte.map(millis),
            ttft_ms: first_token.map(millis),
            response_bytes,
            prompt_tokens: usage.prompt,
            cached_tokens: usage.cached,
            completion_tokens: usage.completion,
        };
        emit(&record);
    }
}

fn millis(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1_000.0
}

fn unix_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

fn emit(record: &impl Serialize) {
    match serde_json::to_string(record) {
        // Keep this literal line protocol compatible with route_replay.py.
        // Structured tracing would JSON-escape the record into a message field.
        Ok(encoded) => eprintln!("[route_journal] {encoded}"),
        Err(error) => eprintln!("[route_journal_error] {error}"),
    }
}

#[cfg(test)]
mod tests {
    use url::Url;

    use super::*;
    use crate::{
        config::Affinity,
        prepare::PreparedRequest,
        router::{Outcome, Router, RouterConfig},
        session_affinity::{SessionAffinityObservation, SessionAffinityOutcome},
        shims::Endpoint,
    };

    #[test]
    fn start_record_is_privacy_bounded() {
        let config = Config::from_lookup(|key| {
            (key == "RJ_UPSTREAM")
                .then(|| "http://secret-engine-a:8000,http://secret-engine-b:8000".to_owned())
        })
        .unwrap();
        assert_eq!(config.affinity, Affinity::Prefix);
        let decision = Decision {
            candidates: vec![1, 0],
            candidate_state: vec![CandidateState {
                index: 1,
                rank: 0,
                overlap_blocks: 760,
                affinity_blocks: 32,
                load_units: 0,
                request_load_units: 1,
                healthy: true,
            }],
            overlap_blocks: 760,
            total_blocks: 760,
            affinity_blocks: 32,
            load_units: 1,
            rotation: 1,
            outcome: Outcome::Overlap,
        };
        let router = Router::new(RouterConfig {
            upstreams: vec![Url::parse("http://engine:8000").unwrap()],
            alpha: 4.0,
            chunk_bytes: 64,
            max_prefix_bytes: 2 << 20,
            max_overlap_blocks: 32,
            index_capacity: 100_000,
            load_unit_bytes: 32 << 10,
            max_load_units: 8,
            projected_load: false,
            affinity: Affinity::Prefix,
        });
        let prepared = PreparedRequest::new(
            Endpoint::Chat,
            br#"{"messages":[],"max_completion_tokens":100000,"stream":true}"#,
            100_000,
            &router,
        );
        let encoded = serde_json::to_string(&RouteJournal::start_record(
            42,
            "chat",
            1_555_943,
            &decision,
            &config,
            RouteAnnotations {
                served_chosen: Some(1),
                exact_canary: CanaryAssignment::Treatment,
                session_affinity: Some(SessionAffinityObservation {
                    policy_version: 1,
                    bonus_blocks: 4,
                    max_load_delta: 0,
                    outcome: SessionAffinityOutcome::WouldPreferPrimary,
                    primary: Some(0),
                    secondary: Some(1),
                    target: Some(0),
                }),
                output_limit: prepared.output_limit,
            },
        ))
        .unwrap();
        for forbidden in [
            "secret-engine",
            "http://",
            "prompt",
            "fingerprint",
            "100000",
        ] {
            assert!(
                !encoded.contains(forbidden),
                "leaked {forbidden}: {encoded}"
            );
        }
        assert!(encoded.contains("\"chosen\":1"));
        assert!(encoded.contains("\"served_chosen\":1"));
        assert!(encoded.contains("\"v\":9"));
        assert!(encoded.contains("\"phase_aware_load\":false"));
        assert!(encoded.contains("\"projected_load\":false"));
        assert!(encoded.contains("\"exact_canary\":\"treatment\""));
        assert!(encoded.contains(
            "\"session_affinity\":{\"policy_version\":1,\"bonus_blocks\":4,\"max_load_delta\":0,\"outcome\":\"would_prefer_primary\",\"primary\":0,\"secondary\":1,\"target\":0}"
        ));
        assert!(encoded.contains(
            "\"output_limit\":{\"policy_version\":1,\"requested_bucket\":\"4097_plus\",\"requested_source\":\"max_completion_tokens\",\"effective_bucket\":\"unset\",\"effective_source\":\"none\",\"mutation\":\"max_completion_tokens_stripped\",\"stream_mode\":\"streaming\"}"
        ));
    }

    #[test]
    fn finish_record_exposes_only_the_admitted_reservation() {
        let record = FinishRecord {
            v: VERSION,
            event: "finish",
            seq: 42,
            unix_ms: 1,
            result: "complete",
            upstream: Some(1),
            request_load_units: Some(4),
            status: 200,
            duration_ms: 10.0,
            first_byte_ms: Some(2.0),
            ttft_ms: Some(3.0),
            response_bytes: 128,
            prompt_tokens: Some(8.0),
            cached_tokens: Some(4.0),
            completion_tokens: Some(2.0),
        };

        let encoded = serde_json::to_string(&record).unwrap();
        assert!(encoded.contains("\"v\":9"));
        assert!(encoded.contains("\"upstream\":1"));
        assert!(encoded.contains("\"request_load_units\":4"));
        for forbidden in ["prompt_text", "token_ids", "fingerprint"] {
            assert!(!encoded.contains(forbidden));
        }
    }
}
