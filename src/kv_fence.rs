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
            if sequence == 0 {
                // vLLM's publisher sequence is process-local and starts at
                // zero. Seeing its first batch proves the initially empty
                // generation has been observed in full.
                self.trusted = true;
                return IngestAction::Apply;
            }
            let count = sequence.saturating_add(1);
            if count <= self.replay_limit {
                // A retained replay beginning at zero is equivalent to a full
                // generation snapshot even when it contains no explicit clear.
                self.pending = Some(PendingReplay {
                    from: 0,
                    through: sequence,
                    restore_trust: true,
                });
                return IngestAction::Replay {
                    from: 0,
                    through: sequence,
                };
            }
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
    pub fn accept_replay(&mut self, sequences: &[u64], establishes_boundary: bool) -> ReplayAction {
        let Some(pending) = self.pending else {
            return ReplayAction::Invalid;
        };
        // vLLM's sequence tracks scheduler steps while the publisher retains
        // only steps that emitted a KV event. Missing sequence numbers are
        // therefore authoritative no-ops, not missing replay messages.
        let valid_bounds = sequences
            .first()
            .is_some_and(|first| *first >= pending.from)
            && sequences.last() == Some(&pending.through);
        let strictly_increasing = sequences.windows(2).all(|pair| pair[0] < pair[1]);
        if !valid_bounds || !strictly_increasing {
            self.pending = None;
            self.generation = self.generation.saturating_add(1);
            self.trusted = false;
            return ReplayAction::Invalid;
        }
        self.pending = None;
        if establishes_boundary {
            self.generation = self.generation.saturating_add(1);
        }
        self.trusted = pending.restore_trust || establishes_boundary;
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

    /// Re-arm a bounded full replay after reconnecting with a fresh transport.
    ///
    /// The caller must already have discarded the prior generation's index.
    /// Only a complete range beginning at sequence zero can establish trust
    /// without retaining any state from the disconnected generation.
    pub fn prepare_full_replay(&mut self, through: u64) -> bool {
        let Some(next_sequence) = through.checked_add(1) else {
            return false;
        };
        if next_sequence > self.replay_limit {
            return false;
        }
        self.next_sequence = Some(next_sequence);
        self.pending = Some(PendingReplay {
            from: 0,
            through,
            restore_trust: true,
        });
        self.trusted = false;
        true
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
    fn startup_sequence_zero_establishes_authoritative_generation() {
        let mut fence = KvEventFence::new(16);
        assert_eq!(fence.ingest(0, false), IngestAction::Apply);
        assert!(fence.trusted());
        assert_eq!(fence.ingest(1, false), IngestAction::Apply);
    }

    #[test]
    fn startup_replays_from_zero_to_recover_complete_generation() {
        let mut fence = KvEventFence::new(16);
        assert_eq!(
            fence.ingest(3, false),
            IngestAction::Replay {
                from: 0,
                through: 3
            }
        );
        assert!(!fence.trusted());
        assert_eq!(
            fence.accept_replay(&[0, 1, 2, 3], false),
            ReplayAction::Recovered
        );
        assert!(fence.trusted());
    }

    #[test]
    fn reconnect_can_rearm_only_a_bounded_full_replay() {
        let mut fence = KvEventFence::new(4);
        fence.generation_changed();
        assert!(fence.prepare_full_replay(3));
        assert!(!fence.trusted());
        assert_eq!(
            fence.accept_replay(&[0, 1, 2, 3], false),
            ReplayAction::Recovered
        );
        assert!(fence.trusted());

        fence.generation_changed();
        assert!(!fence.prepare_full_replay(4));
        assert!(!fence.prepare_full_replay(u64::MAX));
        assert!(!fence.trusted());
    }

    #[test]
    fn bounded_sparse_replay_restores_trust() {
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
        assert_eq!(fence.accept_replay(&[1, 3], false), ReplayAction::Recovered);
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
        assert_eq!(fence.accept_replay(&[1], false), ReplayAction::Invalid);
        assert!(!fence.trusted());
        let generation = fence.generation();
        assert_eq!(fence.ingest(10, false), IngestAction::UnrecoverableGap);
        assert!(fence.generation() > generation);
        assert!(!fence.trusted());
    }

    #[test]
    fn live_events_extend_pending_replay_without_restoring_startup_trust() {
        let mut fence = KvEventFence::new(8);
        // Too old for a bounded startup replay, so this generation remains
        // observation-only before the later gap.
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
            fence.accept_replay(&[11, 12, 13], false),
            ReplayAction::RecoveredObserveOnly
        );
        assert!(!fence.trusted());
    }

    #[test]
    fn clear_inside_replay_establishes_an_authoritative_generation() {
        let mut fence = KvEventFence::new(8);
        fence.ingest(10, false);
        assert_eq!(
            fence.ingest(12, false),
            IngestAction::Replay {
                from: 11,
                through: 12
            }
        );
        let generation = fence.generation();
        assert_eq!(
            fence.accept_replay(&[11, 12], true),
            ReplayAction::Recovered
        );
        assert!(fence.trusted());
        assert!(fence.generation() > generation);
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
