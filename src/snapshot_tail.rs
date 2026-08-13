//! Fail-closed lifecycle fencing for snapshot bootstrap and live-tail catch-up.
//!
//! This module owns no transport or index. Snapshot input is the opaque result
//! of session authentication. Tail, caught-up, and identity frames are also
//! opaque and have crate-private constructors: a future session decoder is the
//! only intended production caller. Until that decoder exists this foundation
//! is deliberately unwired and is not deployable.
//!
//! The companion's contiguous `delivery_sequence` is distinct from vLLM's
//! sparse real-event watermark. Strict `+1` checks apply only to delivery
//! sequence; a jump in real-event watermark is valid because scheduler steps
//! without KV events are not published.

use crate::{
    kv_snapshot::{EngineIncarnation, ResetScope},
    snapshot_session::AuthenticatedSnapshot,
};

const SNAPSHOT_DELIVERY_SEQUENCE: u64 = 0;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SnapshotTailState {
    AwaitingSnapshot,
    CatchingUp,
    Ready,
    Fenced,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SnapshotTailFenceReason {
    StaleSnapshot,
    UnsupportedResetScope,
    IncarnationChanged,
    DigestKeyChanged,
    GenerationChanged,
    TailGap,
    EventWatermarkRegression,
    SequenceOverflow,
    BufferOverflow,
    CaughtUpMismatch,
    Disconnected,
    Cancelled,
    ApplicationFailed,
    UnexpectedState,
}

impl SnapshotTailFenceReason {
    /// Stable, content-free label suitable for logs and metrics.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::StaleSnapshot => "stale_snapshot",
            Self::UnsupportedResetScope => "unsupported_reset_scope",
            Self::IncarnationChanged => "incarnation_changed",
            Self::DigestKeyChanged => "digest_key_changed",
            Self::GenerationChanged => "generation_changed",
            Self::TailGap => "tail_gap",
            Self::EventWatermarkRegression => "event_watermark_regression",
            Self::SequenceOverflow => "sequence_overflow",
            Self::BufferOverflow => "buffer_overflow",
            Self::CaughtUpMismatch => "caught_up_mismatch",
            Self::Disconnected => "disconnected",
            Self::Cancelled => "cancelled",
            Self::ApplicationFailed => "application_failed",
            Self::UnexpectedState => "unexpected_state",
        }
    }
}

/// Public lifecycle state. It intentionally excludes engine identity, key ID,
/// event/delivery sequences, and snapshot/index content.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SnapshotTailStatus {
    pub revision: u64,
    pub generation: u64,
    pub state: SnapshotTailState,
    pub reason: Option<SnapshotTailFenceReason>,
}

#[derive(Clone, Copy)]
struct AuthenticatedIdentity<'a> {
    engine_incarnation: &'a EngineIncarnation,
    digest_key_id: &'a [u8],
    generation: u64,
}

/// Opaque output expected from the future authenticated tail-frame decoder.
/// No public constructor exists, so callers outside the crate cannot assert
/// that arbitrary metadata was authenticated.
pub struct AuthenticatedTailFrame<'a> {
    identity: AuthenticatedIdentity<'a>,
    delivery_sequence: u64,
    event_watermark: u64,
}

impl<'a> AuthenticatedTailFrame<'a> {
    #[allow(dead_code)]
    pub(crate) const fn from_authenticated_session(
        engine_incarnation: &'a EngineIncarnation,
        digest_key_id: &'a [u8],
        generation: u64,
        delivery_sequence: u64,
        event_watermark: u64,
    ) -> Self {
        Self {
            identity: AuthenticatedIdentity {
                engine_incarnation,
                digest_key_id,
                generation,
            },
            delivery_sequence,
            event_watermark,
        }
    }
}

/// Opaque explicit end-of-tail marker from an authenticated session frame.
pub struct AuthenticatedCaughtUpFrame<'a> {
    identity: AuthenticatedIdentity<'a>,
    delivery_sequence: u64,
    event_watermark: u64,
}

impl<'a> AuthenticatedCaughtUpFrame<'a> {
    #[allow(dead_code)]
    pub(crate) const fn from_authenticated_session(
        engine_incarnation: &'a EngineIncarnation,
        digest_key_id: &'a [u8],
        generation: u64,
        delivery_sequence: u64,
        event_watermark: u64,
    ) -> Self {
        Self {
            identity: AuthenticatedIdentity {
                engine_incarnation,
                digest_key_id,
                generation,
            },
            delivery_sequence,
            event_watermark,
        }
    }
}

