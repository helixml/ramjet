//! Transport-independent owner for snapshot bootstrap and atomic publication.
//!
//! One actor owns the published index and every private replacement generation.
//! Authenticated tail frames are queued while a snapshot is built, then applied
//! only to actor-owned state. A same-identity replacement does not disturb the
//! current publication; an incarnation, digest-key, or companion-generation
//! change revokes it immediately. Session epochs make late disconnects and
//! frames from superseded sessions harmless.

use std::collections::{BTreeMap, VecDeque};
use std::fmt;

use thiserror::Error;

use crate::kv_snapshot::EngineIncarnation;
use crate::snapshot_bootstrap::PreparedSnapshotGeneration;
use crate::snapshot_session::SNAPSHOT_DIGEST_KEY_ID_BYTES;
use crate::snapshot_tail::{SnapshotTailFence, SnapshotTailFenceReason};
use crate::snapshot_tail_wire::{VerifiedTailAction, VerifiedTailFrame};

const MAX_SESSIONS: usize = 2;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct SessionEpoch(u64);

impl SessionEpoch {
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Independently authenticated identity of a companion generation.
#[derive(Clone, Eq, PartialEq)]
pub struct SnapshotActorIdentity {
    pub engine_incarnation: EngineIncarnation,
    pub digest_key_id: [u8; SNAPSHOT_DIGEST_KEY_ID_BYTES],
    pub companion_generation: u64,
}

impl fmt::Debug for SnapshotActorIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SnapshotActorIdentity")
            .field("engine_incarnation", &"[REDACTED]")
            .field("digest_key_id", &"[REDACTED]")
            .field("companion_generation", &self.companion_generation)
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SnapshotActorLimits {
    pub max_sessions: usize,
    pub max_queued_tail_frames: usize,
}

impl Default for SnapshotActorLimits {
    fn default() -> Self {
        Self {
            max_sessions: MAX_SESSIONS,
            max_queued_tail_frames: 1_024,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BootstrapSessionState {
    AwaitingSnapshot,
    BuildingSnapshot,
    CatchingUp,
    Published,
    Fenced,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BootstrapSessionStatus {
    pub epoch: SessionEpoch,
    pub state: BootstrapSessionState,
    pub queued_tail_frames: usize,
    pub fence_reason: Option<SnapshotTailFenceReason>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StartSessionResult {
    pub epoch: SessionEpoch,
    pub publication_revoked: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ActorAction {
    SnapshotBuildStarted,
    SnapshotInstalled,
    TailQueued,
    TailApplied,
    DuplicateIgnored,
    IdentityCurrent,
    IdentityChanged,
    Published { epoch: SessionEpoch },
    AlreadyPublished,
    SessionFenced(SnapshotTailFenceReason),
    StaleEpochIgnored,
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum SnapshotActorError {
    #[error("invalid snapshot actor limits")]
    InvalidLimits,
    #[error("snapshot actor session capacity reached")]
    SessionCapacity,
    #[error("snapshot actor session epoch exhausted")]
    EpochExhausted,
    #[error("snapshot actor session is in an invalid state")]
    InvalidState,
}

impl SnapshotActorError {
    #[must_use]
    pub const fn reason(self) -> &'static str {
        match self {
            Self::InvalidLimits => "invalid_limits",
            Self::SessionCapacity => "session_capacity",
            Self::EpochExhausted => "epoch_exhausted",
            Self::InvalidState => "invalid_state",
        }
    }
}

struct Published<I> {
    owner_epoch: SessionEpoch,
    identity: SnapshotActorIdentity,
    index: I,
}

struct BootstrapSession<I> {
    identity: SnapshotActorIdentity,
    minimum_snapshot_watermark: u64,
    fence: SnapshotTailFence,
    state: BootstrapSessionState,
    private_index: Option<I>,
    queued_tail_frames: VecDeque<VerifiedTailFrame>,
    fence_reason: Option<SnapshotTailFenceReason>,
}

/// Deterministic single-owner publication state machine.
///
/// The type contains no locks or tasks. A runtime actor can serialize commands
/// onto it, while focused tests can exercise every race ordering deterministically.
pub struct SnapshotBootstrapActor<I> {
    limits: SnapshotActorLimits,
    next_epoch: u64,
    sessions: BTreeMap<SessionEpoch, BootstrapSession<I>>,
    published: Option<Published<I>>,
}

impl<I> SnapshotBootstrapActor<I> {
    /// Construct a bounded actor.
    ///
    /// # Errors
    ///
    /// Rejects zero limits and session limits above the hard two-session bound.
    pub fn new(limits: SnapshotActorLimits) -> Result<Self, SnapshotActorError> {
        if limits.max_sessions == 0
            || limits.max_sessions > MAX_SESSIONS
            || limits.max_queued_tail_frames == 0
        {
            return Err(SnapshotActorError::InvalidLimits);
        }
        Ok(Self {
            limits,
            next_epoch: 1,
            sessions: BTreeMap::new(),
            published: None,
        })
    }

    #[must_use]
    pub fn published_index(&self) -> Option<&I> {
        self.published.as_ref().map(|published| &published.index)
    }

    #[must_use]
    pub fn published_epoch(&self) -> Option<SessionEpoch> {
        self.published
            .as_ref()
            .map(|published| published.owner_epoch)
    }

    #[must_use]
    pub fn session_count(&self) -> usize {
        self.sessions.len()
    }

    #[must_use]
    pub fn session_status(&self, epoch: SessionEpoch) -> Option<BootstrapSessionStatus> {
        self.sessions
            .get(&epoch)
            .map(|session| BootstrapSessionStatus {
                epoch,
                state: session.state,
                queued_tail_frames: session.queued_tail_frames.len(),
                fence_reason: session.fence_reason,
            })
    }

    /// Start one authenticated companion session.
    ///
    /// A differing identity revokes all old state before admitting the new
    /// session. A matching identity preserves the publication during catch-up.
    ///
    /// # Errors
    ///
    /// Returns an error when two sessions are already active or epochs are
    /// exhausted.
    pub fn start_session(
        &mut self,
        identity: SnapshotActorIdentity,
        minimum_snapshot_watermark: u64,
    ) -> Result<StartSessionResult, SnapshotActorError> {
        // Failed private generations do not consume one of the two bounded
        // slots forever. Their later transport notifications are stale epochs.
        self.sessions
            .retain(|_, session| session.state != BootstrapSessionState::Fenced);
        let identity_changed = self
            .current_identity()
            .is_some_and(|current| current != &identity);
        let publication_revoked = identity_changed && self.published.take().is_some();
        if identity_changed {
            self.sessions.clear();
        }
        if self.sessions.len() >= self.limits.max_sessions {
            return Err(SnapshotActorError::SessionCapacity);
        }
        let epoch = SessionEpoch(self.next_epoch);
        self.next_epoch = self
            .next_epoch
            .checked_add(1)
            .ok_or(SnapshotActorError::EpochExhausted)?;
        let fence = SnapshotTailFence::start_bootstrap(
            identity.engine_incarnation.clone(),
            minimum_snapshot_watermark,
            identity.digest_key_id.to_vec(),
            identity.companion_generation,
        );
        self.sessions.insert(
            epoch,
            BootstrapSession {
                identity,
                minimum_snapshot_watermark,
                fence,
                state: BootstrapSessionState::AwaitingSnapshot,
                private_index: None,
                queued_tail_frames: VecDeque::new(),
                fence_reason: None,
            },
        );
        Ok(StartSessionResult {
            epoch,
            publication_revoked,
        })
    }

    /// Mark the start of external bounded snapshot verification/build work.
    /// Installation still requires an opaque [`PreparedSnapshotGeneration`].
    pub fn begin_snapshot_build(&mut self, epoch: SessionEpoch) -> ActorAction {
        let action = {
            let Some(session) = self.sessions.get_mut(&epoch) else {
                return ActorAction::StaleEpochIgnored;
            };
            if session.state == BootstrapSessionState::AwaitingSnapshot {
                session.state = BootstrapSessionState::BuildingSnapshot;
                ActorAction::SnapshotBuildStarted
            } else {
                Self::fence_session_record(session, SnapshotTailFenceReason::UnexpectedState)
            }
        };
        if matches!(action, ActorAction::SessionFenced(_)) {
            self.revoke_if_owner(epoch);
        }
        action
    }

    /// Observe independently authenticated current identity. Any incarnation,
    /// digest-key, or generation change revokes every old generation at once.
    pub fn observe_identity(&mut self, identity: &SnapshotActorIdentity) -> ActorAction {
        if self
            .current_identity()
            .is_some_and(|current| current != identity)
        {
            self.published = None;
            self.sessions.clear();
            ActorAction::IdentityChanged
        } else {
            ActorAction::IdentityCurrent
        }
    }

    /// Install a privately built index and drain authenticated frames that
    /// arrived during the build.
    ///
    /// # Errors
    ///
    /// Returns [`SnapshotActorError::InvalidState`] unless the snapshot was
    /// previously admitted and is still unfenced.
    pub fn install_prepared_snapshot<E, F>(
        &mut self,
        epoch: SessionEpoch,
        prepared: PreparedSnapshotGeneration<I>,
        mut apply_payload: F,
    ) -> Result<ActorAction, SnapshotActorError>
    where
        F: FnMut(&mut I, &[u8]) -> Result<(), E>,
    {
        let Some(session) = self.sessions.get_mut(&epoch) else {
            return Ok(ActorAction::StaleEpochIgnored);
        };
        if session.state != BootstrapSessionState::BuildingSnapshot {
            return Err(SnapshotActorError::InvalidState);
        }
        if prepared.identity() != &session.identity
            || prepared.snapshot_watermark() < session.minimum_snapshot_watermark
        {
            return Ok(Self::fence_session_record(
                session,
                SnapshotTailFenceReason::UnexpectedState,
            ));
        }
        let (_, _, index, lifecycle) = prepared.into_actor_parts();
        session.fence = lifecycle;
        session.state = BootstrapSessionState::CatchingUp;
        let queued = std::mem::take(&mut session.queued_tail_frames);
        let Some(session) = self.sessions.get_mut(&epoch) else {
            return Ok(ActorAction::StaleEpochIgnored);
        };
        session.private_index = Some(index);

        let mut action = ActorAction::SnapshotInstalled;
        for frame in queued {
            action = self.apply_verified_frame(epoch, frame, &mut apply_payload);
            if matches!(action, ActorAction::SessionFenced(_)) {
                break;
            }
        }
        Ok(action)
    }

    /// Queue or apply one already-authenticated tail frame.
    pub fn accept_tail_frame<E, F>(
        &mut self,
        epoch: SessionEpoch,
        frame: VerifiedTailFrame,
        mut apply_payload: F,
    ) -> ActorAction
    where
        F: FnMut(&mut I, &[u8]) -> Result<(), E>,
    {
        let Some(state) = self.sessions.get(&epoch).map(|session| session.state) else {
            return ActorAction::StaleEpochIgnored;
        };
        if matches!(
            state,
            BootstrapSessionState::AwaitingSnapshot | BootstrapSessionState::BuildingSnapshot
        ) {
            let Some(session) = self.sessions.get_mut(&epoch) else {
                return ActorAction::StaleEpochIgnored;
            };
            if session.queued_tail_frames.len() >= self.limits.max_queued_tail_frames {
                session.fence.buffer_overflowed();
                return Self::fence_session_record(
                    session,
                    SnapshotTailFenceReason::BufferOverflow,
                );
            }
            session.queued_tail_frames.push_back(frame);
            return ActorAction::TailQueued;
        }
        if state == BootstrapSessionState::Fenced {
            let reason = self
                .sessions
                .get(&epoch)
                .and_then(|session| session.fence_reason)
                .unwrap_or(SnapshotTailFenceReason::UnexpectedState);
            return ActorAction::SessionFenced(reason);
        }
        self.apply_verified_frame(epoch, frame, &mut apply_payload)
    }

    /// Fence a failed private snapshot build without disturbing an older,
    /// same-identity publication.
    pub fn snapshot_build_failed(&mut self, epoch: SessionEpoch) -> ActorAction {
        let Some(session) = self.sessions.get_mut(&epoch) else {
            return ActorAction::StaleEpochIgnored;
        };
        session.fence.application_failed();
        let action =
            Self::fence_session_record(session, SnapshotTailFenceReason::ApplicationFailed);
        self.revoke_if_owner(epoch);
        action
    }

    /// Process transport disconnect by local epoch. A delayed disconnect from
    /// a superseded session cannot revoke a newer publication.
    pub fn disconnected(&mut self, epoch: SessionEpoch) -> ActorAction {
        let Some(mut session) = self.sessions.remove(&epoch) else {
            return ActorAction::StaleEpochIgnored;
        };
        session.fence.disconnected();
        self.revoke_if_owner(epoch);
        ActorAction::SessionFenced(SnapshotTailFenceReason::Disconnected)
    }

    fn apply_verified_frame<E, F>(
        &mut self,
        epoch: SessionEpoch,
        frame: VerifiedTailFrame,
        apply_payload: &mut F,
    ) -> ActorAction
    where
        F: FnMut(&mut I, &[u8]) -> Result<(), E>,
    {
        let verified = {
            let Some(session) = self.sessions.get_mut(&epoch) else {
                return ActorAction::StaleEpochIgnored;
            };
            frame.apply_to(&mut session.fence)
        };
        match verified {
            VerifiedTailAction::Apply { payload, .. } => {
                let applied = if self.published_epoch() == Some(epoch) {
                    self.published.as_mut().is_some_and(|published| {
                        apply_payload(&mut published.index, &payload).is_ok()
                    })
                } else {
                    self.sessions
                        .get_mut(&epoch)
                        .and_then(|session| session.private_index.as_mut())
                        .is_some_and(|index| apply_payload(index, &payload).is_ok())
                };
                if applied {
                    ActorAction::TailApplied
                } else {
                    if let Some(session) = self.sessions.get_mut(&epoch) {
                        session.fence.application_failed();
                        Self::fence_session_record(
                            session,
                            SnapshotTailFenceReason::ApplicationFailed,
                        );
                    }
                    self.revoke_if_owner(epoch);
                    ActorAction::SessionFenced(SnapshotTailFenceReason::ApplicationFailed)
                }
            }
            VerifiedTailAction::Duplicate => ActorAction::DuplicateIgnored,
            VerifiedTailAction::Ready => self.publish(epoch),
            VerifiedTailAction::AlreadyReady => ActorAction::AlreadyPublished,
            VerifiedTailAction::IdentityCurrent => ActorAction::IdentityCurrent,
            VerifiedTailAction::Fenced(reason) => {
                if let Some(session) = self.sessions.get_mut(&epoch) {
                    Self::fence_session_record(session, reason);
                }
                self.revoke_if_owner(epoch);
                ActorAction::SessionFenced(reason)
            }
        }
    }

    fn publish(&mut self, epoch: SessionEpoch) -> ActorAction {
        let Some(mut session) = self.sessions.remove(&epoch) else {
            return ActorAction::StaleEpochIgnored;
        };
        let Some(index) = session.private_index.take() else {
            session.fence.application_failed();
            session.state = BootstrapSessionState::Fenced;
            session.fence_reason = Some(SnapshotTailFenceReason::ApplicationFailed);
            self.sessions.insert(epoch, session);
            return ActorAction::SessionFenced(SnapshotTailFenceReason::ApplicationFailed);
        };
        session.state = BootstrapSessionState::Published;
        self.published = Some(Published {
            owner_epoch: epoch,
            identity: session.identity.clone(),
            index,
        });
        // The new epoch owns publication. Forget all older sessions before
        // reinserting it so their late disconnects are unambiguously stale.
        self.sessions.clear();
        self.sessions.insert(epoch, session);
        ActorAction::Published { epoch }
    }

    fn revoke_if_owner(&mut self, epoch: SessionEpoch) {
        if self.published_epoch() == Some(epoch) {
            self.published = None;
        }
    }

    fn current_identity(&self) -> Option<&SnapshotActorIdentity> {
        self.published
            .as_ref()
            .map(|published| &published.identity)
            .or_else(|| {
                self.sessions
                    .values()
                    .next()
                    .map(|session| &session.identity)
            })
    }

    fn fence_session_record(
        session: &mut BootstrapSession<I>,
        reason: SnapshotTailFenceReason,
    ) -> ActorAction {
        session.state = BootstrapSessionState::Fenced;
        session.fence_reason = Some(reason);
        session.queued_tail_frames.clear();
        session.private_index = None;
        ActorAction::SessionFenced(reason)
    }
}

#[cfg(test)]
mod tests {
    use std::convert::Infallible;

    use super::*;
    use crate::kv_snapshot::ResetScope;
    use crate::snapshot_session::{
        AuthenticatedSnapshot, SnapshotSessionBinding, SnapshotSessionChallenge,
        SnapshotSessionExpectations, SnapshotSessionLimits, SnapshotSessionSecret,
        decode_authenticated_snapshot, encode_authenticated_snapshot,
    };
    use crate::snapshot_tail::SnapshotAction;
    use crate::snapshot_tail_wire::{
        TailDirection, TailFrameBinding, TailFrameDecoder, TailFrameType, TailSessionExpectations,
        TailSessionKey, TailWireLimits, encode_tail_frame,
    };

    const SECRET_BYTES: [u8; 32] = *b"snapshot-session-secret-32-byte!";
    const CHALLENGE: SnapshotSessionChallenge = SnapshotSessionChallenge::new([0x41; 32]);
    const KEY_A: [u8; SNAPSHOT_DIGEST_KEY_ID_BYTES] = [0x51; 32];
    const KEY_B: [u8; SNAPSHOT_DIGEST_KEY_ID_BYTES] = [0x52; 32];

    fn incarnation(name: &str) -> EngineIncarnation {
        EngineIncarnation {
            engine_id: name.into(),
            model_revision: "revision".into(),
            image_digest: "sha256:image".into(),
            process_started_unix_ns: 42,
            attestation_sha256: vec![3; 32],
        }
    }

    fn identity(name: &str, key: [u8; 32], generation: u64) -> SnapshotActorIdentity {
        SnapshotActorIdentity {
            engine_incarnation: incarnation(name),
            digest_key_id: key,
            companion_generation: generation,
        }
    }

    fn snapshot(identity: &SnapshotActorIdentity, watermark: u64) -> AuthenticatedSnapshot {
        let secret = SnapshotSessionSecret::new(SECRET_BYTES);
        let frame = encode_authenticated_snapshot(
            b"opaque-snapshot",
            SnapshotSessionBinding {
                challenge: CHALLENGE,
                engine_incarnation: &identity.engine_incarnation,
                snapshot_watermark: watermark,
                digest_key_id: &identity.digest_key_id,
                companion_generation: identity.companion_generation,
            },
            &secret,
            SnapshotSessionLimits::default(),
        )
        .unwrap();
        decode_authenticated_snapshot(
            &frame,
            SnapshotSessionExpectations {
                challenge: CHALLENGE,
                engine_incarnation: &identity.engine_incarnation,
                digest_key_id: &identity.digest_key_id,
                minimum_snapshot_watermark: watermark,
                minimum_companion_generation: identity.companion_generation,
            },
            &secret,
            SnapshotSessionLimits::default(),
        )
        .unwrap()
    }

    fn verified(
        identity: &SnapshotActorIdentity,
        frame_type: TailFrameType,
        message_sequence: u64,
        delivery_sequence: u64,
        event_watermark: u64,
        payload: &[u8],
    ) -> VerifiedTailFrame {
        let secret = SnapshotSessionSecret::new(SECRET_BYTES);
        let key = TailSessionKey::derive(
            &secret,
            CHALLENGE,
            identity.companion_generation,
            TailDirection::CompanionToRouter,
        );
        let frame = encode_tail_frame(
            payload,
            TailFrameBinding {
                frame_type,
                direction: TailDirection::CompanionToRouter,
                session_id: CHALLENGE,
                message_sequence,
                delivery_sequence,
                event_watermark,
                engine_incarnation: &identity.engine_incarnation,
                digest_key_id: &identity.digest_key_id,
                companion_generation: identity.companion_generation,
            },
            &key,
            TailWireLimits::default(),
        )
        .unwrap();
        TailFrameDecoder::new(
            &key,
            TailSessionExpectations {
                direction: TailDirection::CompanionToRouter,
                session_id: CHALLENGE,
                first_message_sequence: message_sequence,
                engine_incarnation: &identity.engine_incarnation,
                digest_key_id: &identity.digest_key_id,
                companion_generation: identity.companion_generation,
            },
            TailWireLimits::default(),
        )
        .unwrap()
        .decode(&frame)
        .unwrap()
    }

    #[allow(clippy::unnecessary_wraps)]
    fn no_apply(_index: &mut Vec<u8>, _payload: &[u8]) -> Result<(), Infallible> {
        Ok(())
    }

    fn begin_build(
        actor: &mut SnapshotBootstrapActor<Vec<u8>>,
        identity: &SnapshotActorIdentity,
        watermark: u64,
    ) -> SessionEpoch {
        let epoch = actor
            .start_session(identity.clone(), watermark)
            .unwrap()
            .epoch;
        assert_eq!(
            actor.begin_snapshot_build(epoch),
            ActorAction::SnapshotBuildStarted
        );
        epoch
    }

    fn prepared(
        identity: &SnapshotActorIdentity,
        watermark: u64,
        index: Vec<u8>,
    ) -> PreparedSnapshotGeneration<Vec<u8>> {
        let authenticated = snapshot(identity, watermark);
        let mut lifecycle = SnapshotTailFence::start_bootstrap(
            identity.engine_incarnation.clone(),
            watermark,
            identity.digest_key_id.to_vec(),
            identity.companion_generation,
        );
        assert!(matches!(
            lifecycle.accept_snapshot(&authenticated, ResetScope::full_engine()),
            SnapshotAction::Accepted { .. }
        ));
        PreparedSnapshotGeneration::from_test_parts(identity.clone(), watermark, index, lifecycle)
    }

    fn finish_publish(
        actor: &mut SnapshotBootstrapActor<Vec<u8>>,
        identity: &SnapshotActorIdentity,
        epoch: SessionEpoch,
        watermark: u64,
        index: Vec<u8>,
    ) {
        assert_eq!(
            actor.install_prepared_snapshot(epoch, prepared(identity, watermark, index), no_apply),
            Ok(ActorAction::SnapshotInstalled)
        );
        assert_eq!(
            actor.accept_tail_frame(
                epoch,
                verified(identity, TailFrameType::CaughtUp, 1, 0, watermark, b""),
                no_apply,
            ),
            ActorAction::Published { epoch }
        );
    }

    #[test]
    fn same_identity_replacement_is_private_until_atomic_publish() {
        let identity = identity("engine-a", KEY_A, 7);
        let mut actor = SnapshotBootstrapActor::new(SnapshotActorLimits::default()).unwrap();
        let old = begin_build(&mut actor, &identity, 100);
        finish_publish(&mut actor, &identity, old, 100, vec![1]);

        let new = begin_build(&mut actor, &identity, 200);
        assert_eq!(actor.published_index(), Some(&vec![1]));
        assert_eq!(
            actor.install_prepared_snapshot(new, prepared(&identity, 200, vec![2]), no_apply),
            Ok(ActorAction::SnapshotInstalled)
        );
        assert_eq!(actor.published_index(), Some(&vec![1]));
        assert_eq!(
            actor.accept_tail_frame(
                new,
                verified(&identity, TailFrameType::CaughtUp, 1, 0, 200, b""),
                no_apply,
            ),
            ActorAction::Published { epoch: new }
        );
        assert_eq!(actor.published_index(), Some(&vec![2]));
        assert_eq!(actor.session_count(), 1);
    }

    #[test]
    fn old_disconnect_cannot_revoke_newer_publication() {
        let identity = identity("engine-a", KEY_A, 7);
        let mut actor = SnapshotBootstrapActor::new(SnapshotActorLimits::default()).unwrap();
        let old = begin_build(&mut actor, &identity, 100);
        finish_publish(&mut actor, &identity, old, 100, vec![1]);
        let new = begin_build(&mut actor, &identity, 200);
        finish_publish(&mut actor, &identity, new, 200, vec![2]);

        assert_eq!(actor.disconnected(old), ActorAction::StaleEpochIgnored);
        assert_eq!(actor.published_epoch(), Some(new));
        assert_eq!(actor.published_index(), Some(&vec![2]));
    }

    #[test]
    fn owner_disconnect_revokes_until_replacement_catches_up() {
        let identity = identity("engine-a", KEY_A, 7);
        let mut actor = SnapshotBootstrapActor::new(SnapshotActorLimits::default()).unwrap();
        let old = begin_build(&mut actor, &identity, 100);
        finish_publish(&mut actor, &identity, old, 100, vec![1]);
        let new = begin_build(&mut actor, &identity, 200);

        assert_eq!(
            actor.disconnected(old),
            ActorAction::SessionFenced(SnapshotTailFenceReason::Disconnected)
        );
        assert_eq!(actor.published_index(), None);
        finish_publish(&mut actor, &identity, new, 200, vec![2]);
        assert_eq!(actor.published_index(), Some(&vec![2]));
    }

    #[test]
    fn identity_key_and_generation_changes_revoke_immediately() {
        for changed in [
            identity("engine-b", KEY_A, 7),
            identity("engine-a", KEY_B, 7),
            identity("engine-a", KEY_A, 8),
        ] {
            let original = identity("engine-a", KEY_A, 7);
            let mut actor = SnapshotBootstrapActor::new(SnapshotActorLimits::default()).unwrap();
            let old = begin_build(&mut actor, &original, 100);
            finish_publish(&mut actor, &original, old, 100, vec![1]);

            let started = actor.start_session(changed, 200).unwrap();
            assert!(started.publication_revoked);
            assert_eq!(actor.published_index(), None);
            assert_eq!(actor.session_count(), 1);
            assert_eq!(actor.disconnected(old), ActorAction::StaleEpochIgnored);
        }
    }

    #[test]
    fn authenticated_identity_observation_revokes_without_new_session() {
        let original = identity("engine-a", KEY_A, 7);
        let changed = identity("engine-a", KEY_B, 7);
        let mut actor = SnapshotBootstrapActor::new(SnapshotActorLimits::default()).unwrap();
        let epoch = begin_build(&mut actor, &original, 100);
        finish_publish(&mut actor, &original, epoch, 100, vec![1]);

        assert_eq!(
            actor.observe_identity(&original),
            ActorAction::IdentityCurrent
        );
        assert_eq!(
            actor.observe_identity(&changed),
            ActorAction::IdentityChanged
        );
        assert_eq!(actor.published_index(), None);
        assert_eq!(actor.session_count(), 0);
        assert_eq!(actor.disconnected(epoch), ActorAction::StaleEpochIgnored);
    }

    #[test]
    fn queued_tail_is_applied_before_caught_up_publication() {
        let identity = identity("engine-a", KEY_A, 7);
        let mut actor = SnapshotBootstrapActor::new(SnapshotActorLimits::default()).unwrap();
        let epoch = begin_build(&mut actor, &identity, 100);
        assert_eq!(
            actor.accept_tail_frame(
                epoch,
                verified(&identity, TailFrameType::Event, 1, 1, 101, &[9]),
                no_apply,
            ),
            ActorAction::TailQueued
        );
        assert_eq!(
            actor.accept_tail_frame(
                epoch,
                verified(&identity, TailFrameType::CaughtUp, 2, 1, 101, b""),
                no_apply,
            ),
            ActorAction::TailQueued
        );
        let action = actor
            .install_prepared_snapshot(
                epoch,
                prepared(&identity, 100, vec![1]),
                |index, payload| {
                    index.extend_from_slice(payload);
                    Ok::<_, Infallible>(())
                },
            )
            .unwrap();
        assert_eq!(action, ActorAction::Published { epoch });
        assert_eq!(actor.published_index(), Some(&vec![1, 9]));
    }

    #[test]
    fn queue_overflow_fences_replacement_and_preserves_old_publish() {
        let identity = identity("engine-a", KEY_A, 7);
        let limits = SnapshotActorLimits {
            max_sessions: 2,
            max_queued_tail_frames: 1,
        };
        let mut actor = SnapshotBootstrapActor::new(limits).unwrap();
        let old = begin_build(&mut actor, &identity, 100);
        finish_publish(&mut actor, &identity, old, 100, vec![1]);
        let new = begin_build(&mut actor, &identity, 200);
        assert_eq!(
            actor.accept_tail_frame(
                new,
                verified(&identity, TailFrameType::Event, 1, 1, 201, &[2]),
                no_apply,
            ),
            ActorAction::TailQueued
        );
        assert_eq!(
            actor.accept_tail_frame(
                new,
                verified(&identity, TailFrameType::Event, 2, 2, 202, &[3]),
                no_apply,
            ),
            ActorAction::SessionFenced(SnapshotTailFenceReason::BufferOverflow)
        );
        assert_eq!(actor.published_epoch(), Some(old));
        assert_eq!(actor.published_index(), Some(&vec![1]));
        assert_eq!(actor.session_status(new).unwrap().queued_tail_frames, 0);
        assert!(actor.start_session(identity, 300).is_ok());
    }

    #[test]
    fn failed_private_apply_never_leaks_partial_state() {
        let identity = identity("engine-a", KEY_A, 7);
        let mut actor = SnapshotBootstrapActor::new(SnapshotActorLimits::default()).unwrap();
        let old = begin_build(&mut actor, &identity, 100);
        finish_publish(&mut actor, &identity, old, 100, vec![1]);
        let new = begin_build(&mut actor, &identity, 200);
        actor.accept_tail_frame(
            new,
            verified(&identity, TailFrameType::Event, 1, 1, 201, &[9]),
            no_apply,
        );
        let action = actor
            .install_prepared_snapshot(new, prepared(&identity, 200, vec![2]), |index, payload| {
                index.extend_from_slice(payload);
                Err::<(), _>(())
            })
            .unwrap();
        assert_eq!(
            action,
            ActorAction::SessionFenced(SnapshotTailFenceReason::ApplicationFailed)
        );
        assert_eq!(actor.published_epoch(), Some(old));
        assert_eq!(actor.published_index(), Some(&vec![1]));
    }

    #[test]
    fn published_owner_applies_live_tail_privately_inside_actor() {
        let identity = identity("engine-a", KEY_A, 7);
        let mut actor = SnapshotBootstrapActor::new(SnapshotActorLimits::default()).unwrap();
        let epoch = begin_build(&mut actor, &identity, 100);
        finish_publish(&mut actor, &identity, epoch, 100, vec![1]);
        assert_eq!(
            actor.accept_tail_frame(
                epoch,
                verified(&identity, TailFrameType::Event, 2, 1, 101, &[9]),
                |index, payload| {
                    index.extend_from_slice(payload);
                    Ok::<_, Infallible>(())
                },
            ),
            ActorAction::TailApplied
        );
        assert_eq!(actor.published_index(), Some(&vec![1, 9]));
    }

    #[test]
    fn two_session_cap_rejects_third_without_disturbing_state() {
        let identity = identity("engine-a", KEY_A, 7);
        let mut actor = SnapshotBootstrapActor::new(SnapshotActorLimits::default()).unwrap();
        let old = begin_build(&mut actor, &identity, 100);
        finish_publish(&mut actor, &identity, old, 100, vec![1]);
        let replacement = begin_build(&mut actor, &identity, 200);
        assert_eq!(
            actor.start_session(identity.clone(), 300),
            Err(SnapshotActorError::SessionCapacity)
        );
        assert_eq!(actor.session_count(), 2);
        assert_eq!(actor.published_epoch(), Some(old));

        finish_publish(&mut actor, &identity, replacement, 200, vec![2]);
        assert_eq!(actor.session_count(), 1);
        assert!(actor.start_session(identity, 300).is_ok());
    }

    #[test]
    fn replacement_build_failure_preserves_old_publication() {
        let identity = identity("engine-a", KEY_A, 7);
        let mut actor = SnapshotBootstrapActor::new(SnapshotActorLimits::default()).unwrap();
        let old = begin_build(&mut actor, &identity, 100);
        finish_publish(&mut actor, &identity, old, 100, vec![1]);
        let replacement = begin_build(&mut actor, &identity, 200);
        assert_eq!(
            actor.snapshot_build_failed(replacement),
            ActorAction::SessionFenced(SnapshotTailFenceReason::ApplicationFailed)
        );
        assert_eq!(actor.published_epoch(), Some(old));
        assert_eq!(actor.published_index(), Some(&vec![1]));
    }

    #[test]
    fn prepared_identity_mismatch_fences_only_replacement() {
        let original = identity("engine-a", KEY_A, 7);
        let changed = identity("engine-b", KEY_A, 7);
        let mut actor = SnapshotBootstrapActor::new(SnapshotActorLimits::default()).unwrap();
        let old = begin_build(&mut actor, &original, 100);
        finish_publish(&mut actor, &original, old, 100, vec![1]);
        let replacement = begin_build(&mut actor, &original, 200);

        assert_eq!(
            actor.install_prepared_snapshot(
                replacement,
                prepared(&changed, 200, vec![2]),
                no_apply,
            ),
            Ok(ActorAction::SessionFenced(
                SnapshotTailFenceReason::UnexpectedState
            ))
        );
        assert_eq!(actor.published_epoch(), Some(old));
        assert_eq!(actor.published_index(), Some(&vec![1]));
    }

    #[test]
    fn invalid_owner_transition_revokes_its_publication() {
        let identity = identity("engine-a", KEY_A, 7);
        let mut actor = SnapshotBootstrapActor::new(SnapshotActorLimits::default()).unwrap();
        let epoch = begin_build(&mut actor, &identity, 100);
        finish_publish(&mut actor, &identity, epoch, 100, vec![1]);

        assert_eq!(
            actor.begin_snapshot_build(epoch),
            ActorAction::SessionFenced(SnapshotTailFenceReason::UnexpectedState)
        );
        assert_eq!(actor.published_index(), None);
    }
}
