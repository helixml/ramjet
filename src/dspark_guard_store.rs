//! Durable, content-free quarantine state for the `DSpark` reliability guard.
//!
//! The store owns no routing or detector policy. It requires a pre-created
//! protected file, holds an exclusive lock on its parent directory for its
//! lifetime, and publishes canonical state by same-directory fsynced rename.

use std::{
    collections::{BTreeMap, HashSet},
    fmt,
    fs::{self, File, Metadata, OpenOptions},
    io::Write,
    os::unix::{
        ffi::OsStrExt,
        fs::{MetadataExt, OpenOptionsExt},
    },
    path::{Component, Path, PathBuf},
};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::snapshot_secret_file::{SnapshotSecretFilePolicy, load_snapshot_control_file};

const SCHEMA_VERSION: u8 = 1;
const MAX_STATE_BYTES: usize = 4 * 1024;
const MAX_PATH_BYTES: usize = 4_096;
const MAX_REPLICAS: usize = 64;
const FILE_MODE: u32 = 0o600;
const PARENT_MODE: u32 = 0o700;
const GROUP_OR_WORLD_WRITE: u32 = 0o022;
const STICKY_BIT: u32 = 0o1000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DsparkGuardStorePolicy {
    pub owner_uid: u32,
    pub group_gid: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StoreMutation {
    Added,
    Unchanged,
    Removed,
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum DsparkGuardStoreError {
    #[error("DSpark guard state path is invalid")]
    InvalidPath,
    #[error("DSpark guard state parent is unsafe")]
    UnsafeParent,
    #[error("DSpark guard state directory is already owned")]
    DirectoryLocked,
    #[error("DSpark guard state file is unsafe")]
    UnsafeFile,
    #[error("DSpark guard state file could not be read")]
    ReadFailed,
    #[error("DSpark guard state document is malformed")]
    Malformed,
    #[error("DSpark guard state document is not canonical")]
    NonCanonical,
    #[error("DSpark guard state schema is unsupported")]
    UnsupportedSchema,
    #[error("DSpark guard state exceeds capacity")]
    Capacity,
    #[error("DSpark guard state contains a duplicate")]
    Duplicate,
    #[error("DSpark guard state replica is invalid")]
    InvalidReplica,
    #[error("DSpark guard state upstream identity changed")]
    UpstreamMismatch,
    #[error("DSpark guard state record conflicts")]
    RecordConflict,
    #[error("DSpark guard state requires a changed EngineCore incarnation")]
    SameEngineCore,
    #[error("DSpark guard state record is missing")]
    MissingRecord,
    #[error("DSpark guard state could not be encoded")]
    EncodeFailed,
    #[error("DSpark guard state could not be published")]
    PublishFailed,
    #[error("DSpark guard state publication outcome is uncertain")]
    CommitUncertain,
}

impl DsparkGuardStoreError {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::InvalidPath => "invalid_path",
            Self::UnsafeParent => "unsafe_parent",
            Self::DirectoryLocked => "directory_locked",
            Self::UnsafeFile => "unsafe_file",
            Self::ReadFailed => "read_failed",
            Self::Malformed => "malformed",
            Self::NonCanonical => "non_canonical",
            Self::UnsupportedSchema => "unsupported_schema",
            Self::Capacity => "capacity",
            Self::Duplicate => "duplicate",
            Self::InvalidReplica => "invalid_replica",
            Self::UpstreamMismatch => "upstream_mismatch",
            Self::RecordConflict => "record_conflict",
            Self::SameEngineCore => "same_engine_core",
            Self::MissingRecord => "missing_record",
            Self::EncodeFailed => "encode_failed",
            Self::PublishFailed => "publish_failed",
            Self::CommitUncertain => "commit_uncertain",
        }
    }
}

/// A lifetime-exclusive durable quarantine set.
///
/// Debug output deliberately excludes its path, ownership, upstream
/// commitments, and `EngineCore` commitments.
pub struct DsparkGuardStore {
    path: PathBuf,
    parent: PathBuf,
    parent_lock: File,
    parent_identity: FileIdentity,
    policy: DsparkGuardStorePolicy,
    upstreams: Box<[[u8; 32]]>,
    state: Mutex<StoreState>,
}

