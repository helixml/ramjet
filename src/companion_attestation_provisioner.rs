//! Host-side provisioning for authenticated companion engine incarnations.
//!
//! The provisioner consumes the bounded, privacy-safe JSON emitted by
//! `bench/node06_engine_metadata.sh`. It never talks to Docker or an engine:
//! the explicit metadata file is the only identity source. Publication is a
//! same-directory, fsynced atomic replacement under a trusted path.

use std::{
    collections::BTreeMap,
    env, fmt,
    fs::{self, File, Metadata, OpenOptions},
    io::Write,
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt, chown},
    path::{Component, Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use chrono::{DateTime, FixedOffset};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::{
    companion_attestation::{
        CompanionAttestationError, encode_authenticated_engine_incarnation,
        load_authenticated_engine_incarnation, load_companion_digest_secret,
    },
    kv_snapshot::EngineIncarnation,
    snapshot_secret_file::{
        SnapshotSecretFileError, SnapshotSecretFilePolicy, load_snapshot_control_file,
    },
};

const METADATA_SCHEMA_VERSION: u16 = 1;
const MAX_METADATA_BYTES: usize = 64 * 1024;
const MAX_PATH_BYTES: usize = 4_096;
const MAX_COMPONENT_BYTES: usize = 512;
const MAX_COLLECTION_ENTRIES: usize = 64;
const MAX_METADATA_AGE_MS: u64 = 300_000;
const DEFAULT_METADATA_AGE_MS: u64 = 30_000;
const FUTURE_SKEW_NS: u64 = 5_000_000_000;
const PROCESS_START_ROUNDING_NS: u64 = 2_000_000_000;
const OUTPUT_MODE: u32 = 0o440;
const GROUP_OR_WORLD_WRITE: u32 = 0o022;
const STICKY_BIT: u32 = 0o1000;

/// Environment-only provisioner configuration. Paths are redacted in Debug.
#[derive(Clone)]
pub struct CompanionAttestationProvisionerConfig {
    pub metadata_path: PathBuf,
    pub digest_secret_path: PathBuf,
    pub attestation_path: PathBuf,
    pub owner_uid: u32,
    pub group_gid: u32,
    pub max_metadata_age: Duration,
}

impl fmt::Debug for CompanionAttestationProvisionerConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CompanionAttestationProvisionerConfig")
            .field("metadata_path", &"[REDACTED]")
            .field("digest_secret_path", &"[REDACTED]")
            .field("attestation_path", &"[REDACTED]")
            .field("owner_uid", &self.owner_uid)
            .field("group_gid", &self.group_gid)
            .field("max_metadata_age", &self.max_metadata_age)
            .finish()
    }
}

impl CompanionAttestationProvisionerConfig {
    /// Load the no-argument provisioner environment contract.
    ///
    /// # Errors
    ///
    /// Rejects missing, aliased, non-normalized, or out-of-range settings.
    pub fn from_env() -> Result<Self, CompanionAttestationProvisionerConfigError> {
        Self::from_lookup(|key| env::var(key).ok())
    }

