//! Transactional authenticated snapshot validation and private-index build.
//!
//! This is the only public bridge from a session-authenticated snapshot to a
//! tail lifecycle. It binds the authenticated envelope to the decoded reset
//! scope, watermark, incarnation, and digest key before returning any state an
//! actor could eventually publish.

use std::fmt;

use thiserror::Error;

use crate::{
    block_digest::BlockDigester,
    digest_index::{DigestIndexError, DigestIndexLimits, DigestKvIndex, SnapshotGroupKey},
    kv_snapshot::{
        DigestAlgorithm, DigestSpec, ResetScope, SnapshotError, SnapshotExpectations,
        SnapshotLimits, decode_snapshot_with_cancel,
    },
    snapshot_actor::SnapshotActorIdentity,
    snapshot_session::AuthenticatedSnapshot,
    snapshot_tail::{SnapshotAction, SnapshotTailFence, SnapshotTailFenceReason},
};

const DIGEST_BYTES: u16 = 32;

#[derive(Debug, Error, Eq, PartialEq)]
pub enum SnapshotBootstrapError {
    #[error("authenticated snapshot binding does not match its payload")]
    BindingMismatch,
    #[error("KV snapshot validation failed")]
    Snapshot(#[source] SnapshotError),
    #[error("private digest-index construction failed")]
    DigestIndex(#[source] DigestIndexError),
    #[error("snapshot lifecycle rejected the authenticated generation")]
    Lifecycle(SnapshotTailFenceReason),
}

impl SnapshotBootstrapError {
    #[must_use]
    pub const fn reason(&self) -> &'static str {
        match self {
            Self::BindingMismatch => "binding_mismatch",
            Self::Snapshot(error) => error.reason(),
            Self::DigestIndex(error) => error.reason(),
            Self::Lifecycle(reason) => reason.as_str(),
        }
    }
}

/// A fully validated private generation. It cannot be created from a decoded
/// snapshot body alone and is not published merely by being constructed.
pub struct PreparedSnapshotGeneration<I = DigestKvIndex> {
    identity: SnapshotActorIdentity,
    snapshot_watermark: u64,
    index: I,
    lifecycle: SnapshotTailFence,
}

impl<I> fmt::Debug for PreparedSnapshotGeneration<I> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PreparedSnapshotGeneration")
            .field("identity", &self.identity)
            .field("snapshot_watermark", &self.snapshot_watermark)
            .field("index", &"[REDACTED]")
            .field("lifecycle", &self.lifecycle.status())
            .finish()
    }
}

impl<I> PreparedSnapshotGeneration<I> {
    #[must_use]
    pub const fn identity(&self) -> &SnapshotActorIdentity {
        &self.identity
    }

    #[must_use]
    pub const fn snapshot_watermark(&self) -> u64 {
        self.snapshot_watermark
    }

    #[must_use]
    pub const fn index(&self) -> &I {
        &self.index
    }

    #[must_use]
    pub const fn lifecycle(&self) -> &SnapshotTailFence {
        &self.lifecycle
    }

    pub(crate) fn into_actor_parts(self) -> (SnapshotActorIdentity, u64, I, SnapshotTailFence) {
        (
            self.identity,
            self.snapshot_watermark,
            self.index,
            self.lifecycle,
        )
    }

    #[cfg(test)]
    pub(crate) fn from_test_parts(
        identity: SnapshotActorIdentity,
        snapshot_watermark: u64,
        index: I,
        lifecycle: SnapshotTailFence,
    ) -> Self {
        Self {
            identity,
            snapshot_watermark,
            index,
            lifecycle,
        }
    }
}

/// Validate an authenticated snapshot and build a private digest index.
///
/// The returned lifecycle is only `CatchingUp`; callers must authenticate and
/// apply the tail, observe an exact caught-up marker, and atomically publish.
///
/// # Errors
///
/// Fails without returning partial state on an outer/inner binding mismatch,
/// snapshot validation error, digest-index error, cancellation, or lifecycle
/// rejection.
pub fn prepare_authenticated_snapshot(
    authenticated: AuthenticatedSnapshot,
    digest_secret: &[u8; 32],
    snapshot_limits: SnapshotLimits,
    index_limits: DigestIndexLimits,
    group: SnapshotGroupKey,
    minimum_snapshot_watermark: u64,
) -> Result<PreparedSnapshotGeneration, SnapshotBootstrapError> {
    prepare_authenticated_snapshot_with_cancel(
        authenticated,
        digest_secret,
        snapshot_limits,
        index_limits,
        group,
        minimum_snapshot_watermark,
        || false,
    )
}

/// Cancellation-aware variant of [`prepare_authenticated_snapshot`].
///
/// # Errors
///
/// Returns the same content-free validation, index, and lifecycle errors as
/// [`prepare_authenticated_snapshot`], including cancellation errors from
/// bounded snapshot decode or private-index construction.
pub fn prepare_authenticated_snapshot_with_cancel(
    authenticated: AuthenticatedSnapshot,
    digest_secret: &[u8; 32],
    snapshot_limits: SnapshotLimits,
    index_limits: DigestIndexLimits,
    group: SnapshotGroupKey,
    minimum_snapshot_watermark: u64,
    mut cancelled: impl FnMut() -> bool,
) -> Result<PreparedSnapshotGeneration, SnapshotBootstrapError> {
    let identity = SnapshotActorIdentity {
        engine_incarnation: authenticated.engine_incarnation().clone(),
        digest_key_id: *authenticated.digest_key_id(),
        companion_generation: authenticated.companion_generation(),
    };
    let digester = BlockDigester::new(*digest_secret);
    if !digester
        .key_id()
        .matches_wire(authenticated.digest_key_id())
    {
        return Err(SnapshotBootstrapError::BindingMismatch);
    }
    let expected_digest = DigestSpec {
        algorithm: DigestAlgorithm::HmacSha256V1,
        key_id: authenticated.digest_key_id().to_vec(),
        digest_bytes: DIGEST_BYTES,
    };
    let body = decode_snapshot_with_cancel(
        authenticated.snapshot_frame(),
        snapshot_limits,
        SnapshotExpectations {
            engine_incarnation: authenticated.engine_incarnation(),
            reset_scope: ResetScope::full_engine(),
            digest: &expected_digest,
        },
        &mut cancelled,
    )
    .map_err(SnapshotBootstrapError::Snapshot)?;
    if body.watermark != authenticated.snapshot_watermark() {
        return Err(SnapshotBootstrapError::BindingMismatch);
    }

    let mut index = DigestKvIndex::from_secret(index_limits, digest_secret)
        .map_err(SnapshotBootstrapError::DigestIndex)?;
    index
        .replace_from_snapshot_with_cancel(&body, group, &mut cancelled)
        .map_err(SnapshotBootstrapError::DigestIndex)?;

    // The exact authenticated generation is authoritative. The caller's
    // generation floor was checked by the session decoder before this point.
    let mut lifecycle = SnapshotTailFence::start_bootstrap(
        body.engine_incarnation.clone(),
        minimum_snapshot_watermark,
        authenticated.digest_key_id().to_vec(),
        authenticated.companion_generation(),
    );
    let action = lifecycle.accept_snapshot(&authenticated, body.reset_scope);
    drop(authenticated);
    match action {
        SnapshotAction::Accepted { .. } => Ok(PreparedSnapshotGeneration {
            identity,
            snapshot_watermark: body.watermark,
            index,
            lifecycle,
        }),
        SnapshotAction::Fenced(reason) => Err(SnapshotBootstrapError::Lifecycle(reason)),
    }
}