impl fmt::Debug for DsparkGuardStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DsparkGuardStore")
            .field("path", &"<redacted>")
            .field("replicas", &self.upstreams.len())
            .finish_non_exhaustive()
    }
}

struct StoreState {
    records: BTreeMap<usize, QuarantineRecord>,
    target_identity: FileIdentity,
    runtime_dirty: bool,
    uncertain_replicas: HashSet<usize>,
    poisoned_replicas: HashSet<usize>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct QuarantineRecord {
    upstream_sha256: [u8; 32],
    engine_core_sha256: [u8; 32],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FileIdentity {
    device: u64,
    inode: u64,
}

impl FileIdentity {
    fn from_metadata(metadata: &Metadata) -> Self {
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
        }
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct WireDocument {
    schema_version: u8,
    runtime_dirty: bool,
    quarantines: Vec<WireRecord>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct WireRecord {
    replica: usize,
    upstream_sha256: String,
    engine_core_sha256: String,
}

impl DsparkGuardStore {
    /// Open and exclusively own one pre-created protected state file.
    ///
    /// `ordered_upstream_sha256` is the caller's privacy-safe commitment to
    /// each configured upstream in routing order. Every persisted record must
    /// match that exact ordinal and commitment.
    ///
    /// # Errors
    ///
    /// Fails closed for an invalid/unsafe path, concurrent owner, unsafe file,
    /// malformed/noncanonical state, unsupported schema, capacity violation,
    /// duplicate record, or upstream mapping change.
    pub fn open(
        path: &Path,
        policy: DsparkGuardStorePolicy,
        ordered_upstream_sha256: &[[u8; 32]],
    ) -> Result<Self, DsparkGuardStoreError> {
        if ordered_upstream_sha256.is_empty() || ordered_upstream_sha256.len() > MAX_REPLICAS {
            return Err(DsparkGuardStoreError::Capacity);
        }
        validate_normalized_absolute_path(path)?;
        let parent = path
            .parent()
            .ok_or(DsparkGuardStoreError::InvalidPath)?
            .to_path_buf();
        let parent_metadata = validate_parents(&parent, policy)?;
        let parent_lock = File::open(&parent).map_err(|_| DsparkGuardStoreError::UnsafeParent)?;
        parent_lock
            .try_lock()
            .map_err(|_| DsparkGuardStoreError::DirectoryLocked)?;
        let locked_metadata = parent_lock
            .metadata()
            .map_err(|_| DsparkGuardStoreError::UnsafeParent)?;
        let parent_identity = FileIdentity::from_metadata(&parent_metadata);
        if FileIdentity::from_metadata(&locked_metadata) != parent_identity {
            return Err(DsparkGuardStoreError::UnsafeParent);
        }

        let target_metadata =
            fs::symlink_metadata(path).map_err(|_| DsparkGuardStoreError::UnsafeFile)?;
        validate_file_metadata(&target_metadata, policy)?;
        let target_identity = FileIdentity::from_metadata(&target_metadata);
        let bytes = load_snapshot_control_file(
            path,
            SnapshotSecretFilePolicy {
                expected_owner_uid: policy.owner_uid,
            },
            MAX_STATE_BYTES,
        )
        .map_err(|_| DsparkGuardStoreError::ReadFailed)?;
        let after_read =
            fs::symlink_metadata(path).map_err(|_| DsparkGuardStoreError::UnsafeFile)?;
        validate_file_metadata(&after_read, policy)?;
        if FileIdentity::from_metadata(&after_read) != target_identity {
            return Err(DsparkGuardStoreError::UnsafeFile);
        }
        let (records, runtime_dirty) = decode_document(&bytes, ordered_upstream_sha256)?;
        let uncertain_replicas = if runtime_dirty {
            (0..ordered_upstream_sha256.len())
                .filter(|replica| !records.contains_key(replica))
                .collect()
        } else {
            HashSet::new()
        };
        let store = Self {
            path: path.to_path_buf(),
            parent,
            parent_lock,
            parent_identity,
            policy,
            upstreams: ordered_upstream_sha256.into(),
            state: Mutex::new(StoreState {
                records,
                target_identity,
                runtime_dirty,
                uncertain_replicas,
                poisoned_replicas: HashSet::new(),
            }),
        };
        store.arm_runtime()?;
        Ok(store)
    }

    #[must_use]
    pub fn quarantined_engine_core(&self, replica: usize) -> Option<[u8; 32]> {
        self.state
            .lock()
            .records
            .get(&replica)
            .map(|record| record.engine_core_sha256)
    }

    #[must_use]
    pub fn requires_uncertain_fence(&self, replica: usize) -> bool {
        self.state.lock().uncertain_replicas.contains(&replica)
    }

    /// Keep the precommitted runtime-dirty marker set across a clean process
    /// drop until this replica's unresolved fence is durably published.
    ///
    /// # Errors
    ///
    /// Rejects an out-of-range replica.
    pub fn poison_runtime(&self, replica: usize) -> Result<(), DsparkGuardStoreError> {
        if self.upstreams.get(replica).is_none() {
            return Err(DsparkGuardStoreError::InvalidReplica);
        }
        self.state.lock().poisoned_replicas.insert(replica);
        Ok(())
    }

    /// Resolve an unclean prior LB lifetime by durably fencing the currently
    /// attested `EngineCore` before the new process may serve that replica.
    ///
    /// # Errors
    ///
    /// Rejects invalid/conflicting records or a publication failure. A failed
    /// attempt poisons clean shutdown so the next process remains fenced.
    pub fn persist_uncertain_fence(
        &self,
        replica: usize,
        engine_core_sha256: [u8; 32],
    ) -> Result<StoreMutation, DsparkGuardStoreError> {
        let mutation = self.persist_quarantine(replica, engine_core_sha256)?;
        self.state.lock().uncertain_replicas.remove(&replica);
        Ok(mutation)
    }

    /// Durably add one quarantine before an enforcing caller publishes its
    /// in-memory `quarantined` state. An identical retry is idempotent.
    ///
    /// # Errors
    ///
    /// Rejects an invalid replica or a different existing `EngineCore` record,
    /// and preserves the prior state on every pre-commit publication failure.
    pub fn persist_quarantine(
        &self,
        replica: usize,
        engine_core_sha256: [u8; 32],
    ) -> Result<StoreMutation, DsparkGuardStoreError> {
        let upstream_sha256 = *self
            .upstreams
            .get(replica)
            .ok_or(DsparkGuardStoreError::InvalidReplica)?;
        let mut state = self.state.lock();
        let candidate = QuarantineRecord {
            upstream_sha256,
            engine_core_sha256,
        };
        match state.records.get(&replica) {
            Some(existing) if existing == &candidate => {
                state.poisoned_replicas.remove(&replica);
                return Ok(StoreMutation::Unchanged);
            }
            Some(_) => {
                state.poisoned_replicas.insert(replica);
                return Err(DsparkGuardStoreError::RecordConflict);
            }
            None => {}
        }
        if state
            .records
            .values()
            .any(|record| record.engine_core_sha256 == engine_core_sha256)
        {
            state.poisoned_replicas.insert(replica);
            return Err(DsparkGuardStoreError::Duplicate);
        }
        let mut next = state.records.clone();
        next.insert(replica, candidate);
        if let Err(error) = self.publish_replacement(&mut state, &next, true) {
            state.poisoned_replicas.insert(replica);
            return Err(error);
        }
        state.records = next;
        state.poisoned_replicas.remove(&replica);
        Ok(StoreMutation::Added)
    }

    /// Durably remove one exact quarantine only after a different attested
    /// `EngineCore` commitment has been observed.
    ///
    /// # Errors
    ///
    /// Rejects same-incarnation rearm, an invalid/missing/conflicting record,
    /// or a publication failure. No in-memory record changes before commit.
    pub fn persist_rearm(
        &self,
        replica: usize,
        quarantined_engine_core_sha256: [u8; 32],
        replacement_engine_core_sha256: [u8; 32],
    ) -> Result<StoreMutation, DsparkGuardStoreError> {
        if quarantined_engine_core_sha256 == replacement_engine_core_sha256 {
            return Err(DsparkGuardStoreError::SameEngineCore);
        }
        let expected_upstream = *self
            .upstreams
            .get(replica)
            .ok_or(DsparkGuardStoreError::InvalidReplica)?;
        let mut state = self.state.lock();
        let existing = state
            .records
            .get(&replica)
            .ok_or(DsparkGuardStoreError::MissingRecord)?;
        if existing.upstream_sha256 != expected_upstream
            || existing.engine_core_sha256 != quarantined_engine_core_sha256
        {
            return Err(DsparkGuardStoreError::RecordConflict);
        }
        let mut next = state.records.clone();
        next.remove(&replica);
        if let Err(error) = self.publish_replacement(&mut state, &next, true) {
            state.poisoned_replicas.insert(replica);
            return Err(error);
        }
        state.records = next;
        state.poisoned_replicas.remove(&replica);
        Ok(StoreMutation::Removed)
    }

    fn publish_replacement(
        &self,
        state: &mut StoreState,
        records: &BTreeMap<usize, QuarantineRecord>,
        runtime_dirty: bool,
    ) -> Result<(), DsparkGuardStoreError> {
        let encoded = encode_document(records, runtime_dirty)?;
        if encoded.len() > MAX_STATE_BYTES {
            return Err(DsparkGuardStoreError::Capacity);
        }
        self.revalidate(state.target_identity)?;
        let mut random = [0_u8; 16];
        getrandom::fill(&mut random).map_err(|_| DsparkGuardStoreError::PublishFailed)?;
        let temporary_path = self
            .parent
            .join(format!(".mini-dynamo-dspark-guard-{}.tmp", hex(&random)));
        let mut cleanup = TemporaryOutput::new(temporary_path.clone());
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(FILE_MODE)
            .open(&temporary_path)
            .map_err(|_| DsparkGuardStoreError::PublishFailed)?;
        file.write_all(&encoded)
            .and_then(|()| file.sync_all())
            .map_err(|_| DsparkGuardStoreError::PublishFailed)?;
        let temporary_metadata = file
            .metadata()
            .map_err(|_| DsparkGuardStoreError::PublishFailed)?;
        validate_file_metadata(&temporary_metadata, self.policy)
            .map_err(|_| DsparkGuardStoreError::PublishFailed)?;
        self.revalidate(state.target_identity)?;
        fs::rename(&temporary_path, &self.path)
            .map_err(|_| DsparkGuardStoreError::PublishFailed)?;
        cleanup.disarm();

        let directory_sync = self
            .parent_lock
            .sync_all()
            .map_err(|_| DsparkGuardStoreError::CommitUncertain);
        let published =
            fs::symlink_metadata(&self.path).map_err(|_| DsparkGuardStoreError::CommitUncertain)?;
        validate_file_metadata(&published, self.policy)
            .map_err(|_| DsparkGuardStoreError::CommitUncertain)?;
        state.target_identity = FileIdentity::from_metadata(&published);
        state.runtime_dirty = runtime_dirty;
        directory_sync
    }

    fn arm_runtime(&self) -> Result<(), DsparkGuardStoreError> {
        let mut state = self.state.lock();
        if state.runtime_dirty {
            return Ok(());
        }
        let records = state.records.clone();
        self.publish_replacement(&mut state, &records, true)
    }

    fn revalidate(&self, target_identity: FileIdentity) -> Result<(), DsparkGuardStoreError> {
        let parent = validate_parents(&self.parent, self.policy)?;
        if FileIdentity::from_metadata(&parent) != self.parent_identity {
            return Err(DsparkGuardStoreError::UnsafeParent);
        }
        let locked = self
            .parent_lock
            .metadata()
            .map_err(|_| DsparkGuardStoreError::UnsafeParent)?;
        if FileIdentity::from_metadata(&locked) != self.parent_identity {
            return Err(DsparkGuardStoreError::UnsafeParent);
        }
        let target =
            fs::symlink_metadata(&self.path).map_err(|_| DsparkGuardStoreError::UnsafeFile)?;
        validate_file_metadata(&target, self.policy)?;
        if FileIdentity::from_metadata(&target) != target_identity {
            return Err(DsparkGuardStoreError::UnsafeFile);
        }
        Ok(())
    }
}

impl Drop for DsparkGuardStore {
    fn drop(&mut self) {
        let mut state = self.state.lock();
        if !state.poisoned_replicas.is_empty()
            || !state.uncertain_replicas.is_empty()
            || !state.runtime_dirty
        {
            return;
        }
        let records = state.records.clone();
        let _ = self.publish_replacement(&mut state, &records, false);
    }
}

fn validate_normalized_absolute_path(path: &Path) -> Result<(), DsparkGuardStoreError> {
    if !path.is_absolute()
        || path.as_os_str().as_bytes().len() > MAX_PATH_BYTES
        || path.file_name().is_none()
        || path
            .components()
            .any(|component| !matches!(component, Component::RootDir | Component::Normal(_)))
    {
        return Err(DsparkGuardStoreError::InvalidPath);
    }
    Ok(())
}

fn validate_parents(
    parent: &Path,
    policy: DsparkGuardStorePolicy,
) -> Result<Metadata, DsparkGuardStoreError> {
    let mut current = PathBuf::new();
    for component in parent.components() {
        match component {
            Component::RootDir => current.push(Path::new("/")),
            Component::Normal(value) => current.push(value),
            Component::CurDir | Component::ParentDir | Component::Prefix(_) => {
                return Err(DsparkGuardStoreError::InvalidPath);
            }
        }
        let metadata =
            fs::symlink_metadata(&current).map_err(|_| DsparkGuardStoreError::UnsafeParent)?;
        if !metadata.file_type().is_dir()
            || (metadata.uid() != 0 && metadata.uid() != policy.owner_uid)
        {
            return Err(DsparkGuardStoreError::UnsafeParent);
        }
        if metadata.mode() & GROUP_OR_WORLD_WRITE != 0 {
            let trusted_sticky_root =
                metadata.uid() == 0 && metadata.mode() & STICKY_BIT != 0 && current != parent;
            if !trusted_sticky_root {
                return Err(DsparkGuardStoreError::UnsafeParent);
            }
        }
    }
    let metadata = fs::symlink_metadata(parent).map_err(|_| DsparkGuardStoreError::UnsafeParent)?;
    if metadata.uid() != policy.owner_uid
        || metadata.gid() != policy.group_gid
        || metadata.mode() & 0o777 != PARENT_MODE
    {
        return Err(DsparkGuardStoreError::UnsafeParent);
    }
    Ok(metadata)
}

fn validate_file_metadata(
    metadata: &Metadata,
    policy: DsparkGuardStorePolicy,
) -> Result<(), DsparkGuardStoreError> {
    if !metadata.file_type().is_file()
        || metadata.nlink() != 1
        || metadata.uid() != policy.owner_uid
        || metadata.gid() != policy.group_gid
        || metadata.mode() & 0o777 != FILE_MODE
        || metadata.len() == 0
        || metadata.len() > u64::try_from(MAX_STATE_BYTES).unwrap_or(u64::MAX)
    {
        return Err(DsparkGuardStoreError::UnsafeFile);
    }
    Ok(())
}

fn decode_document(
    bytes: &[u8],
    upstreams: &[[u8; 32]],
) -> Result<(BTreeMap<usize, QuarantineRecord>, bool), DsparkGuardStoreError> {
    let document = serde_json::from_slice::<WireDocument>(bytes)
        .map_err(|_| DsparkGuardStoreError::Malformed)?;
    if document.schema_version != SCHEMA_VERSION {
        return Err(DsparkGuardStoreError::UnsupportedSchema);
    }
    if document.quarantines.len() > upstreams.len() || document.quarantines.len() > MAX_REPLICAS {
        return Err(DsparkGuardStoreError::Capacity);
    }
    let mut records = BTreeMap::new();
    let mut engine_cores = HashSet::new();
    for wire in document.quarantines {
        let expected_upstream = *upstreams
            .get(wire.replica)
            .ok_or(DsparkGuardStoreError::InvalidReplica)?;
        let upstream_sha256 = decode_sha256(&wire.upstream_sha256)?;
        let engine_core_sha256 = decode_sha256(&wire.engine_core_sha256)?;
        if upstream_sha256 != expected_upstream {
            return Err(DsparkGuardStoreError::UpstreamMismatch);
        }
        if records
            .insert(
                wire.replica,
                QuarantineRecord {
                    upstream_sha256,
                    engine_core_sha256,
                },
            )
            .is_some()
            || !engine_cores.insert(engine_core_sha256)
        {
            return Err(DsparkGuardStoreError::Duplicate);
        }
    }
    let canonical = encode_document(&records, document.runtime_dirty)?;
    if canonical != bytes {
        return Err(DsparkGuardStoreError::NonCanonical);
    }
    Ok((records, document.runtime_dirty))
}

fn encode_document(
    records: &BTreeMap<usize, QuarantineRecord>,
    runtime_dirty: bool,
) -> Result<Vec<u8>, DsparkGuardStoreError> {
    let quarantines = records
        .iter()
        .map(|(&replica, record)| WireRecord {
            replica,
            upstream_sha256: hex(&record.upstream_sha256),
            engine_core_sha256: hex(&record.engine_core_sha256),
        })
        .collect();
    serde_json::to_vec(&WireDocument {
        schema_version: SCHEMA_VERSION,
        runtime_dirty,
        quarantines,
    })
    .map_err(|_| DsparkGuardStoreError::EncodeFailed)
}

fn decode_sha256(value: &str) -> Result<[u8; 32], DsparkGuardStoreError> {
    if value.len() != 64
        || !value
            .as_bytes()
            .iter()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte))
    {
        return Err(DsparkGuardStoreError::Malformed);
    }
    let mut decoded = [0_u8; 32];
    for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
        decoded[index] = (nibble(pair[0])? << 4) | nibble(pair[1])?;
    }
    Ok(decoded)
}

fn nibble(value: u8) -> Result<u8, DsparkGuardStoreError> {
    match value {
        b'0'..=b'9' => Ok(value - b'0'),
        b'a'..=b'f' => Ok(value - b'a' + 10),
        _ => Err(DsparkGuardStoreError::Malformed),
    }
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        output.push(char::from(DIGITS[usize::from(byte >> 4)]));
        output.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    output
}

struct TemporaryOutput(Option<PathBuf>);

impl TemporaryOutput {
    fn new(path: PathBuf) -> Self {
        Self(Some(path))
    }

