//! Sequence, replay, and generation fencing for exact KV-event state.
//!
//! Transport and cache-index implementations sit outside this module. The
//! state machine makes the safety decision: exact inventory starts untrusted,
//! gaps require bounded replay, and a clear/snapshot boundary is the only way
//! an incomplete generation becomes authoritative again.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IngestAction {
    /// Apply this batch to the exact index.
    Apply,
    /// Clear the exact index, apply this batch, and trust the new generation.
    ResetAndApply,
    /// The sequence has already been observed.
    Duplicate,
    /// Keep metrics only; exact state is incomplete and must not route.
    ObserveOnly,
    /// Fetch and apply the inclusive sequence interval before accepting more
    /// live events.
    Replay { from: u64, through: u64 },
    /// The gap exceeded the recovery budget; clear/fence exact state.
    UnrecoverableGap,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReplayAction {
    /// Replay was contiguous and exact routing may resume.
    Recovered,
    /// Replay was contiguous, but the generation was already startup-fenced.
    RecoveredObserveOnly,
    /// Replay was incomplete or out of order; exact state remains fenced.
    Invalid,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PendingReplay {
    from: u64,
    through: u64,
    restore_trust: bool,
}

#[derive(Clone, Debug)]
pub struct KvEventFence {
    next_sequence: Option<u64>,
    trusted: bool,
    generation: u64,
    replay_limit: u64,
    pending: Option<PendingReplay>,
}

impl KvEventFence {
    #[must_use]
    pub const fn new(replay_limit: u64) -> Self {
        Self {
            next_sequence: None,
            trusted: false,
            generation: 0,
            replay_limit,
            pending: None,
        }
    }

    #[must_use]
    pub const fn trusted(&self) -> bool {
        self.trusted && self.pending.is_none()
    }

    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    #[must_use]
    pub const fn next_sequence(&self) -> Option<u64> {
        self.next_sequence
    }

    /// Accept a live event batch sequence.
    ///
    /// `clears_all` must mean the batch establishes an empty authoritative
    /// cache boundary, not merely that one cache group was cleared.
    pub fn ingest(&mut self, sequence: u64, clears_all: bool) -> IngestAction {
        if clears_all {
            self.generation = self.generation.saturating_add(1);
            self.next_sequence = Some(sequence.saturating_add(1));
            self.pending = None;
            self.trusted = true;
            return IngestAction::ResetAndApply;
        }

        if let Some(pending) = self.pending {
            if sequence <= pending.through {
                return IngestAction::Duplicate;
            }
            let extended = sequence.saturating_sub(pending.from).saturating_add(1);
            if extended > self.replay_limit {
                self.fence_after(sequence);
                return IngestAction::UnrecoverableGap;
            }
            self.pending = Some(PendingReplay {
                through: sequence,
                ..pending
            });
            self.next_sequence = Some(sequence.saturating_add(1));
            return IngestAction::Replay {
                from: pending.from,
                through: sequence,
            };
        }

        let Some(expected) = self.next_sequence else {
            self.next_sequence = Some(sequence.saturating_add(1));
            return IngestAction::ObserveOnly;
        };
        if sequence < expected {
            return IngestAction::Duplicate;
        }
        if sequence == expected {
            self.next_sequence = Some(sequence.saturating_add(1));
            return if self.trusted {
                IngestAction::Apply
            } else {
                IngestAction::ObserveOnly
            };
        }

        let count = sequence.saturating_sub(expected).saturating_add(1);
        if count > self.replay_limit {
            self.fence_after(sequence);
            return IngestAction::UnrecoverableGap;
        }
        let restore_trust = self.trusted;
        self.trusted = false;
        self.pending = Some(PendingReplay {
            from: expected,
            through: sequence,
            restore_trust,
        });
        self.next_sequence = Some(sequence.saturating_add(1));
        IngestAction::Replay {
            from: expected,
            through: sequence,
        }
    }

    /// Validate a replay response before its payloads are committed.
    pub fn accept_replay(&mut self, sequences: &[u64]) -> ReplayAction {
        let Some(pending) = self.pending else {
            return ReplayAction::Invalid;
        };
        let expected_len = pending
            .through
            .saturating_sub(pending.from)
            .saturating_add(1);
        let valid_len = usize::try_from(expected_len)
            .ok()
            .is_some_and(|expected| expected == sequences.len());
        let valid_order = sequences.iter().copied().eq(pending.from..=pending.through);
        if !valid_len || !valid_order {
            self.pending = None;
            self.generation = self.generation.saturating_add(1);
            self.trusted = false;
            return ReplayAction::Invalid;
        }
        self.pending = None;
        self.trusted = pending.restore_trust;
        if self.trusted {
            ReplayAction::Recovered
        } else {
            ReplayAction::RecoveredObserveOnly
        }
    }

    /// Fence state when the engine process/cache generation changes.
    pub fn generation_changed(&mut self) {
        self.generation = self.generation.saturating_add(1);
        self.next_sequence = None;
        self.pending = None;
        self.trusted = false;
    }

    fn fence_after(&mut self, sequence: u64) {
        self.generation = self.generation.saturating_add(1);
        self.next_sequence = Some(sequence.saturating_add(1));
        self.pending = None;
        self.trusted = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_is_observation_only_until_authoritative_clear() {
        let mut fence = KvEventFence::new(16);
        assert_eq!(fence.ingest(40, false), IngestAction::ObserveOnly);
        assert_eq!(fence.ingest(41, false), IngestAction::ObserveOnly);
        assert!(!fence.trusted());
        assert_eq!(fence.ingest(42, true), IngestAction::ResetAndApply);
        assert!(fence.trusted());
        assert_eq!(fence.ingest(43, false), IngestAction::Apply);
    }

    #[test]
    fn bounded_contiguous_replay_restores_trust() {
        let mut fence = KvEventFence::new(8);
        assert_eq!(fence.ingest(0, true), IngestAction::ResetAndApply);
        assert_eq!(
            fence.ingest(3, false),
            IngestAction::Replay {
                from: 1,
                through: 3
            }
        );
        assert!(!fence.trusted());
        assert_eq!(fence.accept_replay(&[1, 2, 3]), ReplayAction::Recovered);
        assert!(fence.trusted());
        assert_eq!(fence.ingest(4, false), IngestAction::Apply);
    }

    #[test]
    fn invalid_or_oversized_replay_fences_generation() {
        let mut fence = KvEventFence::new(3);
        fence.ingest(0, true);
        assert_eq!(
            fence.ingest(2, false),
            IngestAction::Replay {
                from: 1,
                through: 2
            }
        );
        assert_eq!(fence.accept_replay(&[2]), ReplayAction::Invalid);
        assert!(!fence.trusted());
        let generation = fence.generation();
        assert_eq!(fence.ingest(10, false), IngestAction::UnrecoverableGap);
        assert!(fence.generation() > generation);
        assert!(!fence.trusted());
    }

    #[test]
    fn live_events_extend_pending_replay_without_restoring_startup_trust() {
        let mut fence = KvEventFence::new(8);
        fence.ingest(10, false);
        assert_eq!(
            fence.ingest(12, false),
            IngestAction::Replay {
                from: 11,
                through: 12
            }
        );
        assert_eq!(
            fence.ingest(13, false),
            IngestAction::Replay {
                from: 11,
                through: 13
            }
        );
        assert_eq!(
            fence.accept_replay(&[11, 12, 13]),
            ReplayAction::RecoveredObserveOnly
        );
        assert!(!fence.trusted());
    }

    #[test]
    fn restart_and_clear_form_generation_fences() {
        let mut fence = KvEventFence::new(8);
        fence.ingest(5, true);
        let generation = fence.generation();
        fence.generation_changed();
        assert!(fence.generation() > generation);
        assert!(!fence.trusted());
        assert_eq!(fence.next_sequence(), None);
        assert_eq!(fence.ingest(0, true), IngestAction::ResetAndApply);
        assert!(fence.trusted());
    }
}
