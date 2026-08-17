//! Authenticated, refreshable engine-incarnation input for the companion.
//!
//! The manifest is a bounded JSON envelope. Its HMAC covers a canonical
//! `MessagePack` encoding under a dedicated domain and uses the digest secret as
//! key material. Filesystem protection is necessary but not sufficient: a
//! syntactically valid manifest without the correct MAC never grants authority.

use std::{path::Path, time::Duration};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::sync::watch;

use crate::{
    kv_snapshot::EngineIncarnation,
    snapshot_secret_file::{
        SnapshotSecretFileError, SnapshotSecretFilePolicy, load_snapshot_control_file,
    },
};

const SECRET_BYTES: usize = 32;
const HMAC_BLOCK_BYTES: usize = 64;
const MAX_ATTESTATION_BYTES: usize = 16 * 1024;
const MAX_COMPONENT_BYTES: usize = 512;
const SCHEMA_VERSION: u16 = 1;
const DOMAIN: &[u8] = b"mini-dynamo:engine-incarnation-attestation:v1\0";

/// Directly owned digest key loaded from hardened storage.
pub struct CompanionDigestSecret([u8; SECRET_BYTES]);

impl CompanionDigestSecret {
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; SECRET_BYTES] {
        &self.0
    }
}

impl std::fmt::Debug for CompanionDigestSecret {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("CompanionDigestSecret([REDACTED])")
    }
}

impl Drop for CompanionDigestSecret {
    fn drop(&mut self) {
        self.0.fill(0);
    }
}

/// Redacted monotonic authority observed by an LB reconnect owner.
///
/// The revision changes for every valid/unavailable transition. A watch
/// receiver can therefore detect a coalesced `valid -> invalid -> same valid`
/// sequence and still revoke the session that crossed the authority gap.
#[derive(Clone, Eq, PartialEq)]
pub struct EngineIncarnationAuthority {
    revision: u64,
    incarnation: Option<EngineIncarnation>,
}

impl EngineIncarnationAuthority {
    #[must_use]
    pub const fn new(revision: u64, incarnation: Option<EngineIncarnation>) -> Self {
        Self {
            revision,
            incarnation,
        }
    }

    #[must_use]
    pub const fn revision(&self) -> u64 {
        self.revision
    }

    #[must_use]
    pub const fn incarnation(&self) -> Option<&EngineIncarnation> {
        self.incarnation.as_ref()
    }
}

impl std::fmt::Debug for EngineIncarnationAuthority {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("EngineIncarnationAuthority")
            .field("revision", &self.revision)
            .field("available", &self.incarnation.is_some())
            .finish()
    }
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum CompanionAttestationError {
    #[error("companion protected file validation failed")]
    File(#[from] SnapshotSecretFileError),
    #[error("companion digest secret has an invalid length")]
    SecretLength,
    #[error("engine incarnation attestation is malformed")]
    Malformed,
    #[error("engine incarnation attestation is unsupported")]
    Unsupported,
    #[error("engine incarnation attestation has invalid fields")]
    InvalidIncarnation,
    #[error("engine incarnation attestation authentication failed")]
    Authentication,
}

impl CompanionAttestationError {
    #[must_use]
    pub const fn reason(&self) -> &'static str {
        match self {
            Self::File(error) => error.reason(),
            Self::SecretLength => "secret_length",
            Self::Malformed => "malformed",
            Self::Unsupported => "unsupported",
            Self::InvalidIncarnation => "invalid_incarnation",
            Self::Authentication => "authentication_failed",
        }
    }
}

#[derive(Serialize)]
struct AttestationPayload<'a> {
    schema_version: u16,
    engine_incarnation: &'a EngineIncarnation,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct AttestationEnvelope {
    schema_version: u16,
    engine_incarnation: EngineIncarnation,
    mac_sha256: Vec<u8>,
}

