//! Bounded replica affinity for reusable structured-decision states.
//!
//! Solomon creates an opaque document state on one replica, then accepts the
//! identifier in later decision requests. The identifier cannot reveal its
//! owner, so ramjet learns the successful replica from a response and keeps a
//! capacity-bounded digest-to-replica map. Raw identifiers are never retained.

use std::num::NonZeroUsize;

use axum::http::Method;
use lru::LruCache;
use parking_lot::Mutex;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::{router::Decision, shims};

const STATE_ID_DOMAIN: &[u8] = b"ramjet structured decision state v1\0";
const MAX_STATE_ID_BYTES: usize = 256;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum StateRequest {
    Create,
    Reuse([u8; 32]),
}

impl StateRequest {
    #[must_use]
    pub(crate) const fn is_reuse(self) -> bool {
        matches!(self, Self::Reuse(_))
    }
}

pub(crate) struct DecisionStateAffinity {
    enabled: bool,
    owners: Mutex<LruCache<[u8; 32], usize>>,
}

impl DecisionStateAffinity {
    #[must_use]
    pub(crate) fn new(enabled: bool, capacity: usize) -> Self {
        Self {
            enabled,
            owners: Mutex::new(LruCache::new(
                NonZeroUsize::new(capacity).expect("positive decision-state capacity"),
            )),
        }
    }

    #[must_use]
    pub(crate) fn request(&self, method: &Method, path: &str, body: &[u8]) -> Option<StateRequest> {
        if !self.enabled || method != Method::POST {
            return None;
        }
        if path == "/states" || path == "/states/" {
            return Some(StateRequest::Create);
        }
        if let Some(state_id) = shims::state_decide_id(path) {
            return state_key(state_id).map(StateRequest::Reuse);
        }
        if path != "/v1/decide" && path != "/v1/decide/" {
            return None;
        }
        // Inline document decisions are the largest requests in this profile.
        // Avoid a second full JSON parse unless the byte spelling of the key
        // needed for reusable-state routing is present. A false positive (for
        // example in document text) only pays the parse; it cannot change the
        // request body or invent an identifier.
        if !body
            .windows(b"\"state_id\"".len())
            .any(|window| window == b"\"state_id\"")
        {
            return Some(StateRequest::Create);
        }
        let value = serde_json::from_slice::<Value>(body).ok()?;
        let object = value.as_object()?;
        match object.get("state_id") {
            Some(Value::String(state_id)) => state_key(state_id).map(StateRequest::Reuse),
            Some(_) => None,
            None => Some(StateRequest::Create),
        }
    }

    /// Prefer the learned owner if it remains eligible to serve.
    pub(crate) fn route(&self, request: Option<StateRequest>, decision: &mut Decision) {
        let Some(StateRequest::Reuse(key)) = request else {
            return;
        };
        let owner = self.owners.lock().get(&key).copied();
        if let Some(owner) = owner {
            decision.prefer_upstream(owner);
        }
    }

    /// A 404 on a state reuse is replica-local evidence, not a terminal model
    /// response. Remove a stale learned owner before trying another replica.
    pub(crate) fn forget_not_found(&self, request: Option<StateRequest>, upstream: usize) {
        let Some(StateRequest::Reuse(key)) = request else {
            return;
        };
        let mut owners = self.owners.lock();
        if owners.peek(&key).copied() == Some(upstream) {
            owners.pop(&key);
        }
    }

    /// Learn only from a successful response to a recognized state operation.
    /// Reuse responses must echo the same identifier that was requested.
    pub(crate) fn learn_response(
        &self,
        request: Option<StateRequest>,
        upstream: usize,
        response: &[u8],
    ) {
        let Some(request) = request else {
            return;
        };
        let Some(id) = serde_json::from_slice::<Value>(response)
            .ok()
            .and_then(|value| value.get("state_id")?.as_str().map(str::to_owned))
        else {
            return;
        };
        let Some(key) = state_key(&id) else {
            return;
        };
        if let StateRequest::Reuse(expected) = request
            && expected != key
        {
            return;
        }
        self.owners.lock().push(key, upstream);
    }

