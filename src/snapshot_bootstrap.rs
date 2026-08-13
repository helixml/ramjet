//! Transactional authenticated snapshot validation and private-index build.
//!
//! This is the only public bridge from a session-authenticated snapshot to a
//! tail lifecycle. It binds the authenticated envelope to the decoded reset
//! scope, watermark, incarnation, and digest key before returning any state an
//! actor could eventually publish.

use thiserror::Error;

use crate::{
    block_digest::BlockDigester,
    digest_index::{DigestIndexError, DigestIndexLimits, DigestKvIndex, SnapshotGroupKey},
    kv_snapshot::{
        DigestAlgorithm, DigestSpec, ResetScope, SnapshotError, SnapshotExpectations,
        SnapshotLimits, decode_snapshot_with_cancel,
    },
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
#[derive(Debug)]
pub struct PreparedSnapshotGeneration {
    index: DigestKvIndex,
    lifecycle: SnapshotTailFence,
}

impl PreparedSnapshotGeneration {
    #[must_use]
    pub const fn index(&self) -> &DigestKvIndex {
        &self.index
    }

    #[must_use]
    pub const fn lifecycle(&self) -> &SnapshotTailFence {
        &self.lifecycle
    }

    #[must_use]
    pub fn into_index_and_lifecycle(self) -> (DigestKvIndex, SnapshotTailFence) {
        (self.index, self.lifecycle)
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
        SnapshotAction::Accepted { .. } => Ok(PreparedSnapshotGeneration { index, lifecycle }),
        SnapshotAction::Fenced(reason) => Err(SnapshotBootstrapError::Lifecycle(reason)),
    }
}