/// Load an exact raw 256-bit digest secret from hardened storage.
///
/// # Errors
///
/// Rejects every file-policy failure and every size other than 32 raw bytes.
pub fn load_companion_digest_secret(
    path: &Path,
    policy: SnapshotSecretFilePolicy,
) -> Result<CompanionDigestSecret, CompanionAttestationError> {
    let mut raw = load_snapshot_control_file(path, policy, SECRET_BYTES)?;
    let converted = <[u8; SECRET_BYTES]>::try_from(raw.as_slice());
    raw.fill(0);
    converted
        .map(CompanionDigestSecret)
        .map_err(|_| CompanionAttestationError::SecretLength)
}

/// Load and authenticate one incarnation manifest.
///
/// # Errors
///
/// Fails closed for filesystem, size, JSON, schema, field, or MAC violations.
pub fn load_authenticated_engine_incarnation(
    path: &Path,
    policy: SnapshotSecretFilePolicy,
    secret: &CompanionDigestSecret,
) -> Result<EngineIncarnation, CompanionAttestationError> {
    let mut bytes = load_snapshot_control_file(path, policy, MAX_ATTESTATION_BYTES)?;
    let decoded = serde_json::from_slice::<AttestationEnvelope>(&bytes)
        .map_err(|_| CompanionAttestationError::Malformed);
    bytes.fill(0);
    let envelope = decoded?;
    if envelope.schema_version != SCHEMA_VERSION {
        return Err(CompanionAttestationError::Unsupported);
    }
    validate_incarnation(&envelope.engine_incarnation)?;
    if envelope.mac_sha256.len() != SECRET_BYTES {
        return Err(CompanionAttestationError::Authentication);
    }
    let expected = authenticate(&envelope.engine_incarnation, secret)?;
    if !constant_time_equal(&expected, &envelope.mac_sha256) {
        return Err(CompanionAttestationError::Authentication);
    }
    Ok(envelope.engine_incarnation)
}

/// Encode the canonical authenticated JSON envelope used by the file watcher.
/// This helper makes provisioning independent of private serialization details.
///
/// # Errors
///
/// Rejects invalid incarnation fields or serialization failure.
pub fn encode_authenticated_engine_incarnation(
    incarnation: &EngineIncarnation,
    secret: &CompanionDigestSecret,
) -> Result<Vec<u8>, CompanionAttestationError> {
    validate_incarnation(incarnation)?;
    let envelope = AttestationEnvelope {
        schema_version: SCHEMA_VERSION,
        engine_incarnation: incarnation.clone(),
        mac_sha256: authenticate(incarnation, secret)?.to_vec(),
    };
    serde_json::to_vec(&envelope).map_err(|_| CompanionAttestationError::Malformed)
}

/// Poll an authenticated manifest until shutdown. Every invalid refresh sends
/// explicit authority loss; a later valid manifest restores authority.
pub async fn watch_authenticated_engine_incarnation(
    path: &Path,
    policy: SnapshotSecretFilePolicy,
    secret: CompanionDigestSecret,
    refresh: Duration,
    authority: &watch::Sender<Option<EngineIncarnation>>,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut ticker = tokio::time::interval(refresh);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut last_failure = None;
    loop {
        tokio::select! {
            biased;
            () = wait_for_shutdown(&mut shutdown) => return,
            _ = ticker.tick() => {
                let refreshed = load_authenticated_engine_incarnation(path, policy, &secret);
                let next = match refreshed {
                    Ok(incarnation) => {
                        last_failure = None;
                        Some(incarnation)
                    }
                    Err(error) => {
                        let reason = error.reason();
                        if last_failure != Some(reason) {
                            tracing::warn!(reason, "engine incarnation authority unavailable");
                            last_failure = Some(reason);
                        }
                        None
                    }
                };
                authority.send_if_modified(|current| {
                    if *current == next {
                        false
                    } else {
                        *current = next;
                        true
                    }
                });
            }
        }
    }
}

