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

/// What a replay response proves about retained history before its first
/// returned batch.
///
/// vLLM answers a replay request with every batch still in its ring buffer
/// whose sequence is at or after the requested start, in order. Missing
/// sequence numbers *between* returned batches are therefore no-op scheduler
/// steps. A missing *leading* interval is ambiguous: it is either no-op steps
/// or batches the ring evicted. Only the evidence below distinguishes them.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReplayCoverage {
    /// A complete response to one replay request. `retained_batches` counts
    /// every batch the publisher returned, including tail batches newer than
    /// the requested interval. Eviction of a batch at or after the requested
    /// start leaves the ring full of newer batches, so a response smaller
    /// than the publisher's ring capacity proves nothing was evicted.
    Response { retained_batches: usize },
    /// Batches that continue a response whose earlier batches were already
    /// accepted. Nothing can be missing between batches of one response.
    Continuation,
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
    publisher_capacity: Option<u64>,
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
            publisher_capacity: None,
        }
    }

    /// Declare the publisher's replay ring capacity (vLLM `buffer_steps`).
    ///
    /// Without it, a replay whose first batch is newer than the requested
    /// start can only be trusted when it also retains an all-blocks clear.
    #[must_use]
    pub const fn with_publisher_capacity(mut self, capacity: Option<u64>) -> Self {
        self.publisher_capacity = capacity;
        self
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
    ///
    /// The response must end exactly at the requested `through` and be
    /// strictly increasing. vLLM's sequence tracks scheduler steps while the
    /// publisher retains only steps that emitted a KV event, so missing
    /// sequence numbers between returned batches are authoritative no-ops. A
    /// response beginning after the requested start is accepted only when
    /// [`Self::leading_edge_proven`] rules out ring-buffer eviction.
    pub fn accept_replay(
        &mut self,
        sequences: &[u64],
        establishes_boundary: bool,
        coverage: ReplayCoverage,
    ) -> ReplayAction {
        let Some(pending) = self.pending else {
            return ReplayAction::Invalid;
        };
        let valid_bounds = sequences.first().is_some_and(|first| {
            *first == pending.from
                || (*first > pending.from
                    && self.leading_edge_proven(establishes_boundary, coverage))
        }) && sequences.last() == Some(&pending.through);
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

    /// Whether the interval before a replay's first returned batch can only
    /// have been no-op scheduler steps, or no longer matters.
    ///
    /// * A retained all-blocks clear makes every earlier batch irrelevant, and
    ///   an evicted batch is necessarily older than every retained one.
    /// * A continuation of an accepted response cannot omit a retained batch.
    /// * A response with fewer batches than the ring holds proves that no
    ///   batch at or after the requested start was evicted.
    ///
    /// Anything else, including an unknown ring capacity, fails closed.
    const fn leading_edge_proven(
        &self,
        establishes_boundary: bool,
        coverage: ReplayCoverage,
    ) -> bool {
        if establishes_boundary {
            return true;
        }
        match coverage {
            ReplayCoverage::Continuation => true,
            ReplayCoverage::Response { retained_batches } => match self.publisher_capacity {
                Some(capacity) => (retained_batches as u64) < capacity,
                None => false,
            },
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
    #[must_use]
    pub const fn can_prepare_full_replay(&self, through: u64) -> bool {
        match through.checked_add(1) {
            Some(next_sequence) => next_sequence <= self.replay_limit,
            None => false,
        }
    }

    pub fn prepare_full_replay(&mut self, through: u64) -> bool {
        if !self.can_prepare_full_replay(through) {
            return false;
        }
        let next_sequence = through + 1;
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

    const fn response(retained_batches: usize) -> ReplayCoverage {
        ReplayCoverage::Response { retained_batches }
    }

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
            fence.accept_replay(&[0, 1, 2, 3], false, response(4)),
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
            fence.accept_replay(&[0, 1, 2, 3], false, response(4)),
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
        assert_eq!(
            fence.accept_replay(&[1, 3], false, response(2)),
            ReplayAction::Recovered
        );
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
        assert_eq!(
            fence.accept_replay(&[1], false, response(1)),
            ReplayAction::Invalid
        );
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
            fence.accept_replay(&[11, 12, 13], false, response(3)),
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
            fence.accept_replay(&[11, 12], true, response(2)),
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

    #[test]
    fn startup_replay_missing_leading_history_fails_closed_without_capacity() {
        let mut fence = KvEventFence::new(16);
        assert_eq!(
            fence.ingest(3, false),
            IngestAction::Replay {
                from: 0,
                through: 3
            }
        );
        let generation = fence.generation();
        // Sequences 0 and 1 may be no-op steps or evicted ring entries; with
        // no ring capacity the replay cannot tell them apart.
        assert_eq!(
            fence.accept_replay(&[2, 3], false, response(2)),
            ReplayAction::Invalid
        );
        assert!(!fence.trusted());
        assert!(fence.generation() > generation);
    }

    #[test]
    fn evicting_ring_cannot_restore_trust() {
        // A two-entry ring that has evicted sequence 0 returns exactly its
        // capacity: eviction cannot be ruled out.
        let mut fence = KvEventFence::new(16).with_publisher_capacity(Some(2));
        fence.ingest(3, false);
        assert_eq!(
            fence.accept_replay(&[2, 3], false, response(2)),
            ReplayAction::Invalid
        );
        assert!(!fence.trusted());

        // A tail newer than `through` also occupies the ring.
        let mut fence = KvEventFence::new(16).with_publisher_capacity(Some(3));
        fence.ingest(3, false);
        assert_eq!(
            fence.accept_replay(&[2, 3], false, response(3)),
            ReplayAction::Invalid
        );
    }

    #[test]
    fn under_capacity_response_proves_sparse_leading_steps() {
        let mut fence = KvEventFence::new(16).with_publisher_capacity(Some(4));
        fence.ingest(3, false);
        assert_eq!(
            fence.accept_replay(&[2, 3], false, response(3)),
            ReplayAction::Recovered
        );
        assert!(fence.trusted());
        assert_eq!(fence.ingest(4, false), IngestAction::Apply);
    }

    #[test]
    fn incremental_gap_with_missing_leading_sequence_fails_closed() {
        let mut fence = KvEventFence::new(16);
        fence.ingest(0, true);
        assert_eq!(
            fence.ingest(5, false),
            IngestAction::Replay {
                from: 1,
                through: 5
            }
        );
        assert_eq!(
            fence.accept_replay(&[3, 5], false, response(2)),
            ReplayAction::Invalid
        );
        assert!(!fence.trusted());
    }

    #[test]
    fn retained_clear_makes_leading_eviction_irrelevant() {
        let mut fence = KvEventFence::new(16);
        fence.ingest(6, false);
        let generation = fence.generation();
        assert_eq!(
            fence.accept_replay(&[4, 5, 6], true, response(3)),
            ReplayAction::Recovered
        );
        assert!(fence.trusted());
        assert!(fence.generation() > generation);
    }

    #[test]
    fn continuation_of_an_accepted_response_keeps_sparse_semantics() {
        let mut fence = KvEventFence::new(16);
        fence.ingest(0, true);
        fence.ingest(1, false);
        assert_eq!(
            fence.ingest(4, false),
            IngestAction::Replay {
                from: 2,
                through: 4
            }
        );
        assert_eq!(
            fence.accept_replay(&[4], false, ReplayCoverage::Continuation),
            ReplayAction::Recovered
        );
        assert!(fence.trusted());
    }
}
