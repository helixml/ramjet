use std::{
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde::Serialize;

use crate::{
    config::Config,
    router::{CandidateState, Decision},
    usage::Accumulator,
};

const VERSION: u8 = 3;

pub struct RouteJournal {
    enabled: bool,
    sequence: AtomicU64,
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
    outcome: &'static str,
    rotation: usize,
    alpha: f64,
    max_affinity_blocks: usize,
    chunk_bytes: usize,
    load_unit_bytes: usize,
    max_load_units: usize,
    score_tie_break: &'static str,
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

    pub fn start(
        &self,
        endpoint: &str,
        request_bytes: usize,
        decision: &Decision,
        config: &Config,
    ) -> Option<u64> {
        if !self.enabled || endpoint == "other" {
            return None;
        }
        let sequence = self.sequence.fetch_add(1, Ordering::Relaxed) + 1;
        let record = Self::start_record(sequence, endpoint, request_bytes, decision, config);
        emit(&record);
        Some(sequence)
    }

    #[must_use]
    pub fn start_record<'a>(
        sequence: u64,
        endpoint: &'a str,
        request_bytes: usize,
        decision: &'a Decision,
        config: &Config,
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
            outcome: decision.outcome.label(),
            rotation: decision.rotation,
            alpha: config.route_alpha,
            max_affinity_blocks: config.route_max_overlap_blocks,
            chunk_bytes: config.route_chunk_bytes,
            load_unit_bytes: config.route_load_unit_bytes,
            max_load_units: config.route_max_load_units,
            score_tie_break: "overlap",
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
    use super::*;
    use crate::{config::Affinity, router::Outcome};

    #[test]
    fn start_record_is_privacy_bounded() {
        let config = Config::from_lookup(|key| {
            (key == "DS4_UPSTREAM")
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
        let encoded = serde_json::to_string(&RouteJournal::start_record(
            42, "chat", 1_555_943, &decision, &config,
        ))
        .unwrap();
        for forbidden in ["secret-engine", "http://", "prompt", "fingerprint"] {
            assert!(
                !encoded.contains(forbidden),
                "leaked {forbidden}: {encoded}"
            );
        }
        assert!(encoded.contains("\"chosen\":1"));
        assert!(encoded.contains("\"v\":3"));
    }
}