/// Opaque authenticated control-plane identity refresh.
pub struct AuthenticatedIdentityFrame<'a> {
    identity: AuthenticatedIdentity<'a>,
}

impl<'a> AuthenticatedIdentityFrame<'a> {
    #[allow(dead_code)]
    pub(crate) const fn from_authenticated_session(
        engine_incarnation: &'a EngineIncarnation,
        digest_key_id: &'a [u8],
        generation: u64,
    ) -> Self {
        Self {
            identity: AuthenticatedIdentity {
                engine_incarnation,
                digest_key_id,
                generation,
            },
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SnapshotAction {
    Accepted {
        snapshot_watermark: u64,
        delivery_sequence: u64,
    },
    Fenced(SnapshotTailFenceReason),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TailAction {
    Apply {
        delivery_sequence: u64,
        event_watermark: u64,
    },
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
    minimum_snapshot_watermark: u64,
    applied_delivery_sequence: Option<u64>,
    applied_event_watermark: Option<u64>,
    revision: u64,
    generation: u64,
    state: SnapshotTailState,
    reason: Option<SnapshotTailFenceReason>,
}

impl SnapshotTailFence {
    #[must_use]
    pub fn start_bootstrap(
        expected_incarnation: EngineIncarnation,
        minimum_snapshot_watermark: u64,
        expected_digest_key_id: Vec<u8>,
        generation: u64,
    ) -> Self {
        Self {
            expected_incarnation,
            expected_digest_key_id,
            minimum_snapshot_watermark,
            applied_delivery_sequence: None,
            applied_event_watermark: None,
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

    /// Validate session-authenticated snapshot metadata before committing its
    /// records to a private catch-up index. The decoded snapshot's reset scope
    /// is supplied separately because it is part of the snapshot body.
    pub(crate) fn accept_snapshot(
        &mut self,
        snapshot: &AuthenticatedSnapshot,
        reset_scope: ResetScope,
    ) -> SnapshotAction {
        if let Some(reason) = self.reason {
            return SnapshotAction::Fenced(reason);
        }
        if self.state != SnapshotTailState::AwaitingSnapshot {
            return SnapshotAction::Fenced(self.fence(SnapshotTailFenceReason::UnexpectedState));
        }
        let identity = AuthenticatedIdentity {
            engine_incarnation: snapshot.engine_incarnation(),
            digest_key_id: snapshot.digest_key_id(),
            generation: snapshot.companion_generation(),
        };
        if let Err(reason) = self.check_identity(identity) {
            return SnapshotAction::Fenced(self.fence(reason));
        }
        if reset_scope != ResetScope::full_engine() {
            return SnapshotAction::Fenced(
                self.fence(SnapshotTailFenceReason::UnsupportedResetScope),
            );
        }
        let watermark = snapshot.snapshot_watermark();
        if watermark < self.minimum_snapshot_watermark {
            return SnapshotAction::Fenced(self.fence(SnapshotTailFenceReason::StaleSnapshot));
        }

        // The authenticated snapshot is delivery item zero. Its real-event
        // watermark is an independent, potentially sparse engine sequence.
        self.applied_delivery_sequence = Some(SNAPSHOT_DELIVERY_SEQUENCE);
        self.applied_event_watermark = Some(watermark);
        self.state = SnapshotTailState::CatchingUp;
        self.bump_revision();
        SnapshotAction::Accepted {
            snapshot_watermark: watermark,
            delivery_sequence: SNAPSHOT_DELIVERY_SEQUENCE,
        }
    }

    /// Sequence one authenticated buffered/live tail frame. Identity is
    /// checked before duplicate handling, so a publisher restart with a lower
    /// delivery sequence cannot masquerade as an old duplicate.
    pub fn accept_tail(&mut self, frame: &AuthenticatedTailFrame<'_>) -> TailAction {
        if let Some(reason) = self.reason {
            return TailAction::Fenced(reason);
        }
        if !matches!(
            self.state,
            SnapshotTailState::CatchingUp | SnapshotTailState::Ready
        ) {
            return TailAction::Fenced(self.fence(SnapshotTailFenceReason::UnexpectedState));
        }
        if let Err(reason) = self.check_identity(frame.identity) {
            return TailAction::Fenced(self.fence(reason));
        }

        let (Some(applied_delivery), Some(applied_event)) =
            (self.applied_delivery_sequence, self.applied_event_watermark)
        else {
            return TailAction::Fenced(self.fence(SnapshotTailFenceReason::UnexpectedState));
        };
        if frame.delivery_sequence <= applied_delivery {
            return TailAction::Duplicate;
        }
        let Some(expected_delivery) = applied_delivery.checked_add(1) else {
            return TailAction::Fenced(self.fence(SnapshotTailFenceReason::SequenceOverflow));
        };
        if frame.delivery_sequence != expected_delivery {
            return TailAction::Fenced(self.fence(SnapshotTailFenceReason::TailGap));
        }
        if frame.delivery_sequence == u64::MAX {
            return TailAction::Fenced(self.fence(SnapshotTailFenceReason::SequenceOverflow));
        }
        if frame.event_watermark <= applied_event {
            return TailAction::Fenced(
                self.fence(SnapshotTailFenceReason::EventWatermarkRegression),
            );
        }

        self.applied_delivery_sequence = Some(frame.delivery_sequence);
        self.applied_event_watermark = Some(frame.event_watermark);
        self.bump_revision();
        TailAction::Apply {
            delivery_sequence: frame.delivery_sequence,
            event_watermark: frame.event_watermark,
        }
    }

    /// Publish only when an authenticated producer marker names the delivery
    /// sequence and real-event watermark already applied by this consumer.
    pub fn caught_up(&mut self, frame: &AuthenticatedCaughtUpFrame<'_>) -> CaughtUpAction {
        if let Some(reason) = self.reason {
            return CaughtUpAction::Fenced(reason);
        }
        if let Err(reason) = self.check_identity(frame.identity) {
            return CaughtUpAction::Fenced(self.fence(reason));
        }
        if self.applied_delivery_sequence != Some(frame.delivery_sequence)
            || self.applied_event_watermark != Some(frame.event_watermark)
        {
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

    /// Authenticated control-plane refresh for immediate revocation even when
    /// no tail payload is arriving.
    pub fn observe_identity(&mut self, frame: &AuthenticatedIdentityFrame<'_>) -> IdentityAction {
        if let Some(reason) = self.reason {
            return IdentityAction::Fenced(reason);
        }
        match self.check_identity(frame.identity) {
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

    /// Fence after an authenticated delta could not be decoded or applied to
    /// the private index. Callers must never publish the partially caught-up
    /// generation after this transition.
    pub fn application_failed(&mut self) {
        self.fence(SnapshotTailFenceReason::ApplicationFailed);
    }

    fn check_identity(
        &self,
        identity: AuthenticatedIdentity<'_>,
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
            self.applied_delivery_sequence = None;
            self.applied_event_watermark = None;
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
    use crate::{
        kv_snapshot::ResetKind,
        snapshot_session::{
            SnapshotSessionBinding, SnapshotSessionChallenge, SnapshotSessionExpectations,
            SnapshotSessionLimits, SnapshotSessionSecret, decode_authenticated_snapshot,
            encode_authenticated_snapshot,
        },
    };

    const CHALLENGE: SnapshotSessionChallenge = SnapshotSessionChallenge::new([0x31; 32]);
    const SECRET_BYTES: [u8; 32] = *b"snapshot-session-secret-32-byte!";
    const KEY_ID: [u8; 32] = [0x6b; 32];
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

    fn snapshot(engine: &EngineIncarnation, watermark: u64) -> AuthenticatedSnapshot {
        let binding = SnapshotSessionBinding {
            challenge: CHALLENGE,
            engine_incarnation: engine,
            snapshot_watermark: watermark,
            digest_key_id: &KEY_ID,
            companion_generation: GENERATION,
        };
        let secret = SnapshotSessionSecret::new(SECRET_BYTES);
        let frame = encode_authenticated_snapshot(
            b"opaque-snapshot",
            binding,
            &secret,
            SnapshotSessionLimits::default(),
        )
        .unwrap();
        decode_authenticated_snapshot(
            &frame,
            SnapshotSessionExpectations {
                challenge: CHALLENGE,
                engine_incarnation: engine,
                digest_key_id: &KEY_ID,
                minimum_snapshot_watermark: watermark,
                minimum_companion_generation: GENERATION,
            },
            &secret,
            SnapshotSessionLimits::default(),
        )
        .unwrap()
    }

    fn tail(
        engine: &EngineIncarnation,
        delivery_sequence: u64,
        event_watermark: u64,
    ) -> AuthenticatedTailFrame<'_> {
        AuthenticatedTailFrame::from_authenticated_session(
            engine,
            &KEY_ID,
            GENERATION,
            delivery_sequence,
            event_watermark,
        )
    }

    fn caught_up(
        engine: &EngineIncarnation,
        delivery_sequence: u64,
        event_watermark: u64,
    ) -> AuthenticatedCaughtUpFrame<'_> {
        AuthenticatedCaughtUpFrame::from_authenticated_session(
            engine,
            &KEY_ID,
            GENERATION,
            delivery_sequence,
            event_watermark,
        )
    }

    fn assert_fenced(machine: &SnapshotTailFence, reason: SnapshotTailFenceReason) {
        assert_eq!(machine.status().state, SnapshotTailState::Fenced);
        assert_eq!(machine.status().reason, Some(reason));
    }

    #[test]
    fn stale_snapshot_fences_while_equal_and_new_watermarks_are_accepted() {
        let engine = incarnation("a");
        let mut stale = machine(10);
        assert_eq!(
            stale.accept_snapshot(&snapshot(&engine, 9), ResetScope::full_engine()),
            SnapshotAction::Fenced(SnapshotTailFenceReason::StaleSnapshot)
        );

        for watermark in [10, 11] {
            let mut current = machine(10);
            assert_eq!(
                current.accept_snapshot(&snapshot(&engine, watermark), ResetScope::full_engine()),
                SnapshotAction::Accepted {
                    snapshot_watermark: watermark,
                    delivery_sequence: 0,
                }
            );
        }
    }

    #[test]
    fn sparse_real_watermarks_are_not_delivery_gaps() {
        let engine = incarnation("a");
        let mut lifecycle = machine(1_000);
        lifecycle.accept_snapshot(&snapshot(&engine, 1_000), ResetScope::full_engine());
        assert_eq!(
            lifecycle.accept_tail(&tail(&engine, 1, 9_000)),
            TailAction::Apply {
                delivery_sequence: 1,
                event_watermark: 9_000,
            }
        );
        assert_eq!(
            lifecycle.accept_tail(&tail(&engine, 2, 100_000)),
            TailAction::Apply {
                delivery_sequence: 2,
                event_watermark: 100_000,
            }
        );
    }

    #[test]
    fn delivery_is_contiguous_and_duplicates_are_side_effect_free() {
        let engine = incarnation("a");
        let mut lifecycle = machine(40);
        lifecycle.accept_snapshot(&snapshot(&engine, 40), ResetScope::full_engine());
        let revision = lifecycle.status().revision;
        assert_eq!(
            lifecycle.accept_tail(&tail(&engine, 0, 40)),
            TailAction::Duplicate
        );
        assert_eq!(lifecycle.status().revision, revision);
        assert!(matches!(
            lifecycle.accept_tail(&tail(&engine, 1, 50)),
            TailAction::Apply { .. }
        ));
        assert_eq!(
            lifecycle.accept_tail(&tail(&engine, 3, 60)),
            TailAction::Fenced(SnapshotTailFenceReason::TailGap)
        );
    }

    #[test]
    fn nonincreasing_event_watermark_fences_only_on_new_delivery() {
        let engine = incarnation("a");
        let mut lifecycle = machine(40);
        lifecycle.accept_snapshot(&snapshot(&engine, 40), ResetScope::full_engine());
        assert_eq!(
            lifecycle.accept_tail(&tail(&engine, 0, 1)),
            TailAction::Duplicate
        );
        assert_eq!(
            lifecycle.accept_tail(&tail(&engine, 1, 40)),
            TailAction::Fenced(SnapshotTailFenceReason::EventWatermarkRegression)
        );
    }

    #[test]
    fn ready_requires_exact_authenticated_caught_up_marker() {
        let engine = incarnation("a");
        let mut lifecycle = machine(5);
        lifecycle.accept_snapshot(&snapshot(&engine, 5), ResetScope::full_engine());
        lifecycle.accept_tail(&tail(&engine, 1, 9));
        assert_eq!(
            lifecycle.caught_up(&caught_up(&engine, 1, 9)),
            CaughtUpAction::Ready
        );
        assert_eq!(lifecycle.status().state, SnapshotTailState::Ready);
        assert_eq!(
            lifecycle.caught_up(&caught_up(&engine, 1, 9)),
            CaughtUpAction::AlreadyReady
        );

        for (delivery, event) in [(0, 9), (1, 8), (2, 9)] {
            let mut mismatch = machine(5);
            mismatch.accept_snapshot(&snapshot(&engine, 5), ResetScope::full_engine());
            mismatch.accept_tail(&tail(&engine, 1, 9));
            assert_eq!(
                mismatch.caught_up(&caught_up(&engine, delivery, event)),
                CaughtUpAction::Fenced(SnapshotTailFenceReason::CaughtUpMismatch)
            );
        }
    }

    #[test]
    fn only_complete_full_engine_reset_scope_is_accepted() {
        let engine = incarnation("a");
        let unsupported = [
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
        for scope in unsupported {
            let mut lifecycle = machine(1);
            assert_eq!(
                lifecycle.accept_snapshot(&snapshot(&engine, 1), scope),
                SnapshotAction::Fenced(SnapshotTailFenceReason::UnsupportedResetScope)
            );
        }
    }

    #[test]
    fn identity_changes_are_checked_before_duplicate_delivery() {
        let expected = incarnation("a");
        let changed = incarnation("b");
        let cases = [
            (
                AuthenticatedTailFrame::from_authenticated_session(
                    &changed, &KEY_ID, GENERATION, 0, 1,
                ),
                SnapshotTailFenceReason::IncarnationChanged,
            ),
            (
                AuthenticatedTailFrame::from_authenticated_session(
                    &expected,
                    &[0x7c; 32],
                    GENERATION,
                    0,
                    1,
                ),
                SnapshotTailFenceReason::DigestKeyChanged,
            ),
            (
                AuthenticatedTailFrame::from_authenticated_session(
                    &expected,
                    &KEY_ID,
                    GENERATION + 1,
                    0,
                    1,
                ),
                SnapshotTailFenceReason::GenerationChanged,
            ),
        ];
        for (frame, reason) in cases {
            let mut lifecycle = machine(1);
            lifecycle.accept_snapshot(&snapshot(&expected, 1), ResetScope::full_engine());
            assert_eq!(lifecycle.accept_tail(&frame), TailAction::Fenced(reason));
        }
    }

    #[test]
    fn authenticated_identity_refresh_revokes_ready_without_an_event() {
        let expected = incarnation("a");
        let changed = incarnation("b");
        let mut lifecycle = machine(1);
        lifecycle.accept_snapshot(&snapshot(&expected, 1), ResetScope::full_engine());
        lifecycle.caught_up(&caught_up(&expected, 0, 1));
        let frame =
            AuthenticatedIdentityFrame::from_authenticated_session(&changed, &KEY_ID, GENERATION);
        assert_eq!(
            lifecycle.observe_identity(&frame),
            IdentityAction::Fenced(SnapshotTailFenceReason::IncarnationChanged)
        );
    }

    #[test]
    fn disconnect_overflow_and_cancellation_are_terminal() {
        type FenceSignal = fn(&mut SnapshotTailFence);
        let cases: [(FenceSignal, SnapshotTailFenceReason); 4] = [
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
            (
                SnapshotTailFence::application_failed,
                SnapshotTailFenceReason::ApplicationFailed,
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
    fn delivery_sequence_overflow_fences() {
        let engine = incarnation("a");
        let mut lifecycle = machine(1);
        lifecycle.accept_snapshot(&snapshot(&engine, 1), ResetScope::full_engine());
        lifecycle.applied_delivery_sequence = Some(u64::MAX - 1);
        assert_eq!(
            lifecycle.accept_tail(&tail(&engine, u64::MAX, 2)),
            TailAction::Fenced(SnapshotTailFenceReason::SequenceOverflow)
        );
        assert_fenced(&lifecycle, SnapshotTailFenceReason::SequenceOverflow);
    }

    #[test]
    fn invalid_order_is_sticky_and_status_is_content_free() {
        let engine = incarnation("a");
        let mut lifecycle = machine(123);
        assert_eq!(
            lifecycle.accept_tail(&tail(&engine, 1, 124)),
            TailAction::Fenced(SnapshotTailFenceReason::UnexpectedState)
        );
        let revision = lifecycle.status().revision;
        lifecycle.cancel();
        assert_eq!(lifecycle.status().revision, revision);
        assert_eq!(
            lifecycle.status(),
            SnapshotTailStatus {
                revision,
                generation: GENERATION,
                state: SnapshotTailState::Fenced,
                reason: Some(SnapshotTailFenceReason::UnexpectedState),
            }
        );
    }
}