    fn disarm(&mut self) {
        self.0 = None;
    }
}

impl Drop for TemporaryOutput {
    fn drop(&mut self) {
        if let Some(path) = self.0.take() {
            let _ = fs::remove_file(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        os::unix::fs::{PermissionsExt, symlink},
        sync::atomic::{AtomicU64, Ordering},
    };

    use super::*;

    static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

    struct TestFiles {
        root: PathBuf,
        state: PathBuf,
        policy: DsparkGuardStorePolicy,
    }

    impl TestFiles {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "md-dspark-store-{}-{}",
                std::process::id(),
                NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&root).unwrap();
            fs::set_permissions(&root, fs::Permissions::from_mode(PARENT_MODE)).unwrap();
            let metadata = fs::symlink_metadata(&root).unwrap();
            let state = root.join("state.json");
            let bytes = encode_document(&BTreeMap::new(), false).unwrap();
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(FILE_MODE)
                .open(&state)
                .unwrap();
            file.write_all(&bytes).unwrap();
            file.sync_all().unwrap();
            Self {
                root,
                state,
                policy: DsparkGuardStorePolicy {
                    owner_uid: metadata.uid(),
                    group_gid: metadata.gid(),
                },
            }
        }

        fn open(&self, upstreams: &[[u8; 32]]) -> DsparkGuardStore {
            DsparkGuardStore::open(&self.state, self.policy, upstreams).unwrap()
        }
    }

