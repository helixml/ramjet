//! Fail-closed lifecycle fencing for snapshot bootstrap and live-tail catch-up.
//!
//! This module deliberately owns no transport and no index. A caller first
//! authenticates and decodes a full-engine snapshot, then presents only its
//! lifecycle metadata here. Tail payloads may be committed only when
//! [`TailAction::Apply`] is returned, and an index may be published only when
//! [`CaughtUpAction::Ready`] is returned.

use crate::kv_snapshot::{EngineIncarnation, ResetScope};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SnapshotTailState {
    AwaitingSnapshot,
    CatchingUp,
    Ready,
    Fenced,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SnapshotTailFenceReason {
    UnauthenticatedSnapshot,
    StaleSnapshot,
    UnsupportedResetScope,
    IncarnationChanged,
    DigestKeyChanged,
    GenerationChanged,
    TailGap,
    SequenceOverflow,
    BufferOverflow,
    CaughtUpMismatch,
    Disconnected,
    Cancelled,
    UnexpectedState,
}

impl SnapshotTailFenceReason {
    /// Stable, content-free label suitable for logs and metrics.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::UnauthenticatedSnapshot => "unauthenticated_snapshot",
            Self::StaleSnapshot => "stale_snapshot",
            Self::UnsupportedResetScope => "unsupported_reset_scope",
            Self::IncarnationChanged => "incarnation_changed",
            Self::DigestKeyChanged => "digest_key_changed",
            Self::GenerationChanged => "generation_changed",
            Self::TailGap => "tail_gap",
            Self::SequenceOverflow => "sequence_overflow",
            Self::BufferOverflow => "buffer_overflow",
            Self::CaughtUpMismatch => "caught_up_mismatch",
            Self::Disconnected => "disconnected",
            Self::Cancelled => "cancelled",
            Self::UnexpectedState => "unexpected_state",
        }
    }
}

/// Public lifecycle state. It intentionally excludes engine identity, key ID,
/// event sequences, and snapshot/index content.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SnapshotTailStatus {
    pub revision: u64,
    pub generation: u64,
    pub state: SnapshotTailState,
    pub reason: Option<SnapshotTailFenceReason>,
}

/// Identity carried by an authenticated snapshot or live-tail control plane.
#[derive(Clone, Copy, Debug)]
pub struct SnapshotTailIdentity<'a> {
    pub engine_incarnation: &'a EngineIncarnation,
    pub digest_key_id: &'a [u8],
    pub generation: u64,
}

