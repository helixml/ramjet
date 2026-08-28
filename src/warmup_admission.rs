use std::time::Duration;

use parking_lot::Mutex;

use crate::config::WarmupAdmissionMode;

#[derive(Clone, Copy, Debug)]
pub struct WarmupAdmissionConfig {
    pub mode: WarmupAdmissionMode,
    pub consecutive_successes: usize,
    pub stable_for: Duration,
    pub replicas: usize,
}

#[derive(Clone, Copy, Debug)]
struct ReplicaState {
    ready: bool,
    consecutive_successes: usize,
    first_success: Option<Duration>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WarmupAdmissionOutcome {
    Stable,
    Pending,
    Admitted,
    Reset,
    Serving,
}

impl WarmupAdmissionOutcome {
    pub const ALL: [Self; 5] = [
        Self::Stable,
        Self::Pending,
        Self::Admitted,
        Self::Reset,
        Self::Serving,
    ];

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Stable => "stable",
            Self::Pending => "pending",
            Self::Admitted => "admitted",
            Self::Reset => "reset",
            Self::Serving => "serving",
        }
    }
}

pub struct WarmupAdmission {
    config: WarmupAdmissionConfig,
    replicas: Mutex<Vec<ReplicaState>>,
}

impl WarmupAdmission {
    /// Create process-local passive admission state. Replicas begin admitted:
    /// the policy acts only after this process observes a health loss, avoiding
    /// a fleet-wide outage when the stateless load balancer itself restarts.
    ///
    /// # Panics
    ///
    /// Panics when there are no replicas or the required success count is zero.
    #[must_use]
    pub fn new(config: WarmupAdmissionConfig) -> Self {
        assert!(config.replicas > 0, "warmup admission needs a replica");
        assert!(
            config.consecutive_successes > 0,
            "warmup admission success count must be positive"
        );
        Self {
            config,
            replicas: Mutex::new(vec![
                ReplicaState {
                    ready: true,
                    consecutive_successes: 0,
                    first_success: None,
                };
                config.replicas
            ]),
        }
    }

    pub fn observe_probe(
        &self,
        replica: usize,
        healthy: bool,
        now: Duration,
    ) -> WarmupAdmissionOutcome {
        let mut replicas = self.replicas.lock();
        let Some(state) = replicas.get_mut(replica) else {
            return WarmupAdmissionOutcome::Reset;
        };
        if !healthy {
            state.ready = false;
            state.consecutive_successes = 0;
            state.first_success = None;
            return WarmupAdmissionOutcome::Reset;
        }
        if state.ready {
            return WarmupAdmissionOutcome::Stable;
        }
        let first_success = *state.first_success.get_or_insert(now);
        state.consecutive_successes = state.consecutive_successes.saturating_add(1);
        if state.consecutive_successes >= self.config.consecutive_successes
            && now.saturating_sub(first_success) >= self.config.stable_for
        {
            state.ready = true;
            return WarmupAdmissionOutcome::Admitted;
        }
        WarmupAdmissionOutcome::Pending
    }

    /// Real completed serving work is stronger evidence than passive health.
    /// It admits immediately, including a fail-open request during recovery.
    pub fn observe_serving(&self, replica: usize) -> WarmupAdmissionOutcome {
        let mut replicas = self.replicas.lock();
        let Some(state) = replicas.get_mut(replica) else {
            return WarmupAdmissionOutcome::Reset;
        };
        state.ready = true;
        state.consecutive_successes = 0;
        state.first_success = None;
        WarmupAdmissionOutcome::Serving
    }

    #[must_use]
    pub fn ready(&self, replica: usize) -> bool {
        self.replicas
            .lock()
            .get(replica)
            .is_some_and(|state| state.ready)
    }

    #[must_use]
    pub fn routing_ready(&self, replica: usize) -> bool {
        self.config.mode != WarmupAdmissionMode::Enforce || self.ready(replica)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(mode: WarmupAdmissionMode) -> WarmupAdmission {
        WarmupAdmission::new(WarmupAdmissionConfig {
            mode,
            consecutive_successes: 3,
            stable_for: Duration::from_secs(30),
            replicas: 1,
        })
    }

    #[test]
    fn recovered_replica_needs_both_success_count_and_stable_time() {
        let policy = policy(WarmupAdmissionMode::Enforce);
        assert!(policy.routing_ready(0));
        assert_eq!(
            policy.observe_probe(0, false, Duration::ZERO),
            WarmupAdmissionOutcome::Reset
        );
        assert!(!policy.routing_ready(0));
        assert_eq!(
            policy.observe_probe(0, true, Duration::from_secs(5)),
            WarmupAdmissionOutcome::Pending
        );
        assert_eq!(
            policy.observe_probe(0, true, Duration::from_secs(20)),
            WarmupAdmissionOutcome::Pending
        );
        assert_eq!(
            policy.observe_probe(0, true, Duration::from_secs(35)),
            WarmupAdmissionOutcome::Admitted
        );
        assert!(policy.routing_ready(0));
    }

    #[test]
    fn failure_resets_progress_and_shadow_never_fences() {
        let policy = policy(WarmupAdmissionMode::Shadow);
        policy.observe_probe(0, false, Duration::ZERO);
        policy.observe_probe(0, true, Duration::from_secs(5));
        policy.observe_probe(0, false, Duration::from_secs(10));
        assert_eq!(
            policy.observe_probe(0, true, Duration::from_secs(35)),
            WarmupAdmissionOutcome::Pending
        );
        assert!(policy.routing_ready(0));
        assert!(!policy.ready(0));
    }

    #[test]
    fn real_serving_evidence_admits_immediately() {
        let policy = policy(WarmupAdmissionMode::Enforce);
        policy.observe_probe(0, false, Duration::ZERO);
        assert_eq!(policy.observe_serving(0), WarmupAdmissionOutcome::Serving);
        assert!(policy.routing_ready(0));
    }
}