    impl Drop for TestFiles {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn add_is_durable_idempotent_and_reloads() {
        let files = TestFiles::new();
        let upstreams = [[1; 32], [2; 32]];
        {
            let store = files.open(&upstreams);
            assert_eq!(
                store.persist_quarantine(1, [9; 32]),
                Ok(StoreMutation::Added)
            );
            assert_eq!(
                store.persist_quarantine(1, [9; 32]),
                Ok(StoreMutation::Unchanged)
            );
            assert_eq!(store.quarantined_engine_core(1), Some([9; 32]));
        }
        let reopened = files.open(&upstreams);
        assert!(!reopened.requires_uncertain_fence(0));
        assert!(!reopened.requires_uncertain_fence(1));
        assert_eq!(reopened.quarantined_engine_core(0), None);
        assert_eq!(reopened.quarantined_engine_core(1), Some([9; 32]));
    }

    #[test]
    fn rearm_requires_changed_core_and_is_durable() {
        let files = TestFiles::new();
        let upstreams = [[1; 32], [2; 32]];
        {
            let store = files.open(&upstreams);
            store.persist_quarantine(0, [8; 32]).unwrap();
            let before = fs::read(&files.state).unwrap();
            assert_eq!(
                store.persist_rearm(0, [8; 32], [8; 32]),
                Err(DsparkGuardStoreError::SameEngineCore)
            );
            assert_eq!(fs::read(&files.state).unwrap(), before);
            assert_eq!(
                store.persist_rearm(0, [8; 32], [7; 32]),
                Ok(StoreMutation::Removed)
            );
        }
        assert_eq!(files.open(&upstreams).quarantined_engine_core(0), None);
    }