    /// Deterministic constructor for tests and service managers.
    ///
    /// # Errors
    ///
    /// Rejects invalid settings without embedding their values in the error.
    pub fn from_lookup(
        mut get: impl FnMut(&str) -> Option<String>,
    ) -> Result<Self, CompanionAttestationProvisionerConfigError> {
        let metadata_path = required_path(&mut get, "RJ_SNAPSHOT_ENGINE_METADATA_PATH")?;
        let digest_secret_path = required_path(&mut get, "RJ_SNAPSHOT_DIGEST_SECRET_PATH")?;
        let attestation_path = required_path(&mut get, "RJ_SNAPSHOT_ATTESTATION_PATH")?;
        if metadata_path == digest_secret_path
            || metadata_path == attestation_path
            || digest_secret_path == attestation_path
        {
            return Err(CompanionAttestationProvisionerConfigError::AliasedPaths);
        }
        let owner_uid = required_u32(&mut get, "RJ_SNAPSHOT_SECRET_OWNER_UID")?;
        let group_gid = required_u32(&mut get, "RJ_SNAPSHOT_SECRET_GROUP_GID")?;
        let max_age_ms = get("RJ_SNAPSHOT_ATTESTATION_MAX_AGE_MS")
            .as_deref()
            .map(str::parse::<u64>)
            .transpose()
            .map_err(
                |_| CompanionAttestationProvisionerConfigError::InvalidSetting {
                    key: "RJ_SNAPSHOT_ATTESTATION_MAX_AGE_MS",
                },
            )?
            .unwrap_or(DEFAULT_METADATA_AGE_MS);
        if !(1..=MAX_METADATA_AGE_MS).contains(&max_age_ms) {
            return Err(CompanionAttestationProvisionerConfigError::InvalidSetting {
                key: "RJ_SNAPSHOT_ATTESTATION_MAX_AGE_MS",
            });
        }
        Ok(Self {
            metadata_path,
            digest_secret_path,
            attestation_path,
            owner_uid,
            group_gid,
            max_metadata_age: Duration::from_millis(max_age_ms),
        })
    }
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum CompanionAttestationProvisionerConfigError {
    #[error("missing attestation provisioner setting {key}")]
    Missing { key: &'static str },
    #[error("invalid attestation provisioner setting {key}")]
    InvalidSetting { key: &'static str },
    #[error("attestation provisioner protected paths must be distinct")]
    AliasedPaths,
}

impl CompanionAttestationProvisionerConfigError {
    #[must_use]
    pub const fn reason(self) -> &'static str {
        match self {
            Self::Missing { .. } => "missing_setting",
            Self::InvalidSetting { .. } => "invalid_setting",
            Self::AliasedPaths => "aliased_paths",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CompanionAttestationProvisionOutcome {
    Created,
    Updated,
    Unchanged,
}

#[derive(Debug, Error)]
pub enum CompanionAttestationProvisionerError {
    #[error("attestation provisioner configuration failed")]
    Config(#[from] CompanionAttestationProvisionerConfigError),
    #[error("attestation provisioner metadata file validation failed")]
    MetadataFile(#[from] SnapshotSecretFileError),
    #[error("attestation provisioner digest secret validation failed")]
    DigestSecret(CompanionAttestationError),
    #[error("attestation provisioner metadata is malformed")]
    MalformedMetadata,
    #[error("attestation provisioner metadata schema is unsupported")]
    UnsupportedMetadata,
    #[error("attestation provisioner metadata fields are invalid")]
    InvalidMetadata,
    #[error("attestation provisioner metadata is stale")]
    StaleMetadata,
    #[error("attestation provisioner metadata is from the future")]
    FutureMetadata,
    #[error("attestation provisioner metadata receipt is not authoritative")]
    UnverifiedMetadata,
    #[error("attestation provisioner destination is unsafe")]
    UnsafeDestination,
    #[error("attestation provisioner existing output is invalid")]
    InvalidExistingOutput,
    #[error("attestation provisioner refused an engine-incarnation rollback")]
    IdentityRollback,
    #[error("attestation provisioner found conflicting identity for one process")]
    IdentityConflict,
    #[error("attestation provisioner could not publish output")]
    PublishFailed,
}

impl CompanionAttestationProvisionerError {
    #[must_use]
    pub const fn reason(&self) -> &'static str {
        match self {
            Self::Config(error) => error.reason(),
            Self::MetadataFile(error) => error.reason(),
            Self::DigestSecret(error) => error.reason(),
            Self::MalformedMetadata => "malformed_metadata",
            Self::UnsupportedMetadata => "unsupported_metadata",
            Self::InvalidMetadata => "invalid_metadata",
            Self::StaleMetadata => "stale_metadata",
            Self::FutureMetadata => "future_metadata",
            Self::UnverifiedMetadata => "unverified_metadata",
            Self::UnsafeDestination => "unsafe_destination",
            Self::InvalidExistingOutput => "invalid_existing_output",
            Self::IdentityRollback => "identity_rollback",
            Self::IdentityConflict => "identity_conflict",
            Self::PublishFailed => "publish_failed",
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EngineMetadata {
    schema_version: u16,
    live: LiveEngineMetadata,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    receipt: Option<ReceiptMetadata>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    verified: Option<bool>,
}

fn deserialize_required_nullable<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

#[derive(Deserialize)]
struct ReceiptMetadata {
    receipt_sha256: String,
    status: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LiveEngineMetadata {
    captured_utc: String,
    container: String,
    configured_image: String,
    image_id: String,
    #[serde(default)]
    image_descriptor_digest: String,
    #[serde(default)]
    image_config_digest: String,
    #[serde(default)]
    repo_digests: Vec<String>,
    model_revision: String,
    tokenizer_revision: String,
    tokenizer_sha256: String,
    config_sha256: String,
    driver: String,
    topology_sha256: String,
    started_at: String,
    process_started_unix_ns: u64,
    restart_count: u64,
    cpuset_cpus: String,
    cpuset_mems: String,
    #[serde(default)]
    runtime_packages: BTreeMap<String, String>,
    argv_sha256: String,
    #[serde(default)]
    effective_contract: BTreeMap<String, String>,
}

#[derive(Serialize)]
struct IdentityEvidence<'a> {
    schema_version: u16,
    engine_id: &'a str,
    configured_image: &'a str,
    image_id: &'a str,
    image_descriptor_digest: &'a str,
    image_config_digest: &'a str,
    repo_digests: Vec<&'a str>,
    model_revision: &'a str,
    tokenizer_revision: &'a str,
    tokenizer_sha256: &'a str,
    config_sha256: &'a str,
    driver: &'a str,
    topology_sha256: &'a str,
    container_started_unix_ns: u64,
    process_started_unix_ns: u64,
    restart_count: u64,
    cpuset_cpus: &'a str,
    cpuset_mems: &'a str,
    runtime_packages: &'a BTreeMap<String, String>,
    argv_sha256: &'a str,
    effective_contract: &'a BTreeMap<String, String>,
    receipt_sha256: Option<&'a str>,
}

struct DestinationState {
    parent: PathBuf,
    _lock: File,
    parent_identity: FileIdentity,
    target_identity: Option<FileIdentity>,
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

/// Provision one authenticated incarnation using the current wall clock.
///
/// # Errors
///
/// Fails closed before replacement for every metadata, freshness, path,
/// ownership, rollback, authentication, or I/O violation.
pub fn provision_authenticated_engine_incarnation(
    config: &CompanionAttestationProvisionerConfig,
) -> Result<CompanionAttestationProvisionOutcome, CompanionAttestationProvisionerError> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| CompanionAttestationProvisionerError::FutureMetadata)?;
    let now_ns = u64::try_from(now.as_nanos())
        .map_err(|_| CompanionAttestationProvisionerError::FutureMetadata)?;
    provision_authenticated_engine_incarnation_at(config, now_ns)
}

fn provision_authenticated_engine_incarnation_at(
    config: &CompanionAttestationProvisionerConfig,
    now_ns: u64,
) -> Result<CompanionAttestationProvisionOutcome, CompanionAttestationProvisionerError> {
    let policy = SnapshotSecretFilePolicy {
        expected_owner_uid: config.owner_uid,
    };
    validate_private_metadata_file(&config.metadata_path, config.owner_uid)?;
    let mut metadata_bytes =
        load_snapshot_control_file(&config.metadata_path, policy, MAX_METADATA_BYTES)?;
    validate_private_metadata_file(&config.metadata_path, config.owner_uid)?;
    let parsed = serde_json::from_slice::<EngineMetadata>(&metadata_bytes)
        .map_err(|_| CompanionAttestationProvisionerError::MalformedMetadata);
    metadata_bytes.fill(0);
    let metadata = parsed?;
    let incarnation = derive_incarnation(&metadata, config.max_metadata_age, now_ns)?;
    let secret = load_companion_digest_secret(&config.digest_secret_path, policy)
        .map_err(CompanionAttestationProvisionerError::DigestSecret)?;
    let encoded = encode_authenticated_engine_incarnation(&incarnation, &secret)
        .map_err(CompanionAttestationProvisionerError::DigestSecret)?;
    let destination = validate_destination(config)?;

    let existing = match destination.target_identity {
        Some(_) => Some(
            load_authenticated_engine_incarnation(&config.attestation_path, policy, &secret)
                .map_err(|_| CompanionAttestationProvisionerError::InvalidExistingOutput)?,
        ),
        None => None,
    };
    if let Some(current) = &existing {
        compare_existing_identity(current, &incarnation)?;
        if current == &incarnation {
            revalidate_destination(config, &destination)?;
            return Ok(CompanionAttestationProvisionOutcome::Unchanged);
        }
    }
    publish_atomic(config, &destination, &encoded)?;
    let published =
        load_authenticated_engine_incarnation(&config.attestation_path, policy, &secret)
            .map_err(|_| CompanionAttestationProvisionerError::PublishFailed)?;
    if published != incarnation {
        return Err(CompanionAttestationProvisionerError::PublishFailed);
    }
    Ok(if existing.is_some() {
        CompanionAttestationProvisionOutcome::Updated
    } else {
        CompanionAttestationProvisionOutcome::Created
    })
}

fn derive_incarnation(
    metadata: &EngineMetadata,
    max_age: Duration,
    now_ns: u64,
) -> Result<EngineIncarnation, CompanionAttestationProvisionerError> {
    if metadata.schema_version != METADATA_SCHEMA_VERSION {
        return Err(CompanionAttestationProvisionerError::UnsupportedMetadata);
    }
    match (&metadata.receipt, metadata.verified) {
        (Some(receipt), Some(true)) if receipt.status.as_deref() == Some("qualified") => {}
        (None, None) => {}
        _ => return Err(CompanionAttestationProvisionerError::UnverifiedMetadata),
    }
    let live = &metadata.live;
    validate_live_metadata(live, metadata.receipt.as_ref())?;
    let captured_ns = parse_utc_ns(&live.captured_utc)?;
    let container_started_ns = parse_utc_ns(&live.started_at)?;
    let process_started_ns = live.process_started_unix_ns;
    if captured_ns < container_started_ns
        || process_started_ns == 0
        || captured_ns < process_started_ns
        || process_started_ns.saturating_add(PROCESS_START_ROUNDING_NS) < container_started_ns
    {
        return Err(CompanionAttestationProvisionerError::InvalidMetadata);
    }
    if captured_ns > now_ns.saturating_add(FUTURE_SKEW_NS)
        || container_started_ns > now_ns.saturating_add(FUTURE_SKEW_NS)
        || process_started_ns > now_ns.saturating_add(FUTURE_SKEW_NS)
    {
        return Err(CompanionAttestationProvisionerError::FutureMetadata);
    }
    let max_age_ns = u64::try_from(max_age.as_nanos()).unwrap_or(u64::MAX);
    if now_ns.saturating_sub(captured_ns) > max_age_ns {
        return Err(CompanionAttestationProvisionerError::StaleMetadata);
    }
    let mut repo_digests = live
        .repo_digests
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>();
    repo_digests.sort_unstable();
    let evidence = IdentityEvidence {
        schema_version: METADATA_SCHEMA_VERSION,
        engine_id: &live.container,
        configured_image: &live.configured_image,
        image_id: &live.image_id,
        image_descriptor_digest: &live.image_descriptor_digest,
        image_config_digest: &live.image_config_digest,
        repo_digests,
        model_revision: &live.model_revision,
        tokenizer_revision: &live.tokenizer_revision,
        tokenizer_sha256: &live.tokenizer_sha256,
        config_sha256: &live.config_sha256,
        driver: &live.driver,
        topology_sha256: &live.topology_sha256,
        container_started_unix_ns: container_started_ns,
        process_started_unix_ns: process_started_ns,
        restart_count: live.restart_count,
        cpuset_cpus: &live.cpuset_cpus,
        cpuset_mems: &live.cpuset_mems,
        runtime_packages: &live.runtime_packages,
        argv_sha256: &live.argv_sha256,
        effective_contract: &live.effective_contract,
        receipt_sha256: metadata
            .receipt
            .as_ref()
            .map(|value| value.receipt_sha256.as_str()),
    };
    let canonical = rmp_serde::to_vec_named(&evidence)
        .map_err(|_| CompanionAttestationProvisionerError::InvalidMetadata)?;
    let attestation_sha256 = Sha256::digest(canonical).to_vec();
    Ok(EngineIncarnation {
        engine_id: live.container.clone(),
        model_revision: live.model_revision.clone(),
        image_digest: live.image_id.clone(),
        process_started_unix_ns: process_started_ns,
        attestation_sha256,
    })
}

fn validate_live_metadata(
    live: &LiveEngineMetadata,
    receipt: Option<&ReceiptMetadata>,
) -> Result<(), CompanionAttestationProvisionerError> {
    for value in [
        &live.container,
        &live.configured_image,
        &live.model_revision,
        &live.tokenizer_revision,
        &live.driver,
        &live.cpuset_cpus,
    ] {
        validate_component(value)?;
    }
    if live.cpuset_mems.len() > MAX_COMPONENT_BYTES
        || live.cpuset_mems.chars().any(char::is_control)
    {
        return Err(CompanionAttestationProvisionerError::InvalidMetadata);
    }
    validate_sha256_digest(&live.image_id)?;
    for value in [&live.image_descriptor_digest, &live.image_config_digest] {
        if !value.is_empty() {
            validate_sha256_digest(value)?;
        }
    }
    for value in [
        &live.tokenizer_sha256,
        &live.config_sha256,
        &live.topology_sha256,
        &live.argv_sha256,
    ] {
        validate_sha256_hex(value)?;
    }
    if live.repo_digests.len() > MAX_COLLECTION_ENTRIES
        || live.runtime_packages.len() > MAX_COLLECTION_ENTRIES
        || live.effective_contract.len() > MAX_COLLECTION_ENTRIES
    {
        return Err(CompanionAttestationProvisionerError::InvalidMetadata);
    }
    for digest in &live.repo_digests {
        let Some((name, value)) = digest.rsplit_once('@') else {
            return Err(CompanionAttestationProvisionerError::InvalidMetadata);
        };
        validate_component(name)?;
        validate_sha256_digest(value)?;
    }
    for (key, value) in live.runtime_packages.iter().chain(&live.effective_contract) {
        validate_component(key)?;
        validate_component(value)?;
    }
    if let Some(receipt) = receipt {
        validate_sha256_hex(&receipt.receipt_sha256)?;
    }
    Ok(())
}

fn validate_component(value: &str) -> Result<(), CompanionAttestationProvisionerError> {
    if value.is_empty() || value.len() > MAX_COMPONENT_BYTES || value.chars().any(char::is_control)
    {
        return Err(CompanionAttestationProvisionerError::InvalidMetadata);
    }
    Ok(())
}

fn validate_sha256_digest(value: &str) -> Result<(), CompanionAttestationProvisionerError> {
    let Some(hex) = value.strip_prefix("sha256:") else {
        return Err(CompanionAttestationProvisionerError::InvalidMetadata);
    };
    validate_sha256_hex(hex)
}

fn validate_sha256_hex(value: &str) -> Result<(), CompanionAttestationProvisionerError> {
    if value.len() != 64 || !value.as_bytes().iter().all(u8::is_ascii_hexdigit) {
        return Err(CompanionAttestationProvisionerError::InvalidMetadata);
    }
    Ok(())
}

fn parse_utc_ns(value: &str) -> Result<u64, CompanionAttestationProvisionerError> {
    let parsed = DateTime::<FixedOffset>::parse_from_rfc3339(value)
        .map_err(|_| CompanionAttestationProvisionerError::InvalidMetadata)?;
    if parsed.offset().local_minus_utc() != 0 {
        return Err(CompanionAttestationProvisionerError::InvalidMetadata);
    }
    let nanoseconds = parsed
        .timestamp_nanos_opt()
        .ok_or(CompanionAttestationProvisionerError::InvalidMetadata)?;
    u64::try_from(nanoseconds).map_err(|_| CompanionAttestationProvisionerError::InvalidMetadata)
}

fn compare_existing_identity(
    current: &EngineIncarnation,
    candidate: &EngineIncarnation,
) -> Result<(), CompanionAttestationProvisionerError> {
    if current.engine_id != candidate.engine_id {
        return Err(CompanionAttestationProvisionerError::IdentityConflict);
    }
    match candidate
        .process_started_unix_ns
        .cmp(&current.process_started_unix_ns)
    {
        std::cmp::Ordering::Less => Err(CompanionAttestationProvisionerError::IdentityRollback),
        std::cmp::Ordering::Equal if candidate != current => {
            Err(CompanionAttestationProvisionerError::IdentityConflict)
        }
        _ => Ok(()),
    }
}

fn validate_private_metadata_file(
    path: &Path,
    owner_uid: u32,
) -> Result<(), CompanionAttestationProvisionerError> {
    let metadata = fs::symlink_metadata(path).map_err(|_| {
        CompanionAttestationProvisionerError::MetadataFile(SnapshotSecretFileError::InvalidMetadata)
    })?;
    if !metadata.file_type().is_file()
        || metadata.nlink() != 1
        || metadata.uid() != owner_uid
        || metadata.mode() & 0o777 != 0o600
        || metadata.len() == 0
        || metadata.len() > u64::try_from(MAX_METADATA_BYTES).unwrap_or(u64::MAX)
    {
        return Err(CompanionAttestationProvisionerError::MetadataFile(
            SnapshotSecretFileError::UnsafePermissions,
        ));
    }
    Ok(())
}

fn validate_destination(
    config: &CompanionAttestationProvisionerConfig,
) -> Result<DestinationState, CompanionAttestationProvisionerError> {
    validate_normalized_absolute_path(&config.attestation_path)
        .map_err(|()| CompanionAttestationProvisionerError::UnsafeDestination)?;
    let parent = config
        .attestation_path
        .parent()
        .ok_or(CompanionAttestationProvisionerError::UnsafeDestination)?
        .to_path_buf();
    let mut current = PathBuf::new();
    for component in parent.components() {
        match component {
            Component::RootDir => current.push(Path::new("/")),
            Component::Normal(value) => current.push(value),
            Component::CurDir | Component::ParentDir | Component::Prefix(_) => {
                return Err(CompanionAttestationProvisionerError::UnsafeDestination);
            }
        }
        let metadata = fs::symlink_metadata(&current)
            .map_err(|_| CompanionAttestationProvisionerError::UnsafeDestination)?;
        if !metadata.file_type().is_dir()
            || (metadata.uid() != 0 && metadata.uid() != config.owner_uid)
        {
            return Err(CompanionAttestationProvisionerError::UnsafeDestination);
        }
        if metadata.mode() & GROUP_OR_WORLD_WRITE != 0 {
            let trusted_sticky_root =
                metadata.uid() == 0 && metadata.mode() & STICKY_BIT != 0 && current != parent;
            if !trusted_sticky_root {
                return Err(CompanionAttestationProvisionerError::UnsafeDestination);
            }
        }
    }
    let parent_metadata = fs::symlink_metadata(&parent)
        .map_err(|_| CompanionAttestationProvisionerError::UnsafeDestination)?;
    let parent_lock =
        File::open(&parent).map_err(|_| CompanionAttestationProvisionerError::UnsafeDestination)?;
    parent_lock
        .try_lock()
        .map_err(|_| CompanionAttestationProvisionerError::UnsafeDestination)?;
    let locked_metadata = parent_lock
        .metadata()
        .map_err(|_| CompanionAttestationProvisionerError::UnsafeDestination)?;
    if FileIdentity::from_metadata(&locked_metadata)
        != FileIdentity::from_metadata(&parent_metadata)
    {
        return Err(CompanionAttestationProvisionerError::UnsafeDestination);
    }
    let target_identity = match fs::symlink_metadata(&config.attestation_path) {
        Ok(metadata) => {
            validate_output_metadata(&metadata, config)?;
            Some(FileIdentity::from_metadata(&metadata))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(_) => return Err(CompanionAttestationProvisionerError::UnsafeDestination),
    };
    Ok(DestinationState {
        parent,
        _lock: parent_lock,
        parent_identity: FileIdentity::from_metadata(&parent_metadata),
        target_identity,
    })
}

fn validate_output_metadata(
    metadata: &Metadata,
    config: &CompanionAttestationProvisionerConfig,
) -> Result<(), CompanionAttestationProvisionerError> {
    if !metadata.file_type().is_file()
        || metadata.nlink() != 1
        || metadata.uid() != config.owner_uid
        || metadata.gid() != config.group_gid
        || metadata.mode() & 0o777 != OUTPUT_MODE
    {
        return Err(CompanionAttestationProvisionerError::UnsafeDestination);
    }
    Ok(())
}

fn publish_atomic(
    config: &CompanionAttestationProvisionerConfig,
    state: &DestinationState,
    encoded: &[u8],
) -> Result<(), CompanionAttestationProvisionerError> {
    revalidate_destination(config, state)?;
    let mut random = [0_u8; 16];
    getrandom::fill(&mut random)
        .map_err(|_| CompanionAttestationProvisionerError::PublishFailed)?;
    let temporary_name = format!(".ramjet-attestation-{}.tmp", hex(&random));
    let temporary_path = state.parent.join(temporary_name);
    let mut cleanup = TemporaryOutput::new(temporary_path.clone());
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary_path)
        .map_err(|_| CompanionAttestationProvisionerError::PublishFailed)?;
    file.write_all(encoded)
        .and_then(|()| file.sync_all())
        .map_err(|_| CompanionAttestationProvisionerError::PublishFailed)?;
    chown(
        &temporary_path,
        Some(config.owner_uid),
        Some(config.group_gid),
    )
    .map_err(|_| CompanionAttestationProvisionerError::PublishFailed)?;
    file.set_permissions(fs::Permissions::from_mode(OUTPUT_MODE))
        .and_then(|()| file.sync_all())
        .map_err(|_| CompanionAttestationProvisionerError::PublishFailed)?;
    let metadata = file
        .metadata()
        .map_err(|_| CompanionAttestationProvisionerError::PublishFailed)?;
    validate_output_metadata(&metadata, config)
        .map_err(|_| CompanionAttestationProvisionerError::PublishFailed)?;
    revalidate_destination(config, state)?;
    fs::rename(&temporary_path, &config.attestation_path)
        .map_err(|_| CompanionAttestationProvisionerError::PublishFailed)?;
    cleanup.disarm();
    File::open(&state.parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|_| CompanionAttestationProvisionerError::PublishFailed)?;
    let published = fs::symlink_metadata(&config.attestation_path)
        .map_err(|_| CompanionAttestationProvisionerError::PublishFailed)?;
    validate_output_metadata(&published, config)
        .map_err(|_| CompanionAttestationProvisionerError::PublishFailed)
}

fn revalidate_destination(
    config: &CompanionAttestationProvisionerConfig,
    state: &DestinationState,
) -> Result<(), CompanionAttestationProvisionerError> {
    let parent = fs::symlink_metadata(&state.parent)
        .map_err(|_| CompanionAttestationProvisionerError::UnsafeDestination)?;
    if FileIdentity::from_metadata(&parent) != state.parent_identity {
        return Err(CompanionAttestationProvisionerError::UnsafeDestination);
    }
    let current_target = match fs::symlink_metadata(&config.attestation_path) {
        Ok(metadata) => {
            validate_output_metadata(&metadata, config)?;
            Some(FileIdentity::from_metadata(&metadata))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(_) => return Err(CompanionAttestationProvisionerError::UnsafeDestination),
    };
    if current_target != state.target_identity {
        return Err(CompanionAttestationProvisionerError::UnsafeDestination);
    }
    Ok(())
}

struct TemporaryOutput {
    path: PathBuf,
    armed: bool,
}

impl TemporaryOutput {
    const fn new(path: PathBuf) -> Self {
        Self { path, armed: true }
    }

    const fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for TemporaryOutput {
    fn drop(&mut self) {
        if self.armed {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;

    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(output, "{byte:02x}");
    }
    output
}

fn required_path(
    get: &mut impl FnMut(&str) -> Option<String>,
    key: &'static str,
) -> Result<PathBuf, CompanionAttestationProvisionerConfigError> {
    let value = get(key)
        .filter(|value| !value.is_empty())
        .ok_or(CompanionAttestationProvisionerConfigError::Missing { key })?;
    let path = PathBuf::from(&value);
    if value.len() > MAX_PATH_BYTES
        || value.ends_with('/')
        || value[1..]
            .split('/')
            .any(|component| component.is_empty() || matches!(component, "." | ".."))
        || validate_normalized_absolute_path(&path).is_err()
    {
        return Err(CompanionAttestationProvisionerConfigError::InvalidSetting { key });
    }
    Ok(path)
}

fn validate_normalized_absolute_path(path: &Path) -> Result<(), ()> {
    if !path.is_absolute() || path.file_name().is_none() {
        return Err(());
    }
    for component in path.components() {
        if matches!(
            component,
            Component::CurDir | Component::ParentDir | Component::Prefix(_)
        ) {
            return Err(());
        }
    }
    Ok(())
}

fn required_u32(
    get: &mut impl FnMut(&str) -> Option<String>,
    key: &'static str,
) -> Result<u32, CompanionAttestationProvisionerConfigError> {
    let value = get(key)
        .filter(|value| !value.is_empty())
        .ok_or(CompanionAttestationProvisionerConfigError::Missing { key })?
        .parse::<u32>()
        .map_err(|_| CompanionAttestationProvisionerConfigError::InvalidSetting { key })?;
    if value == u32::MAX {
        return Err(CompanionAttestationProvisionerConfigError::InvalidSetting { key });
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashMap,
        os::unix::fs::{MetadataExt, PermissionsExt, symlink},
        sync::atomic::{AtomicU64, Ordering},
    };

    use super::*;

    static TEST_ID: AtomicU64 = AtomicU64::new(1);
    const NOW_NS: u64 = 1_787_000_100_000_000_000;

    struct TestFiles {
        directory: PathBuf,
        metadata: PathBuf,
        secret: PathBuf,
        output: PathBuf,
        uid: u32,
        gid: u32,
    }

    impl TestFiles {
        fn new() -> Self {
            let id = TEST_ID.fetch_add(1, Ordering::Relaxed);
            let directory = PathBuf::from("/tmp").join(format!(
                "md-attestation-provisioner-{}-{id}",
                std::process::id()
            ));
            fs::create_dir(&directory).unwrap();
            fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
            let directory_metadata = fs::metadata(&directory).unwrap();
            let metadata = directory.join("engine-metadata.json");
            let secret = directory.join("digest-secret");
            fs::write(&secret, [0x51; 32]).unwrap();
            fs::set_permissions(&secret, fs::Permissions::from_mode(0o600)).unwrap();
            Self {
                output: directory.join("attestation.json"),
                directory,
                metadata,
                secret,
                uid: directory_metadata.uid(),
                gid: directory_metadata.gid(),
            }
        }

        fn config(&self) -> CompanionAttestationProvisionerConfig {
            CompanionAttestationProvisionerConfig {
                metadata_path: self.metadata.clone(),
                digest_secret_path: self.secret.clone(),
                attestation_path: self.output.clone(),
                owner_uid: self.uid,
                group_gid: self.gid,
                max_metadata_age: Duration::from_secs(30),
            }
        }

        fn write_metadata(&self, value: &serde_json::Value) {
            fs::write(&self.metadata, serde_json::to_vec(value).unwrap()).unwrap();
            fs::set_permissions(&self.metadata, fs::Permissions::from_mode(0o600)).unwrap();
        }
    }

    impl Drop for TestFiles {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.directory);
        }
    }

    fn timestamp(ns: u64) -> String {
        let seconds = i64::try_from(ns / 1_000_000_000).unwrap();
        let nanos = u32::try_from(ns % 1_000_000_000).unwrap();
        DateTime::from_timestamp(seconds, nanos)
            .unwrap()
            .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
    }

    fn metadata(started_ns: u64, captured_ns: u64) -> serde_json::Value {
        serde_json::json!({
            "schema_version": 1,
            "live": {
                "captured_utc": timestamp(captured_ns),
                "container": "dspark-0731",
                "configured_image": "example.invalid/engine:r34",
                "image_id": format!("sha256:{}", "1".repeat(64)),
                "image_descriptor_digest": format!("sha256:{}", "2".repeat(64)),
                "image_config_digest": format!("sha256:{}", "3".repeat(64)),
                "repo_digests": [format!("example.invalid/engine@sha256:{}", "2".repeat(64))],
                "model_revision": "model-revision",
                "tokenizer_revision": "tokenizer-revision",
                "tokenizer_sha256": "4".repeat(64),
                "config_sha256": "5".repeat(64),
                "driver": "595.84",
                "topology_sha256": "6".repeat(64),
                "started_at": timestamp(started_ns),
                "process_started_unix_ns": started_ns,
                "restart_count": 0,
                "cpuset_cpus": "0-11,24-35",
                "cpuset_mems": "0",
                "runtime_packages": {"torch": "3.0.0", "vllm": "0.13.1"},
                "argv_sha256": "7".repeat(64),
                "effective_contract": {"block_size": "256", "tensor_parallel_size": "4"}
            },
            "receipt": null,
            "verified": null
        })
    }

    fn load_output(files: &TestFiles) -> EngineIncarnation {
        let policy = SnapshotSecretFilePolicy {
            expected_owner_uid: files.uid,
        };
        let secret = load_companion_digest_secret(&files.secret, policy).unwrap();
        load_authenticated_engine_incarnation(&files.output, policy, &secret).unwrap()
    }

    #[test]
    fn creates_exact_atomic_output_and_is_idempotent() {
        let files = TestFiles::new();
        files.write_metadata(&metadata(NOW_NS - 10_000_000_000, NOW_NS - 1_000_000_000));
        assert_eq!(
            provision_authenticated_engine_incarnation_at(&files.config(), NOW_NS).unwrap(),
            CompanionAttestationProvisionOutcome::Created
        );
        let first_metadata = fs::metadata(&files.output).unwrap();
        assert_eq!(first_metadata.mode() & 0o777, OUTPUT_MODE);
        assert_eq!(first_metadata.uid(), files.uid);
        assert_eq!(first_metadata.gid(), files.gid);
        assert_eq!(first_metadata.nlink(), 1);
        let first_identity = load_output(&files);
        assert_eq!(first_identity.engine_id, "dspark-0731");
        assert_eq!(
            first_identity.process_started_unix_ns,
            NOW_NS - 10_000_000_000
        );
        assert_eq!(first_identity.attestation_sha256.len(), 32);
        assert_eq!(
            provision_authenticated_engine_incarnation_at(&files.config(), NOW_NS).unwrap(),
            CompanionAttestationProvisionOutcome::Unchanged
        );
        assert_eq!(
            FileIdentity::from_metadata(&fs::metadata(&files.output).unwrap()),
            FileIdentity::from_metadata(&first_metadata)
        );
    }

    #[test]
    fn stable_evidence_ignores_capture_time_and_repo_digest_order() {
        let files = TestFiles::new();
        let mut first = metadata(NOW_NS - 20_000_000_000, NOW_NS - 2_000_000_000);
        first["live"]["repo_digests"] = serde_json::json!([
            format!("z.invalid/engine@sha256:{}", "8".repeat(64)),
            format!("a.invalid/engine@sha256:{}", "9".repeat(64))
        ]);
        files.write_metadata(&first);
        provision_authenticated_engine_incarnation_at(&files.config(), NOW_NS).unwrap();
        let initial = load_output(&files);
        let mut refreshed = first;
        refreshed["live"]["captured_utc"] = timestamp(NOW_NS - 1_000_000_000).into();
        refreshed["live"]["repo_digests"]
            .as_array_mut()
            .unwrap()
            .reverse();
        files.write_metadata(&refreshed);
        assert_eq!(
            provision_authenticated_engine_incarnation_at(&files.config(), NOW_NS).unwrap(),
            CompanionAttestationProvisionOutcome::Unchanged
        );
        assert_eq!(load_output(&files), initial);
    }

    #[test]
    fn valid_new_process_updates_but_rollback_preserves_new_output() {
        let files = TestFiles::new();
        let old_started = NOW_NS - 20_000_000_000;
        files.write_metadata(&metadata(old_started, NOW_NS - 2_000_000_000));
        provision_authenticated_engine_incarnation_at(&files.config(), NOW_NS).unwrap();
        let new_started = NOW_NS - 5_000_000_000;
        files.write_metadata(&metadata(new_started, NOW_NS - 1_000_000_000));
        assert_eq!(
            provision_authenticated_engine_incarnation_at(&files.config(), NOW_NS).unwrap(),
            CompanionAttestationProvisionOutcome::Updated
        );
        let updated_bytes = fs::read(&files.output).unwrap();
        files.write_metadata(&metadata(old_started, NOW_NS - 500_000_000));
        assert_eq!(
            provision_authenticated_engine_incarnation_at(&files.config(), NOW_NS)
                .unwrap_err()
                .reason(),
            "identity_rollback"
        );
        assert_eq!(fs::read(&files.output).unwrap(), updated_bytes);
    }

    #[test]
    fn same_process_with_changed_contract_is_a_conflict() {
        let files = TestFiles::new();
        let started = NOW_NS - 10_000_000_000;
        let original = metadata(started, NOW_NS - 2_000_000_000);
        files.write_metadata(&original);
        provision_authenticated_engine_incarnation_at(&files.config(), NOW_NS).unwrap();
        let original_bytes = fs::read(&files.output).unwrap();
        let mut changed = original;
        changed["live"]["argv_sha256"] = "a".repeat(64).into();
        changed["live"]["captured_utc"] = timestamp(NOW_NS - 1_000_000_000).into();
        files.write_metadata(&changed);
        assert_eq!(
            provision_authenticated_engine_incarnation_at(&files.config(), NOW_NS)
                .unwrap_err()
                .reason(),
            "identity_conflict"
        );
        assert_eq!(fs::read(&files.output).unwrap(), original_bytes);
    }

    #[test]
    fn rejects_stale_future_and_pre_start_captures() {
        let files = TestFiles::new();
        files.write_metadata(&metadata(NOW_NS - 50_000_000_000, NOW_NS - 31_000_000_000));
        assert_eq!(
            provision_authenticated_engine_incarnation_at(&files.config(), NOW_NS)
                .unwrap_err()
                .reason(),
            "stale_metadata"
        );
        files.write_metadata(&metadata(NOW_NS, NOW_NS + FUTURE_SKEW_NS + 1));
        assert_eq!(
            provision_authenticated_engine_incarnation_at(&files.config(), NOW_NS)
                .unwrap_err()
                .reason(),
            "future_metadata"
        );
        files.write_metadata(&metadata(NOW_NS - 1_000_000_000, NOW_NS - 2_000_000_000));
        assert_eq!(
            provision_authenticated_engine_incarnation_at(&files.config(), NOW_NS)
                .unwrap_err()
                .reason(),
            "invalid_metadata"
        );
        assert!(!files.output.exists());
    }

    #[test]
    fn receipt_must_be_verified_and_qualified() {
        let files = TestFiles::new();
        let mut input = metadata(NOW_NS - 10_000_000_000, NOW_NS - 1_000_000_000);
        let verified = input.as_object_mut().unwrap().remove("verified").unwrap();
        files.write_metadata(&input);
        assert_eq!(
            provision_authenticated_engine_incarnation_at(&files.config(), NOW_NS)
                .unwrap_err()
                .reason(),
            "malformed_metadata"
        );
        input["verified"] = verified;
        input["receipt"] = serde_json::json!({
            "receipt_sha256": "a".repeat(64),
            "status": "qualified"
        });
        input["verified"] = false.into();
        files.write_metadata(&input);
        assert_eq!(
            provision_authenticated_engine_incarnation_at(&files.config(), NOW_NS)
                .unwrap_err()
                .reason(),
            "unverified_metadata"
        );
        input["verified"] = true.into();
        files.write_metadata(&input);
        assert!(provision_authenticated_engine_incarnation_at(&files.config(), NOW_NS).is_ok());
    }

    #[test]
    fn rejects_unsafe_inputs_and_destination_without_replacement() {
        let files = TestFiles::new();
        let input = metadata(NOW_NS - 10_000_000_000, NOW_NS - 1_000_000_000);
        files.write_metadata(&input);
        fs::set_permissions(&files.metadata, fs::Permissions::from_mode(0o640)).unwrap();
        assert_eq!(
            provision_authenticated_engine_incarnation_at(&files.config(), NOW_NS)
                .unwrap_err()
                .reason(),
            "unsafe_permissions"
        );
        fs::set_permissions(&files.metadata, fs::Permissions::from_mode(0o600)).unwrap();
        let linked = files.directory.join("linked-output");
        symlink(&files.output, &linked).unwrap();
        let mut config = files.config();
        config.attestation_path = linked;
        assert_eq!(
            provision_authenticated_engine_incarnation_at(&config, NOW_NS)
                .unwrap_err()
                .reason(),
            "unsafe_destination"
        );
        assert!(!files.output.exists());
    }

    #[test]
    fn rejects_unknown_identity_fields_and_concurrent_publication() {
        let files = TestFiles::new();
        let mut input = metadata(NOW_NS - 10_000_000_000, NOW_NS - 1_000_000_000);
        input["live"]["unbound_future_identity"] = "value".into();
        files.write_metadata(&input);
        assert_eq!(
            provision_authenticated_engine_incarnation_at(&files.config(), NOW_NS)
                .unwrap_err()
                .reason(),
            "malformed_metadata"
        );
        input["live"]
            .as_object_mut()
            .unwrap()
            .remove("unbound_future_identity");
        files.write_metadata(&input);
        let directory = File::open(&files.directory).unwrap();
        directory.try_lock().unwrap();
        assert_eq!(
            provision_authenticated_engine_incarnation_at(&files.config(), NOW_NS)
                .unwrap_err()
                .reason(),
            "unsafe_destination"
        );
        assert!(!files.output.exists());
    }

    #[test]
    fn tampered_existing_output_fails_closed() {
        let files = TestFiles::new();
        files.write_metadata(&metadata(NOW_NS - 10_000_000_000, NOW_NS - 1_000_000_000));
        provision_authenticated_engine_incarnation_at(&files.config(), NOW_NS).unwrap();
        let mut tampered = fs::read(&files.output).unwrap();
        tampered[0] ^= 1;
        fs::set_permissions(&files.output, fs::Permissions::from_mode(0o600)).unwrap();
        fs::write(&files.output, &tampered).unwrap();
        fs::set_permissions(&files.output, fs::Permissions::from_mode(OUTPUT_MODE)).unwrap();
        assert_eq!(
            provision_authenticated_engine_incarnation_at(&files.config(), NOW_NS)
                .unwrap_err()
                .reason(),
            "invalid_existing_output"
        );
        assert_eq!(fs::read(&files.output).unwrap(), tampered);
    }

    #[test]
    fn config_is_environment_only_bounded_and_redacted() {
        let files = TestFiles::new();
        let mut values = HashMap::from([
            (
                "RJ_SNAPSHOT_ENGINE_METADATA_PATH",
                files.metadata.to_string_lossy().into_owned(),
            ),
            (
                "RJ_SNAPSHOT_DIGEST_SECRET_PATH",
                files.secret.to_string_lossy().into_owned(),
            ),
            (
                "RJ_SNAPSHOT_ATTESTATION_PATH",
                files.output.to_string_lossy().into_owned(),
            ),
            ("RJ_SNAPSHOT_SECRET_OWNER_UID", files.uid.to_string()),
            ("RJ_SNAPSHOT_SECRET_GROUP_GID", files.gid.to_string()),
            ("RJ_SNAPSHOT_ATTESTATION_MAX_AGE_MS", "25000".to_owned()),
        ]);
        let config =
            CompanionAttestationProvisionerConfig::from_lookup(|key| values.get(key).cloned())
                .unwrap();
        let debug = format!("{config:?}");
        assert!(!debug.contains(files.directory.to_str().unwrap()));
        assert!(debug.contains("[REDACTED]"));
        values.insert(
            "RJ_SNAPSHOT_ATTESTATION_PATH",
            files.metadata.to_string_lossy().into_owned(),
        );
        assert_eq!(
            CompanionAttestationProvisionerConfig::from_lookup(|key| { values.get(key).cloned() })
                .unwrap_err(),
            CompanionAttestationProvisionerConfigError::AliasedPaths
        );
    }
}