/// Poll an authenticated manifest for an LB reconnect owner. Unlike the
/// compatibility watcher above, this channel carries a monotonic revision so
/// intermediate authority loss cannot be hidden by watch-value coalescing.
pub async fn watch_authenticated_engine_incarnation_authority(
    path: &Path,
    policy: SnapshotSecretFilePolicy,
    secret: CompanionDigestSecret,
    refresh: Duration,
    authority: &watch::Sender<EngineIncarnationAuthority>,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut ticker = tokio::time::interval(refresh);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut last_failure = None;
    loop {
        tokio::select! {
            biased;
            () = wait_for_shutdown(&mut shutdown) => return,
            _ = ticker.tick() => {
                let refreshed = load_authenticated_engine_incarnation(path, policy, &secret);
                let next = match refreshed {
                    Ok(incarnation) => {
                        last_failure = None;
                        Some(incarnation)
                    }
                    Err(error) => {
                        let reason = error.reason();
                        if last_failure != Some(reason) {
                            tracing::warn!(reason, "engine incarnation authority unavailable");
                            last_failure = Some(reason);
                        }
                        None
                    }
                };
                authority.send_if_modified(|current| {
                    if current.incarnation == next {
                        false
                    } else {
                        current.revision = current.revision.saturating_add(1);
                        current.incarnation = next;
                        true
                    }
                });
            }
        }
    }
}

fn validate_incarnation(incarnation: &EngineIncarnation) -> Result<(), CompanionAttestationError> {
    let components = [
        incarnation.engine_id.as_bytes(),
        incarnation.model_revision.as_bytes(),
        incarnation.image_digest.as_bytes(),
    ];
    if components
        .iter()
        .any(|component| component.is_empty() || component.len() > MAX_COMPONENT_BYTES)
        || incarnation.process_started_unix_ns == 0
        || incarnation.attestation_sha256.len() != SECRET_BYTES
    {
        return Err(CompanionAttestationError::InvalidIncarnation);
    }
    Ok(())
}

fn authenticate(
    incarnation: &EngineIncarnation,
    secret: &CompanionDigestSecret,
) -> Result<[u8; SECRET_BYTES], CompanionAttestationError> {
    let payload = rmp_serde::to_vec_named(&AttestationPayload {
        schema_version: SCHEMA_VERSION,
        engine_incarnation: incarnation,
    })
    .map_err(|_| CompanionAttestationError::Malformed)?;
    let mut hmac = HmacSha256::new(secret.as_bytes());
    hmac.update(DOMAIN);
    hmac.update(&payload);
    Ok(hmac.finalize())
}

fn constant_time_equal(expected: &[u8; SECRET_BYTES], actual: &[u8]) -> bool {
    if actual.len() != SECRET_BYTES {
        return false;
    }
    expected
        .iter()
        .zip(actual)
        .fold(0_u8, |difference, (left, right)| {
            difference | (left ^ right)
        })
        == 0
}

struct HmacSha256 {
    inner: Sha256,
    outer_pad: [u8; HMAC_BLOCK_BYTES],
}

impl HmacSha256 {
    fn new(key: &[u8]) -> Self {
        let mut normalized = [0_u8; HMAC_BLOCK_BYTES];
        normalized[..key.len()].copy_from_slice(key);
        let mut inner_pad = [0x36_u8; HMAC_BLOCK_BYTES];
        let mut outer_pad = [0x5c_u8; HMAC_BLOCK_BYTES];
        for ((inner, outer), key_byte) in inner_pad.iter_mut().zip(&mut outer_pad).zip(normalized) {
            *inner ^= key_byte;
            *outer ^= key_byte;
        }
        let mut inner = Sha256::new();
        inner.update(inner_pad);
        normalized.fill(0);
        inner_pad.fill(0);
        Self { inner, outer_pad }
    }

    fn update(&mut self, bytes: &[u8]) {
        self.inner.update(bytes);
    }

    fn finalize(mut self) -> [u8; SECRET_BYTES] {
        let inner = self.inner.finalize();
        let mut outer = Sha256::new();
        outer.update(self.outer_pad);
        outer.update(inner);
        self.outer_pad.fill(0);
        outer.finalize().into()
    }
}