    #[test]
    fn conflicting_mutations_preserve_prior_file() {
        let files = TestFiles::new();
        let upstreams = [[1; 32]];
        let store = files.open(&upstreams);
        store.persist_quarantine(0, [8; 32]).unwrap();
        let before = fs::read(&files.state).unwrap();
        assert_eq!(
            store.persist_quarantine(0, [7; 32]),
            Err(DsparkGuardStoreError::RecordConflict)
        );
        assert_eq!(
            store.persist_rearm(0, [6; 32], [5; 32]),
            Err(DsparkGuardStoreError::RecordConflict)
        );
        assert_eq!(fs::read(&files.state).unwrap(), before);
    }

    #[test]
    fn precommit_publication_failure_preserves_memory_and_disk() {
        let files = TestFiles::new();
        let upstreams = [[1; 32], [2; 32]];
        let store = files.open(&upstreams);
        store.persist_quarantine(0, [8; 32]).unwrap();
        let before = fs::read(&files.state).unwrap();

        fs::set_permissions(&files.root, fs::Permissions::from_mode(0o500)).unwrap();
        assert_eq!(
            store.persist_quarantine(1, [7; 32]),
            Err(DsparkGuardStoreError::UnsafeParent)
        );
        assert_eq!(store.quarantined_engine_core(0), Some([8; 32]));
        assert_eq!(store.quarantined_engine_core(1), None);
        assert_eq!(fs::read(&files.state).unwrap(), before);
        fs::set_permissions(&files.root, fs::Permissions::from_mode(PARENT_MODE)).unwrap();
        drop(store);

        let restarted = files.open(&upstreams);
        assert!(!restarted.requires_uncertain_fence(0));
        assert!(restarted.requires_uncertain_fence(1));
        assert_eq!(
            restarted.persist_uncertain_fence(1, [7; 32]),
            Ok(StoreMutation::Added)
        );
        assert!(!restarted.requires_uncertain_fence(1));
    }