    #[cfg(test)]
    fn owner(&self, state_id: &str) -> Option<usize> {
        self.owners.lock().peek(&state_key(state_id)?).copied()
    }
}

fn state_key(state_id: &str) -> Option<[u8; 32]> {
    if state_id.is_empty()
        || state_id.len() > MAX_STATE_ID_BYTES
        || state_id.bytes().any(|byte| byte.is_ascii_control())
    {
        return None;
    }
    let mut digest = Sha256::new();
    digest.update(STATE_ID_DOMAIN);
    digest.update(state_id.as_bytes());
    Some(digest.finalize().into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::router::{CandidateState, Outcome};

    fn decision() -> Decision {
        Decision {
            candidates: vec![0, 1],
            candidate_state: vec![candidate(0, 0, true), candidate(1, 1, true)],
            overlap_blocks: 0,
            total_blocks: 1,
            affinity_blocks: 0,
            load_units: 1,
            rotation: 0,
            outcome: Outcome::RoundRobin,
        }
    }

    fn candidate(index: usize, rank: usize, healthy: bool) -> CandidateState {
        CandidateState {
            index,
            rank,
            overlap_blocks: 0,
            affinity_blocks: 0,
            load_units: 0,
            request_load_units: 1,
            healthy,
            stale_blocks: 0,
            horizon_ms: None,
            overlap_ages_ms: Vec::new(),
        }
    }

    #[test]
    fn recognizes_create_and_reuse_shapes_without_retaining_raw_ids() {
        let affinity = DecisionStateAffinity::new(true, 4);
        assert_eq!(
            affinity.request(&Method::POST, "/states", br#"{"state":"document"}"#),
            Some(StateRequest::Create)
        );
        assert_eq!(
            affinity.request(
                &Method::POST,
                "/v1/decide",
                br#"{"state":"document","questions":{}}"#,
            ),
            Some(StateRequest::Create)
        );
        let reuse = affinity
            .request(
                &Method::POST,
                "/v1/decide",
                br#"{"state_id":"abc","questions":{}}"#,
            )
            .unwrap();
        assert!(reuse.is_reuse());
        assert_eq!(
            affinity.request(&Method::POST, "/states/abc/decide", br#"{"questions":{}}"#),
            Some(reuse)
        );
    }

    #[test]
    fn learns_promotes_forgets_and_relearns_an_owner() {
        let affinity = DecisionStateAffinity::new(true, 4);
        affinity.learn_response(Some(StateRequest::Create), 1, br#"{"state_id":"abc"}"#);
        assert_eq!(affinity.owner("abc"), Some(1));
        let request = affinity.request(
            &Method::POST,
            "/v1/decide",
            br#"{"state_id":"abc","questions":{}}"#,
        );
        let mut route = decision();
        affinity.route(request, &mut route);
        assert_eq!(route.candidates, [1, 0]);
        assert_eq!(route.outcome, Outcome::StateAffinity);

        affinity.forget_not_found(request, 1);
        assert_eq!(affinity.owner("abc"), None);
        affinity.learn_response(request, 0, br#"{"state_id":"abc"}"#);
        assert_eq!(affinity.owner("abc"), Some(0));
    }

    #[test]
    fn refuses_to_promote_an_unhealthy_owner() {
        let affinity = DecisionStateAffinity::new(true, 4);
        affinity.learn_response(Some(StateRequest::Create), 1, br#"{"state_id":"abc"}"#);
        let mut route = decision();
        route.candidate_state[1].healthy = false;
        let request = affinity.request(
            &Method::POST,
            "/v1/decide",
            br#"{"state_id":"abc","questions":{}}"#,
        );
        affinity.route(request, &mut route);
        assert_eq!(route.candidates, [0, 1]);
    }

    #[test]
    fn capacity_evicts_the_least_recently_used_binding() {
        let affinity = DecisionStateAffinity::new(true, 1);
        affinity.learn_response(Some(StateRequest::Create), 0, br#"{"state_id":"a"}"#);
        affinity.learn_response(Some(StateRequest::Create), 1, br#"{"state_id":"b"}"#);
        assert_eq!(affinity.owner("a"), None);
        assert_eq!(affinity.owner("b"), Some(1));
    }
}
