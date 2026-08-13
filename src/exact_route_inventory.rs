//! Read-only exact-route inventory abstraction.
//!
//! Direct vLLM consumers retain raw-token inventories. Snapshot companions
//! publish compact digest indexes. Routing needs only an authoritative,
//! revision-stable longest-prefix lookup, so it must not depend on either
//! storage representation.

use crate::{
    kv_consumer::SharedFencedInventory, snapshot_actor::SnapshotPublicationMarker,
    snapshot_consumer::SharedSnapshotPublication,
};

#[derive(Clone)]
pub enum ExactRouteInventory {
    Direct(SharedFencedInventory),
    Snapshot(SharedSnapshotPublication),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ExactInventoryMarker {
    Direct { generation: u64, revision: u64 },
    Snapshot(SnapshotPublicationMarker),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ExactInventoryLookup {
    pub marker: ExactInventoryMarker,
    pub overlap_tokens: usize,
    pub resident_tokens: usize,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct ExactInventoryStatus {
    pub trusted: bool,
    pub resident_blocks: usize,
    pub resident_tokens: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ExactInventoryLookupError {
    Untrusted,
    Lookup,
}

impl ExactRouteInventory {
    #[must_use]
    pub fn direct(inventory: SharedFencedInventory) -> Self {
        Self::Direct(inventory)
    }

    #[must_use]
    pub fn snapshot(publication: SharedSnapshotPublication) -> Self {
        Self::Snapshot(publication)
    }

    /// Whether this inventory representation is qualified to select serving
    /// placement. Compact companion snapshots are observation-only until their
    /// production comparison gate is complete.
    #[must_use]
    pub const fn supports_placement(&self) -> bool {
        matches!(self, Self::Direct(_))
    }

    #[must_use]
    pub fn ready(&self) -> bool {
        self.marker().is_some()
    }

    /// Return content-free health for whichever exact backend routing uses.
    #[must_use]
    pub(crate) fn status(&self) -> ExactInventoryStatus {
        match self {
            Self::Direct(inventory) => {
                let inventory = inventory.read();
                let stats = inventory.stats();
                ExactInventoryStatus {
                    trusted: inventory.trusted(),
                    resident_blocks: stats.external_hashes,
                    resident_tokens: stats.token_ids,
                }
            }
            Self::Snapshot(publication) => {
                let publication = publication.lock();
                let Some(index) = publication.published_index() else {
                    return ExactInventoryStatus::default();
                };
                let stats = index.stats();
                ExactInventoryStatus {
                    trusted: publication.published_marker().is_some(),
                    resident_blocks: stats.external_hashes,
                    resident_tokens: stats.logical_token_ids,
                }
            }
        }
    }

    pub(crate) fn marker(&self) -> Option<ExactInventoryMarker> {
        match self {
            Self::Direct(inventory) => {
                let inventory = inventory.read();
                inventory.trusted().then(|| ExactInventoryMarker::Direct {
                    generation: inventory.generation(),
                    revision: inventory.revision(),
                })
            }
            Self::Snapshot(publication) => publication
                .lock()
                .published_marker()
                .map(ExactInventoryMarker::Snapshot),
        }
    }

    pub(crate) fn lookup(
        &self,
        token_ids: &[u32],
    ) -> Result<ExactInventoryLookup, ExactInventoryLookupError> {
        match self {
            Self::Direct(inventory) => {
                let inventory = inventory.read();
                if !inventory.trusted() {
                    return Err(ExactInventoryLookupError::Untrusted);
                }
                let marker = ExactInventoryMarker::Direct {
                    generation: inventory.generation(),
                    revision: inventory.revision(),
                };
                let exact = inventory
                    .find_longest(token_ids)
                    .map_err(|_| ExactInventoryLookupError::Lookup)?
                    .ok_or(ExactInventoryLookupError::Untrusted)?;
                Ok(ExactInventoryLookup {
                    marker,
                    overlap_tokens: exact.token_ids,
                    resident_tokens: inventory.stats().token_ids,
                })
            }
            Self::Snapshot(publication) => {
                let publication = publication.lock();
                let marker = publication
                    .published_marker()
                    .ok_or(ExactInventoryLookupError::Untrusted)?;
                let index = publication
                    .published_index()
                    .ok_or(ExactInventoryLookupError::Untrusted)?;
                let exact = index
                    .find_longest(token_ids)
                    .map_err(|_| ExactInventoryLookupError::Lookup)?;
                Ok(ExactInventoryLookup {
                    marker: ExactInventoryMarker::Snapshot(marker),
                    overlap_tokens: exact.token_ids,
                    resident_tokens: index.stats().logical_token_ids,
                })
            }
        }
    }

    pub(crate) fn unchanged(&self, expected: ExactInventoryMarker) -> bool {
        self.marker() == Some(expected)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use parking_lot::{Mutex, RwLock};

    use super::*;
    use crate::{
        exact_index::{ExactIndexLimits, FencedExactKvInventory},
        snapshot_actor::{SnapshotActorLimits, SnapshotBootstrapActor},
    };

    #[test]
    fn untrusted_direct_and_unpublished_snapshot_fail_closed() {
        let direct = ExactRouteInventory::direct(Arc::new(RwLock::new(
            FencedExactKvInventory::new(8, ExactIndexLimits::default()),
        )));
        let snapshot = ExactRouteInventory::snapshot(Arc::new(Mutex::new(
            SnapshotBootstrapActor::new(SnapshotActorLimits::default()).unwrap(),
        )));

        for inventory in [direct, snapshot] {
            assert!(!inventory.ready());
            assert_eq!(inventory.marker(), None);
            assert!(!inventory.status().trusted);
            assert_eq!(
                inventory.lookup(&[1, 2, 3]),
                Err(ExactInventoryLookupError::Untrusted)
            );
        }
    }

    #[test]
    fn snapshot_capability_is_observation_only() {
        let direct = ExactRouteInventory::direct(Arc::new(RwLock::new(
            FencedExactKvInventory::new(8, ExactIndexLimits::default()),
        )));
        let snapshot = ExactRouteInventory::snapshot(Arc::new(Mutex::new(
            SnapshotBootstrapActor::new(SnapshotActorLimits::default()).unwrap(),
        )));

        assert!(direct.supports_placement());
        assert!(!snapshot.supports_placement());
    }
}