    #[test]
    fn duplicate_engine_core_is_rejected_before_publication() {
        let files = TestFiles::new();
        let upstreams = [[1; 32], [2; 32]];
        let store = files.open(&upstreams);
        store.persist_quarantine(0, [8; 32]).unwrap();
        let before = fs::read(&files.state).unwrap();
        assert_eq!(
            store.persist_quarantine(1, [8; 32]),
            Err(DsparkGuardStoreError::Duplicate)
        );
        assert_eq!(store.quarantined_engine_core(1), None);
        assert_eq!(fs::read(&files.state).unwrap(), before);
    }

    #[test]
    fn exclusive_parent_lock_rejects_a_second_owner() {
        let files = TestFiles::new();
        let upstreams = [[1; 32]];
        let _owner = files.open(&upstreams);
        assert!(matches!(
            DsparkGuardStore::open(&files.state, files.policy, &upstreams),
            Err(DsparkGuardStoreError::DirectoryLocked)
        ));
    }

    #[test]
    fn live_record_rejects_upstream_reorder_or_change() {
        let files = TestFiles::new();
        let upstreams = [[1; 32], [2; 32]];
        {
            let store = files.open(&upstreams);
            store.persist_quarantine(0, [8; 32]).unwrap();
        }
        assert!(matches!(
            DsparkGuardStore::open(&files.state, files.policy, &[[2; 32], [1; 32]]),
            Err(DsparkGuardStoreError::UpstreamMismatch)
        ));
    }