async fn wait_for_shutdown(shutdown: &mut watch::Receiver<bool>) {
    loop {
        if *shutdown.borrow() || shutdown.changed().await.is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        os::unix::fs::{MetadataExt, PermissionsExt},
        sync::atomic::{AtomicU64, Ordering},
    };

    use tokio::time::timeout;

    use super::*;

    static TEST_ID: AtomicU64 = AtomicU64::new(1);

    struct TestFiles {
        directory: std::path::PathBuf,
        secret: std::path::PathBuf,
        attestation: std::path::PathBuf,
        owner: u32,
    }

    impl TestFiles {
        fn new() -> Self {
            let id = TEST_ID.fetch_add(1, Ordering::Relaxed);
            let directory = std::env::temp_dir()
                .join(format!("ramjet-attestation-{}-{id}", std::process::id()));
            fs::create_dir(&directory).unwrap();
            fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
            let owner = fs::metadata(&directory).unwrap().uid();
            let secret = directory.join("digest-secret");
            fs::write(&secret, [0x51; SECRET_BYTES]).unwrap();
            fs::set_permissions(&secret, fs::Permissions::from_mode(0o600)).unwrap();
            Self {
                attestation: directory.join("attestation.json"),
                directory,
                secret,
                owner,
            }
        }

        fn policy(&self) -> SnapshotSecretFilePolicy {
            SnapshotSecretFilePolicy {
                expected_owner_uid: self.owner,
            }
        }

        fn write_attestation(&self, bytes: &[u8]) {
            fs::write(&self.attestation, bytes).unwrap();
            fs::set_permissions(&self.attestation, fs::Permissions::from_mode(0o600)).unwrap();
        }

        fn atomically_replace_attestation(&self, bytes: &[u8]) {
            let replacement = self.directory.join("attestation.next");
            fs::write(&replacement, bytes).unwrap();
            fs::set_permissions(&replacement, fs::Permissions::from_mode(0o600)).unwrap();
            fs::rename(replacement, &self.attestation).unwrap();
        }
    }

    impl Drop for TestFiles {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.directory);
        }
    }

    fn incarnation(started: u64) -> EngineIncarnation {
        EngineIncarnation {
            engine_id: "engine-a".to_owned(),
            model_revision: "revision".to_owned(),
            image_digest: "sha256:image".to_owned(),
            process_started_unix_ns: started,
            attestation_sha256: vec![7; SECRET_BYTES],
        }
    }

    #[test]
    fn hardened_authenticated_manifest_round_trips_and_rejects_tamper() {
        let files = TestFiles::new();
        let secret = load_companion_digest_secret(&files.secret, files.policy()).unwrap();
        assert_eq!(format!("{secret:?}"), "CompanionDigestSecret([REDACTED])");
        let authority = EngineIncarnationAuthority::new(7, Some(incarnation(42)));
        let authority_debug = format!("{authority:?}");
        assert_eq!(
            authority_debug,
            "EngineIncarnationAuthority { revision: 7, available: true }"
        );
        assert!(!authority_debug.contains("engine-a"));
        assert!(!authority_debug.contains("sha256:image"));
        let encoded = encode_authenticated_engine_incarnation(&incarnation(42), &secret).unwrap();
        files.write_attestation(&encoded);
        assert_eq!(
            load_authenticated_engine_incarnation(&files.attestation, files.policy(), &secret)
                .unwrap(),
            incarnation(42)
        );

        let mut envelope: serde_json::Value = serde_json::from_slice(&encoded).unwrap();
        envelope["engine_incarnation"]["process_started_unix_ns"] = 43.into();
        files.write_attestation(&serde_json::to_vec(&envelope).unwrap());
        assert_eq!(
            load_authenticated_engine_incarnation(&files.attestation, files.policy(), &secret),
            Err(CompanionAttestationError::Authentication)
        );
    }

    #[test]
    fn protected_inputs_reject_unsafe_permissions_and_secret_sizes() {
        let files = TestFiles::new();
        fs::set_permissions(&files.secret, fs::Permissions::from_mode(0o622)).unwrap();
        assert!(matches!(
            load_companion_digest_secret(&files.secret, files.policy()),
            Err(CompanionAttestationError::File(
                SnapshotSecretFileError::UnsafePermissions
            ))
        ));
        fs::set_permissions(&files.secret, fs::Permissions::from_mode(0o600)).unwrap();
        fs::write(&files.secret, [0x51; SECRET_BYTES - 1]).unwrap();
        assert!(load_companion_digest_secret(&files.secret, files.policy()).is_err());
    }

    #[tokio::test]
    async fn watcher_does_not_churn_same_identity_and_accepts_atomic_rotation() {
        let files = TestFiles::new();
        let secret = load_companion_digest_secret(&files.secret, files.policy()).unwrap();
        let good = encode_authenticated_engine_incarnation(&incarnation(42), &secret).unwrap();
        files.write_attestation(&good);
        let (authority_tx, mut authority_rx) =
            watch::channel(EngineIncarnationAuthority::new(1, Some(incarnation(42))));
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let path = files.attestation.clone();
        let policy = files.policy();
        let task = tokio::spawn(async move {
            watch_authenticated_engine_incarnation_authority(
                &path,
                policy,
                secret,
                Duration::from_millis(10),
                &authority_tx,
                shutdown_rx,
            )
            .await;
        });

        files.atomically_replace_attestation(&good);
        tokio::time::sleep(Duration::from_millis(35)).await;
        assert!(!authority_rx.has_changed().unwrap());

        let signing_secret = load_companion_digest_secret(&files.secret, files.policy()).unwrap();
        let rotated =
            encode_authenticated_engine_incarnation(&incarnation(43), &signing_secret).unwrap();
        files.atomically_replace_attestation(&rotated);
        timeout(Duration::from_secs(1), async {
            loop {
                authority_rx.changed().await.unwrap();
                let current = authority_rx.borrow_and_update();
                if current.revision() == 2 && current.incarnation() == Some(&incarnation(43)) {
                    break;
                }
            }
        })
        .await
        .unwrap();

        shutdown_tx.send(true).unwrap();
        task.await.unwrap();
    }

    #[tokio::test]
    async fn watcher_fences_unsafe_and_malformed_refreshes_then_recovers() {
        let files = TestFiles::new();
        let secret = load_companion_digest_secret(&files.secret, files.policy()).unwrap();
        let good = encode_authenticated_engine_incarnation(&incarnation(42), &secret).unwrap();
        files.write_attestation(&good);
        let (authority_tx, mut authority_rx) =
            watch::channel(EngineIncarnationAuthority::new(1, Some(incarnation(42))));
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let path = files.attestation.clone();
        let policy = files.policy();
        let task = tokio::spawn(async move {
            watch_authenticated_engine_incarnation_authority(
                &path,
                policy,
                secret,
                Duration::from_millis(10),
                &authority_tx,
                shutdown_rx,
            )
            .await;
        });

        fs::set_permissions(&files.attestation, fs::Permissions::from_mode(0o666)).unwrap();
        wait_for_authority(&mut authority_rx, 2, None).await;

        files.atomically_replace_attestation(&good);
        wait_for_authority(&mut authority_rx, 3, Some(&incarnation(42))).await;

        files.atomically_replace_attestation(b"not authenticated");
        wait_for_authority(&mut authority_rx, 4, None).await;

        let signing_secret = load_companion_digest_secret(&files.secret, files.policy()).unwrap();
        let refreshed =
            encode_authenticated_engine_incarnation(&incarnation(43), &signing_secret).unwrap();
        files.atomically_replace_attestation(&refreshed);
        timeout(Duration::from_secs(1), async {
            loop {
                authority_rx.changed().await.unwrap();
                let current = authority_rx.borrow_and_update();
                if current.revision() == 5 && current.incarnation() == Some(&incarnation(43)) {
                    break;
                }
            }
        })
        .await
        .unwrap();
        shutdown_tx.send(true).unwrap();
        task.await.unwrap();
    }

    async fn wait_for_authority(
        authority: &mut watch::Receiver<EngineIncarnationAuthority>,
        revision: u64,
        expected: Option<&EngineIncarnation>,
    ) {
        timeout(Duration::from_secs(1), async {
            loop {
                authority.changed().await.unwrap();
                let current = authority.borrow_and_update();
                if current.revision() == revision && current.incarnation() == expected {
                    break;
                }
            }
        })
        .await
        .unwrap();
    }
}