#[derive(Clone, Copy, Debug)]
pub struct AuthenticatedSnapshot<'a> {
    pub identity: SnapshotTailIdentity<'a>,
    pub watermark: u64,
    pub reset_scope: ResetScope,
    /// Must be set only after the snapshot frame and its channel/session have
    /// both been authenticated by the transport layer.
    pub authenticated: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SnapshotAction {
    Accepted { watermark: u64 },
    Fenced(SnapshotTailFenceReason),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TailAction {
    Apply { sequence: u64 },
    Duplicate,
    Fenced(SnapshotTailFenceReason),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CaughtUpAction {
    Ready,
    AlreadyReady,
    Fenced(SnapshotTailFenceReason),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IdentityAction {
    Current,
    Fenced(SnapshotTailFenceReason),
}

/// One bootstrap generation's fail-closed snapshot/tail state machine.
#[derive(Clone, Debug)]
pub struct SnapshotTailFence {
    expected_incarnation: EngineIncarnation,
    expected_digest_key_id: Vec<u8>,
    minimum_watermark: u64,
    applied_sequence: Option<u64>,
    revision: u64,
    generation: u64,
    state: SnapshotTailState,
    reason: Option<SnapshotTailFenceReason>,
}

impl SnapshotTailFence {
    #[must_use]
    pub fn start_bootstrap(
        expected_incarnation: EngineIncarnation,
        minimum_watermark: u64,
        expected_digest_key_id: Vec<u8>,
        generation: u64,
    ) -> Self {
        Self {
            expected_incarnation,
            expected_digest_key_id,
            minimum_watermark,
            applied_sequence: None,
            revision: 1,
            generation,
            state: SnapshotTailState::AwaitingSnapshot,
            reason: None,
        }
    }

    #[must_use]
    pub const fn status(&self) -> SnapshotTailStatus {
        SnapshotTailStatus {
            revision: self.revision,
            generation: self.generation,
            state: self.state,
            reason: self.reason,
        }
    }

    /// Validate an already-decoded snapshot before any snapshot records are
    /// committed to the private catch-up index.
    pub fn accept_snapshot(&mut self, snapshot: AuthenticatedSnapshot<'_>) -> SnapshotAction {
        if let Some(reason) = self.reason {
            return SnapshotAction::Fenced(reason);
        }
        if self.state != SnapshotTailState::AwaitingSnapshot {
            return SnapshotAction::Fenced(self.fence(SnapshotTailFenceReason::UnexpectedState));
        }
        if !snapshot.authenticated {
            return SnapshotAction::Fenced(
                self.fence(SnapshotTailFenceReason::UnauthenticatedSnapshot),
            );
        }
        if let Err(reason) = self.check_identity(snapshot.identity) {
            return SnapshotAction::Fenced(self.fence(reason));
        }
        if snapshot.reset_scope != ResetScope::full_engine() {
            return SnapshotAction::Fenced(
                self.fence(SnapshotTailFenceReason::UnsupportedResetScope),
            );
        }
        if snapshot.watermark < self.minimum_watermark {
            return SnapshotAction::Fenced(self.fence(SnapshotTailFenceReason::StaleSnapshot));
        }
        if snapshot.watermark == u64::MAX {
            return SnapshotAction::Fenced(self.fence(SnapshotTailFenceReason::SequenceOverflow));
        }

        self.applied_sequence = Some(snapshot.watermark);
        self.state = SnapshotTailState::CatchingUp;
        self.bump_revision();
        SnapshotAction::Accepted {
            watermark: snapshot.watermark,
        }
    }

    /// Sequence one buffered or live event. Identity is checked even for a
    /// duplicate so a stale stream can never hide a generation/key change.
    pub fn accept_tail(&mut self, identity: SnapshotTailIdentity<'_>, sequence: u64) -> TailAction {
        if let Some(reason) = self.reason {
            return TailAction::Fenced(reason);
        }
        if !matches!(
            self.state,
            SnapshotTailState::CatchingUp | SnapshotTailState::Ready
        ) {
            return TailAction::Fenced(self.fence(SnapshotTailFenceReason::UnexpectedState));
        }
        if let Err(reason) = self.check_identity(identity) {
            return TailAction::Fenced(self.fence(reason));
        }

        let Some(applied) = self.applied_sequence else {
            return TailAction::Fenced(self.fence(SnapshotTailFenceReason::UnexpectedState));
        };
        if sequence <= applied {
            return TailAction::Duplicate;
        }
        let Some(expected) = applied.checked_add(1) else {
            return TailAction::Fenced(self.fence(SnapshotTailFenceReason::SequenceOverflow));
        };
        if sequence != expected {
            return TailAction::Fenced(self.fence(SnapshotTailFenceReason::TailGap));
        }
        if sequence == u64::MAX {
            return TailAction::Fenced(self.fence(SnapshotTailFenceReason::SequenceOverflow));
        }

        self.applied_sequence = Some(sequence);
        self.bump_revision();
        TailAction::Apply { sequence }
    }

    /// Publish only when the producer explicitly marks the sequence already
    /// applied by this consumer as caught up.
    pub fn caught_up(
        &mut self,
        identity: SnapshotTailIdentity<'_>,
        sequence: u64,
    ) -> CaughtUpAction {
        if let Some(reason) = self.reason {
            return CaughtUpAction::Fenced(reason);
        }
        if let Err(reason) = self.check_identity(identity) {
            return CaughtUpAction::Fenced(self.fence(reason));
        }
        let Some(applied) = self.applied_sequence else {
            return CaughtUpAction::Fenced(self.fence(SnapshotTailFenceReason::UnexpectedState));
        };
        if sequence != applied {
            return CaughtUpAction::Fenced(self.fence(SnapshotTailFenceReason::CaughtUpMismatch));
        }
        match self.state {
            SnapshotTailState::CatchingUp => {
                self.state = SnapshotTailState::Ready;
                self.bump_revision();
                CaughtUpAction::Ready
            }
            SnapshotTailState::Ready => CaughtUpAction::AlreadyReady,
            SnapshotTailState::AwaitingSnapshot | SnapshotTailState::Fenced => {
                CaughtUpAction::Fenced(self.fence(SnapshotTailFenceReason::UnexpectedState))
            }
        }
    }

    /// Observe a control-plane identity refresh even when no tail payload is
    /// arriving. Rotation or process replacement immediately revokes Ready.
    pub fn observe_identity(&mut self, identity: SnapshotTailIdentity<'_>) -> IdentityAction {
        if let Some(reason) = self.reason {
            return IdentityAction::Fenced(reason);
        }
        match self.check_identity(identity) {
            Ok(()) => IdentityAction::Current,
            Err(reason) => IdentityAction::Fenced(self.fence(reason)),
        }
    }

    pub fn disconnected(&mut self) {
        self.fence(SnapshotTailFenceReason::Disconnected);
    }

    pub fn buffer_overflowed(&mut self) {
        self.fence(SnapshotTailFenceReason::BufferOverflow);
    }

    pub fn cancel(&mut self) {
        self.fence(SnapshotTailFenceReason::Cancelled);
    }

    fn check_identity(
        &self,
        identity: SnapshotTailIdentity<'_>,
    ) -> Result<(), SnapshotTailFenceReason> {
        if identity.generation != self.generation {
            return Err(SnapshotTailFenceReason::GenerationChanged);
        }
        if identity.engine_incarnation != &self.expected_incarnation {
            return Err(SnapshotTailFenceReason::IncarnationChanged);
        }
        if identity.digest_key_id != self.expected_digest_key_id {
            return Err(SnapshotTailFenceReason::DigestKeyChanged);
        }
        Ok(())
    }

    fn fence(&mut self, reason: SnapshotTailFenceReason) -> SnapshotTailFenceReason {
        if self.state != SnapshotTailState::Fenced {
            self.state = SnapshotTailState::Fenced;
            self.reason = Some(reason);
            self.applied_sequence = None;
            self.bump_revision();
        }
        self.reason.unwrap_or(reason)
    }

    fn bump_revision(&mut self) {
        self.revision = self.revision.saturating_add(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kv_snapshot::ResetKind;

    const KEY_ID: &[u8] = b"test-key-id";
    const GENERATION: u64 = 7;

    fn incarnation(suffix: &str) -> EngineIncarnation {
        EngineIncarnation {
            engine_id: format!("engine-{suffix}"),
            model_revision: "revision".into(),
            image_digest: "sha256:image".into(),
            process_started_unix_ns: 42,
            attestation_sha256: vec![3; 32],
        }
    }

    fn machine(floor: u64) -> SnapshotTailFence {
        SnapshotTailFence::start_bootstrap(incarnation("a"), floor, KEY_ID.to_vec(), GENERATION)
    }

    fn identity(engine: &EngineIncarnation) -> SnapshotTailIdentity<'_> {
        SnapshotTailIdentity {
            engine_incarnation: engine,
            digest_key_id: KEY_ID,
            generation: GENERATION,
        }
    }

    fn snapshot(engine: &EngineIncarnation, watermark: u64) -> AuthenticatedSnapshot<'_> {
        AuthenticatedSnapshot {
            identity: identity(engine),
            watermark,
            reset_scope: ResetScope::full_engine(),
            authenticated: true,
        }
    }

    fn assert_fenced(machine: &SnapshotTailFence, reason: SnapshotTailFenceReason) {
        let status = machine.status();
        assert_eq!(status.state, SnapshotTailState::Fenced);
        assert_eq!(status.reason, Some(reason));
    }

    #[test]
    fn stale_snapshot_fences_while_equal_and_new_watermarks_are_accepted() {
        let engine = incarnation("a");

        let mut stale = machine(10);
        assert_eq!(
            stale.accept_snapshot(snapshot(&engine, 9)),
            SnapshotAction::Fenced(SnapshotTailFenceReason::StaleSnapshot)
        );
        assert_fenced(&stale, SnapshotTailFenceReason::StaleSnapshot);

        for watermark in [10, 11] {
            let mut current = machine(10);
            assert_eq!(
                current.accept_snapshot(snapshot(&engine, watermark)),
                SnapshotAction::Accepted { watermark }
            );
            assert_eq!(current.status().state, SnapshotTailState::CatchingUp);
        }
    }

    #[test]
    fn tail_is_strictly_contiguous_and_duplicates_are_side_effect_free() {
        let engine = incarnation("a");
        let mut lifecycle = machine(40);
        lifecycle.accept_snapshot(snapshot(&engine, 40));
        let revision = lifecycle.status().revision;

        assert_eq!(
            lifecycle.accept_tail(identity(&engine), 39),
            TailAction::Duplicate
        );
        assert_eq!(
            lifecycle.accept_tail(identity(&engine), 40),
            TailAction::Duplicate
        );
        assert_eq!(lifecycle.status().revision, revision);
        assert_eq!(
            lifecycle.accept_tail(identity(&engine), 41),
            TailAction::Apply { sequence: 41 }
        );
        assert_eq!(
            lifecycle.accept_tail(identity(&engine), 43),
            TailAction::Fenced(SnapshotTailFenceReason::TailGap)
        );
        assert_fenced(&lifecycle, SnapshotTailFenceReason::TailGap);
    }

    #[test]
    fn ready_requires_exact_explicit_caught_up_marker() {
        let engine = incarnation("a");
        let mut lifecycle = machine(5);
        lifecycle.accept_snapshot(snapshot(&engine, 5));
        lifecycle.accept_tail(identity(&engine), 6);
        assert_eq!(lifecycle.status().state, SnapshotTailState::CatchingUp);
        assert_eq!(
            lifecycle.caught_up(identity(&engine), 6),
            CaughtUpAction::Ready
        );
        assert_eq!(lifecycle.status().state, SnapshotTailState::Ready);
        let revision = lifecycle.status().revision;
        assert_eq!(
            lifecycle.caught_up(identity(&engine), 6),
            CaughtUpAction::AlreadyReady
        );
        assert_eq!(lifecycle.status().revision, revision);
        assert_eq!(
            lifecycle.accept_tail(identity(&engine), 7),
            TailAction::Apply { sequence: 7 }
        );
        assert_eq!(lifecycle.status().state, SnapshotTailState::Ready);
    }

    #[test]
    fn mismatched_caught_up_marker_fences() {
        let engine = incarnation("a");
        for marker in [4, 6] {
            let mut lifecycle = machine(5);
            lifecycle.accept_snapshot(snapshot(&engine, 5));
            assert_eq!(
                lifecycle.caught_up(identity(&engine), marker),
                CaughtUpAction::Fenced(SnapshotTailFenceReason::CaughtUpMismatch)
            );
        }
    }

    #[test]
    fn snapshot_authentication_and_full_reset_scope_are_mandatory() {
        let engine = incarnation("a");
        let mut unauthenticated = snapshot(&engine, 1);
        unauthenticated.authenticated = false;
        let mut lifecycle = machine(1);
        assert_eq!(
            lifecycle.accept_snapshot(unauthenticated),
            SnapshotAction::Fenced(SnapshotTailFenceReason::UnauthenticatedSnapshot)
        );

        let unsupported_scopes = [
            ResetScope {
                kind: ResetKind::DataParallelRank,
                data_parallel_rank: Some(0),
                group_idx: None,
            },
            ResetScope {
                kind: ResetKind::CacheGroup,
                data_parallel_rank: Some(0),
                group_idx: Some(0),
            },
            ResetScope {
                kind: ResetKind::Unsupported,
                data_parallel_rank: None,
                group_idx: None,
            },
            ResetScope {
                kind: ResetKind::FullEngine,
                data_parallel_rank: Some(0),
                group_idx: None,
            },
        ];
        for reset_scope in unsupported_scopes {
            let mut partial = snapshot(&engine, 1);
            partial.reset_scope = reset_scope;
            let mut lifecycle = machine(1);
            assert_eq!(
                lifecycle.accept_snapshot(partial),
                SnapshotAction::Fenced(SnapshotTailFenceReason::UnsupportedResetScope)
            );
        }
    }

    #[test]
    fn every_identity_change_fences_snapshot_and_tail() {
        let expected = incarnation("a");
        let changed = incarnation("b");

        let bad_snapshot_identities = [
            SnapshotTailIdentity {
                engine_incarnation: &changed,
                digest_key_id: KEY_ID,
                generation: GENERATION,
            },
            SnapshotTailIdentity {
                engine_incarnation: &expected,
                digest_key_id: b"changed-key",
                generation: GENERATION,
            },
            SnapshotTailIdentity {
                engine_incarnation: &expected,
                digest_key_id: KEY_ID,
                generation: GENERATION + 1,
            },
        ];
        let reasons = [
            SnapshotTailFenceReason::IncarnationChanged,
            SnapshotTailFenceReason::DigestKeyChanged,
            SnapshotTailFenceReason::GenerationChanged,
        ];
        for (bad_identity, reason) in bad_snapshot_identities.into_iter().zip(reasons) {
            let mut lifecycle = machine(1);
            let mut candidate = snapshot(&expected, 1);
            candidate.identity = bad_identity;
            assert_eq!(
                lifecycle.accept_snapshot(candidate),
                SnapshotAction::Fenced(reason)
            );
        }

        for (bad_identity, reason) in bad_snapshot_identities.into_iter().zip(reasons) {
            let mut lifecycle = machine(1);
            lifecycle.accept_snapshot(snapshot(&expected, 1));
            assert_eq!(
                lifecycle.accept_tail(bad_identity, 1),
                TailAction::Fenced(reason)
            );
        }
    }

    #[test]
    fn identity_refresh_revokes_ready_without_waiting_for_an_event() {
        let expected = incarnation("a");
        let changed = incarnation("b");
        let mut lifecycle = machine(1);
        lifecycle.accept_snapshot(snapshot(&expected, 1));
        lifecycle.caught_up(identity(&expected), 1);
        assert_eq!(
            lifecycle.observe_identity(identity(&expected)),
            IdentityAction::Current
        );
        assert_eq!(
            lifecycle.observe_identity(SnapshotTailIdentity {
                engine_incarnation: &changed,
                digest_key_id: KEY_ID,
                generation: GENERATION,
            }),
            IdentityAction::Fenced(SnapshotTailFenceReason::IncarnationChanged)
        );
    }

    #[test]
    fn disconnect_overflow_and_cancellation_are_terminal_fences() {
        type FenceSignal = fn(&mut SnapshotTailFence);
        let cases: [(FenceSignal, SnapshotTailFenceReason); 3] = [
            (
                SnapshotTailFence::disconnected,
                SnapshotTailFenceReason::Disconnected,
            ),
            (
                SnapshotTailFence::buffer_overflowed,
                SnapshotTailFenceReason::BufferOverflow,
            ),
            (
                SnapshotTailFence::cancel,
                SnapshotTailFenceReason::Cancelled,
            ),
        ];
        for (signal, reason) in cases {
            let mut lifecycle = machine(0);
            let revision = lifecycle.status().revision;
            signal(&mut lifecycle);
            assert_fenced(&lifecycle, reason);
            assert_eq!(lifecycle.status().revision, revision + 1);
            signal(&mut lifecycle);
            assert_eq!(lifecycle.status().revision, revision + 1);
        }
    }

    #[test]
    fn sequence_overflow_fences_snapshot_or_tail() {
        let engine = incarnation("a");
        let mut snapshot_overflow = machine(0);
        assert_eq!(
            snapshot_overflow.accept_snapshot(snapshot(&engine, u64::MAX)),
            SnapshotAction::Fenced(SnapshotTailFenceReason::SequenceOverflow)
        );

        let mut tail_overflow = machine(u64::MAX - 1);
        tail_overflow.accept_snapshot(snapshot(&engine, u64::MAX - 1));
        assert_eq!(
            tail_overflow.accept_tail(identity(&engine), u64::MAX),
            TailAction::Fenced(SnapshotTailFenceReason::SequenceOverflow)
        );
    }

    #[test]
    fn invalid_order_fences_and_first_reason_is_sticky() {
        let engine = incarnation("a");
        let mut lifecycle = machine(0);
        assert_eq!(
            lifecycle.accept_tail(identity(&engine), 0),
            TailAction::Fenced(SnapshotTailFenceReason::UnexpectedState)
        );
        let revision = lifecycle.status().revision;
        lifecycle.cancel();
        assert_eq!(lifecycle.status().revision, revision);
        assert_fenced(&lifecycle, SnapshotTailFenceReason::UnexpectedState);
    }

    #[test]
    fn public_status_contains_only_lifecycle_metadata() {
        let lifecycle = machine(123);
        assert_eq!(
            lifecycle.status(),
            SnapshotTailStatus {
                revision: 1,
                generation: GENERATION,
                state: SnapshotTailState::AwaitingSnapshot,
                reason: None,
            }
        );
        assert_eq!(
            SnapshotTailFenceReason::DigestKeyChanged.as_str(),
            "digest_key_changed"
        );
    }
}