    #[test]
    fn malformed_noncanonical_unknown_and_duplicate_documents_fail_closed() {
        for bytes in [
            b"not-json".as_slice(),
            b"{\"schema_version\":2,\"quarantines\":[]}".as_slice(),
            b"{ \"schema_version\":1,\"quarantines\":[]}".as_slice(),
            b"{\"schema_version\":1,\"quarantines\":[],\"extra\":0}".as_slice(),
        ] {
            let files = TestFiles::new();
            fs::write(&files.state, bytes).unwrap();
            fs::set_permissions(&files.state, fs::Permissions::from_mode(FILE_MODE)).unwrap();
            assert!(DsparkGuardStore::open(&files.state, files.policy, &[[1; 32]]).is_err());
        }

        let files = TestFiles::new();
        let record = WireRecord {
            replica: 0,
            upstream_sha256: hex(&[1; 32]),
            engine_core_sha256: hex(&[8; 32]),
        };
        let bytes = serde_json::to_vec(&WireDocument {
            schema_version: 1,
            runtime_dirty: false,
            quarantines: vec![
                record,
                WireRecord {
                    replica: 0,
                    upstream_sha256: hex(&[1; 32]),
                    engine_core_sha256: hex(&[7; 32]),
                },
            ],
        })
        .unwrap();
        fs::write(&files.state, bytes).unwrap();
        fs::set_permissions(&files.state, fs::Permissions::from_mode(FILE_MODE)).unwrap();
        assert!(matches!(
            DsparkGuardStore::open(&files.state, files.policy, &[[1; 32]]),
            Err(DsparkGuardStoreError::Capacity | DsparkGuardStoreError::Duplicate)
        ));
    }

    #[test]
    fn unsafe_file_and_parent_shapes_are_rejected() {
        let files = TestFiles::new();
        fs::set_permissions(&files.state, fs::Permissions::from_mode(0o640)).unwrap();
        assert!(matches!(
            DsparkGuardStore::open(&files.state, files.policy, &[[1; 32]]),
            Err(DsparkGuardStoreError::UnsafeFile)
        ));

        let files = TestFiles::new();
        let hardlink = files.root.join("second-link");
        fs::hard_link(&files.state, hardlink).unwrap();
        assert!(matches!(
            DsparkGuardStore::open(&files.state, files.policy, &[[1; 32]]),
            Err(DsparkGuardStoreError::UnsafeFile)
        ));

        let files = TestFiles::new();
        let target = files.root.join("target");
        fs::rename(&files.state, &target).unwrap();
        symlink(&target, &files.state).unwrap();
        assert!(matches!(
            DsparkGuardStore::open(&files.state, files.policy, &[[1; 32]]),
            Err(DsparkGuardStoreError::UnsafeFile)
        ));

        let files = TestFiles::new();
        fs::set_permissions(&files.root, fs::Permissions::from_mode(0o770)).unwrap();
        assert!(matches!(
            DsparkGuardStore::open(&files.state, files.policy, &[[1; 32]]),
            Err(DsparkGuardStoreError::UnsafeParent)
        ));
    }

    #[test]
    fn debug_errors_and_state_are_content_free() {
        let files = TestFiles::new();
        let store = files.open(&[[1; 32]]);
        let debug = format!("{store:?}");
        assert!(debug.contains("<redacted>"));
        assert!(!debug.contains(files.state.to_string_lossy().as_ref()));
        assert_eq!(
            DsparkGuardStoreError::UpstreamMismatch.label(),
            "upstream_mismatch"
        );
    }
}
