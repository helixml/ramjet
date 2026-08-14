//! Versioned compatibility evidence for local request tokenization.
//!
//! A manifest binds the local renderer/tokenizer to the runtime identity of the
//! engines and to synthetic token-vector goldens. Exact token IDs may be used
//! for shadow scoring only while every layer matches.

use std::{
    collections::{BTreeMap, HashSet},
    path::Path,
};

use anyhow::Context;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

const MAX_MANIFEST_BYTES: u64 = 1 << 20;
const MAX_GOLDENS: usize = 64;
const MAX_ADMITTED_CLASSES: usize = 32;
const MAX_INCARNATION_BYTES: usize = 256;
const MAX_KV_EVENT_CAPACITY: usize = 1_000_000_000;
const MAX_RUNTIME_ARGUMENTS: usize = 256;
const MAX_RUNTIME_ARGUMENT_BYTES: usize = 4096;
const MAX_RUNTIME_ARGUMENT_TOTAL_BYTES: usize = 64 << 10;
const MAX_RUNTIME_ENVIRONMENT: usize = 128;
const MAX_RUNTIME_PACKAGES: usize = 64;
const MAX_RUNTIME_ARTIFACTS: usize = 16;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompatibilityManifest {
    pub schema_version: u32,
    pub model: ModelIdentity,
    pub engine: EngineIdentity,
    pub tokenizer: TokenizerIdentity,
    pub renderer: RendererIdentity,
    pub admitted_request_classes: Vec<String>,
    pub goldens: Vec<GoldenCase>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelIdentity {
    pub id: String,
    pub root: String,
    pub max_model_len: usize,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EngineIdentity {
    pub version: String,
    pub image_digest: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct KvEventsIdentity {
    pub enable_kv_cache_events: bool,
    pub publisher: String,
    pub endpoint: String,
    pub replay_endpoint: String,
    pub buffer_steps: usize,
    pub hwm: usize,
    pub max_queue_size: usize,
    pub topic: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServingRuntimeManifest {
    pub schema_version: u32,
    pub compatibility_manifest_sha256: String,
    pub engine: ServingRuntimeEngine,
    pub process: ServingRuntimeProcess,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServingRuntimeEngine {
    pub core_process_count: usize,
    pub kv_events: KvEventsIdentity,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServingRuntimeProcess {
    pub argv: Vec<String>,
    pub argv_sha256: String,
    pub environment: BTreeMap<String, String>,
    pub environment_sha256: String,
    pub packages: BTreeMap<String, String>,
    pub packages_sha256: String,
    pub artifacts: Vec<ServingRuntimeArtifact>,
    pub artifacts_sha256: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ServingRuntimeArtifact {
    pub path: String,
    pub sha256: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TokenizerIdentity {
    pub sha256: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RendererIdentity {
    pub profile: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GoldenCase {
    pub name: String,
    pub endpoint: String,
    pub request: Value,
    pub token_count: usize,
    pub token_ids_sha256: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeOutcome {
    Match,
    ModelsDecode,
    ModelMissing,
    ModelAmbiguous,
    ModelRootMismatch,
    ModelContextMismatch,
    VersionDecode,
    VersionMismatch,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ServingIdentityOutcome {
    Match,
    Decode,
    SchemaMismatch,
    FrontendIncarnationInvalid,
    CoreIncarnationInvalid,
    CoreProcessMismatch,
    ModelMismatch,
    EngineMismatch,
    KvEventsMismatch,
    LaunchMismatch,
    EnvironmentMismatch,
    PackageMismatch,
    ArtifactMismatch,
    TokenizerMismatch,
    RendererMismatch,
}

impl ServingIdentityOutcome {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Match => "match",
            Self::Decode => "decode",
            Self::SchemaMismatch => "schema_mismatch",
            Self::FrontendIncarnationInvalid => "frontend_incarnation_invalid",
            Self::CoreIncarnationInvalid => "core_incarnation_invalid",
            Self::CoreProcessMismatch => "core_process_mismatch",
            Self::ModelMismatch => "model_mismatch",
            Self::EngineMismatch => "engine_mismatch",
            Self::KvEventsMismatch => "kv_events_mismatch",
            Self::LaunchMismatch => "launch_mismatch",
            Self::EnvironmentMismatch => "environment_mismatch",
            Self::PackageMismatch => "package_mismatch",
            Self::ArtifactMismatch => "artifact_mismatch",
            Self::TokenizerMismatch => "tokenizer_mismatch",
            Self::RendererMismatch => "renderer_mismatch",
        }
    }
}

impl RuntimeOutcome {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Match => "match",
            Self::ModelsDecode => "models_decode",
            Self::ModelMissing => "model_missing",
            Self::ModelAmbiguous => "model_ambiguous",
            Self::ModelRootMismatch => "model_root_mismatch",
            Self::ModelContextMismatch => "model_context_mismatch",
            Self::VersionDecode => "version_decode",
            Self::VersionMismatch => "version_mismatch",
        }
    }
}

#[derive(Debug, Deserialize)]
struct ModelsResponse {
    data: Vec<RuntimeModel>,
}

#[derive(Debug, Deserialize)]
struct RuntimeModel {
    id: String,
    root: String,
    max_model_len: usize,
}

#[derive(Debug, Deserialize)]
struct VersionResponse {
    version: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ServingIdentityResponse {
    schema_version: u32,
    incarnation: ServingIncarnation,
    model: ModelIdentity,
    engine: ServingEngineIdentity,
    tokenizer: TokenizerIdentity,
    renderer: RendererIdentity,
    runtime: ServingRuntimeEvidence,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ServingEngineIdentity {
    version: String,
    image_digest: String,
    core_process_count: usize,
    kv_events: KvEventsIdentity,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ServingIncarnation {
    frontend: String,
    engine_core: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ServingRuntimeEvidence {
    #[serde(rename = "argv_sha256")]
    argv: String,
    #[serde(rename = "environment_sha256")]
    environment: String,
    #[serde(rename = "packages_sha256")]
    packages: String,
    #[serde(rename = "artifacts_sha256")]
    artifacts: String,
}

impl CompatibilityManifest {
    /// Load and validate a compatibility manifest and its operator pin.
    ///
    /// # Errors
    ///
    /// Fails closed for an unreadable, oversized, unpinned, malformed, or
    /// internally inconsistent manifest.
    pub fn load(
        path: &Path,
        expected_manifest_sha256: &str,
        expected_tokenizer_sha256: &str,
        expected_renderer_profile: &str,
    ) -> anyhow::Result<Self> {
        let metadata = std::fs::metadata(path)
            .with_context(|| format!("stat compatibility manifest {}", path.display()))?;
        anyhow::ensure!(
            metadata.len() <= MAX_MANIFEST_BYTES,
            "compatibility manifest exceeds 1 MiB"
        );
        let bytes = std::fs::read(path)
            .with_context(|| format!("read compatibility manifest {}", path.display()))?;
        anyhow::ensure!(
            sha256_hex(&bytes) == expected_manifest_sha256,
            "compatibility manifest SHA-256 mismatch"
        );
        let manifest: Self =
            serde_json::from_slice(&bytes).context("decode versioned compatibility manifest")?;
        manifest.validate(expected_tokenizer_sha256, expected_renderer_profile)?;
        Ok(manifest)
    }

    fn validate(
        &self,
        expected_tokenizer_sha256: &str,
        expected_renderer_profile: &str,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(self.schema_version == 1, "unsupported manifest schema");
        anyhow::ensure!(!self.model.id.is_empty(), "manifest model id is empty");
        anyhow::ensure!(!self.model.root.is_empty(), "manifest model root is empty");
        anyhow::ensure!(
            self.model.max_model_len > 0,
            "manifest model context is zero"
        );
        anyhow::ensure!(
            !self.engine.version.is_empty(),
            "manifest engine version is empty"
        );
        anyhow::ensure!(
            self.engine.image_digest.starts_with("sha256:")
                && valid_sha256(&self.engine.image_digest[7..]),
            "manifest engine image digest is invalid"
        );
        anyhow::ensure!(
            self.tokenizer.sha256 == expected_tokenizer_sha256,
            "manifest tokenizer SHA-256 does not match configured artifact"
        );
        anyhow::ensure!(
            self.renderer.profile == expected_renderer_profile,
            "manifest renderer profile does not match configured profile"
        );
        anyhow::ensure!(
            !self.admitted_request_classes.is_empty()
                && self.admitted_request_classes.len() <= MAX_ADMITTED_CLASSES,
            "manifest admitted request classes are empty or oversized"
        );
        anyhow::ensure!(
            !self.goldens.is_empty() && self.goldens.len() <= MAX_GOLDENS,
            "manifest goldens are empty or oversized"
        );
        let admitted = unique_nonempty(&self.admitted_request_classes, "request class")?;
        let names = self
            .goldens
            .iter()
            .map(|golden| golden.name.clone())
            .collect::<Vec<_>>();
        let golden_names = unique_nonempty(&names, "golden name")?;
        anyhow::ensure!(
            admitted.is_subset(&golden_names),
            "every admitted request class needs a same-named golden"
        );
        for golden in &self.goldens {
            anyhow::ensure!(golden.endpoint == "chat", "unsupported golden endpoint");
            anyhow::ensure!(
                golden.request.is_object(),
                "golden request is not an object"
            );
            anyhow::ensure!(
                golden.token_count > 0 && golden.token_count <= self.model.max_model_len,
                "golden token count is invalid"
            );
            anyhow::ensure!(
                valid_sha256(&golden.token_ids_sha256),
                "golden token-ID digest is invalid"
            );
        }
        Ok(())
    }

    #[must_use]
    pub fn admitted(&self, class: &str) -> bool {
        self.admitted_request_classes
            .iter()
            .any(|candidate| candidate == class)
    }

    #[must_use]
    pub fn runtime_outcome(&self, models: &[u8], version: &[u8]) -> RuntimeOutcome {
        let Ok(models) = serde_json::from_slice::<ModelsResponse>(models) else {
            return RuntimeOutcome::ModelsDecode;
        };
        let matches = models
            .data
            .iter()
            .filter(|model| model.id == self.model.id)
            .collect::<Vec<_>>();
        let Some(model) = matches.first() else {
            return RuntimeOutcome::ModelMissing;
        };
        if matches.len() != 1 {
            return RuntimeOutcome::ModelAmbiguous;
        }
        if model.root != self.model.root {
            return RuntimeOutcome::ModelRootMismatch;
        }
        if model.max_model_len != self.model.max_model_len {
            return RuntimeOutcome::ModelContextMismatch;
        }
        let Ok(version) = serde_json::from_slice::<VersionResponse>(version) else {
            return RuntimeOutcome::VersionDecode;
        };
        if version.version != self.engine.version {
            return RuntimeOutcome::VersionMismatch;
        }
        RuntimeOutcome::Match
    }

    /// Validate one atomically captured engine serving identity.
    ///
    /// The endpoint response binds model, engine image, tokenizer, renderer,
    /// and a process incarnation in one bounded JSON document. The opaque
    /// incarnation is validated but never logged or labeled. Callers that need
    /// restart fencing may retain only the returned SHA-256 commitment.
    #[must_use]
    pub fn serving_identity_outcome(
        &self,
        runtime: &ServingRuntimeManifest,
        body: &[u8],
    ) -> ServingIdentityOutcome {
        self.serving_identity_evidence(runtime, body).0
    }

    /// Validate an atomic serving identity and return an opaque process-only
    /// commitment when it matches the pinned runtime contract.
    #[must_use]
    pub fn serving_identity_evidence(
        &self,
        runtime: &ServingRuntimeManifest,
        body: &[u8],
    ) -> (ServingIdentityOutcome, Option<[u8; 32]>) {
        let Ok(identity) = serde_json::from_slice::<ServingIdentityResponse>(body) else {
            return (ServingIdentityOutcome::Decode, None);
        };
        if identity.schema_version != 3 {
            return (ServingIdentityOutcome::SchemaMismatch, None);
        }
        if !valid_incarnation(&identity.incarnation.frontend) {
            return (ServingIdentityOutcome::FrontendIncarnationInvalid, None);
        }
        if identity.incarnation.engine_core.len() != runtime.engine.core_process_count {
            return (ServingIdentityOutcome::CoreProcessMismatch, None);
        }
        let mut core_incarnations = HashSet::with_capacity(identity.incarnation.engine_core.len());
        if identity.incarnation.engine_core.iter().any(|value| {
            !valid_incarnation(value)
                || value == &identity.incarnation.frontend
                || !core_incarnations.insert(value)
        }) {
            return (ServingIdentityOutcome::CoreIncarnationInvalid, None);
        }
        if identity.model.id != self.model.id
            || identity.model.root != self.model.root
            || identity.model.max_model_len != self.model.max_model_len
        {
            return (ServingIdentityOutcome::ModelMismatch, None);
        }
        if identity.engine.version != self.engine.version
            || identity.engine.image_digest != self.engine.image_digest
        {
            return (ServingIdentityOutcome::EngineMismatch, None);
        }
        if identity.engine.core_process_count != runtime.engine.core_process_count {
            return (ServingIdentityOutcome::CoreProcessMismatch, None);
        }
        if identity.engine.kv_events != runtime.engine.kv_events {
            return (ServingIdentityOutcome::KvEventsMismatch, None);
        }
        if identity.runtime.argv != runtime.process.argv_sha256 {
            return (ServingIdentityOutcome::LaunchMismatch, None);
        }
        if identity.runtime.environment != runtime.process.environment_sha256 {
            return (ServingIdentityOutcome::EnvironmentMismatch, None);
        }
        if identity.runtime.packages != runtime.process.packages_sha256 {
            return (ServingIdentityOutcome::PackageMismatch, None);
        }
        if identity.runtime.artifacts != runtime.process.artifacts_sha256 {
            return (ServingIdentityOutcome::ArtifactMismatch, None);
        }
        if identity.tokenizer.sha256 != self.tokenizer.sha256 {
            return (ServingIdentityOutcome::TokenizerMismatch, None);
        }
        if identity.renderer.profile != self.renderer.profile {
            return (ServingIdentityOutcome::RendererMismatch, None);
        }
        (
            ServingIdentityOutcome::Match,
            Some(engine_core_incarnation_commitment(&identity.incarnation)),
        )
    }
}

fn engine_core_incarnation_commitment(incarnation: &ServingIncarnation) -> [u8; 32] {
    let mut digest = Sha256::new();
    digest.update(b"mini-dynamo-engine-core-incarnation-v1\0");
    digest.update((incarnation.engine_core.len() as u64).to_le_bytes());
    let mut cores = incarnation
        .engine_core
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>();
    cores.sort_unstable();
    for core in cores {
        digest_component(&mut digest, core.as_bytes());
    }
    digest.finalize().into()
}

fn digest_component(digest: &mut Sha256, component: &[u8]) {
    digest.update((component.len() as u64).to_le_bytes());
    digest.update(component);
}

impl ServingRuntimeManifest {
    /// Load and validate the serving-only runtime contract and its operator pin.
    ///
    /// # Errors
    ///
    /// Fails closed for an unreadable, oversized, unpinned, malformed, or
    /// compatibility-unlinked document.
    pub fn load(
        path: &Path,
        expected_manifest_sha256: &str,
        compatibility_manifest_sha256: &str,
    ) -> anyhow::Result<Self> {
        let metadata = std::fs::metadata(path)
            .with_context(|| format!("stat serving runtime manifest {}", path.display()))?;
        anyhow::ensure!(
            metadata.len() <= MAX_MANIFEST_BYTES,
            "serving runtime manifest exceeds 1 MiB"
        );
        let bytes = std::fs::read(path)
            .with_context(|| format!("read serving runtime manifest {}", path.display()))?;
        anyhow::ensure!(
            sha256_hex(&bytes) == expected_manifest_sha256,
            "serving runtime manifest SHA-256 mismatch"
        );
        let manifest: Self =
            serde_json::from_slice(&bytes).context("decode versioned serving runtime manifest")?;
        manifest.validate(compatibility_manifest_sha256)?;
        Ok(manifest)
    }

    fn validate(&self, compatibility_manifest_sha256: &str) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.schema_version == 2,
            "unsupported serving runtime schema"
        );
        anyhow::ensure!(
            self.compatibility_manifest_sha256 == compatibility_manifest_sha256
                && valid_sha256(&self.compatibility_manifest_sha256),
            "serving runtime manifest compatibility link mismatch"
        );
        anyhow::ensure!(
            (1..=64).contains(&self.engine.core_process_count),
            "serving runtime engine core process count is invalid"
        );
        self.engine.kv_events.validate()?;
        self.process.validate()
    }
}

impl ServingRuntimeProcess {
    fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            (1..=MAX_RUNTIME_ARGUMENTS).contains(&self.argv.len())
                && self.argv.first().is_some_and(|value| value == "serve"),
            "serving runtime argv is invalid"
        );
        let mut argument_bytes = 0usize;
        for value in &self.argv {
            let bytes = value.as_bytes();
            argument_bytes = argument_bytes
                .checked_add(bytes.len())
                .context("serving runtime argv is oversized")?;
            anyhow::ensure!(
                !bytes.is_empty()
                    && bytes.len() <= MAX_RUNTIME_ARGUMENT_BYTES
                    && value.is_ascii()
                    && !bytes.contains(&0),
                "serving runtime argument is invalid"
            );
        }
        anyhow::ensure!(
            argument_bytes <= MAX_RUNTIME_ARGUMENT_TOTAL_BYTES
                && !self
                    .argv
                    .iter()
                    .any(|value| sensitive_runtime_argument(value)),
            "serving runtime argv is invalid"
        );
        anyhow::ensure!(
            valid_sha256(&self.argv_sha256) && self.argv_sha256 == nul_joined_sha256(&self.argv),
            "serving runtime argv digest mismatch"
        );

        validate_runtime_map(
            &self.environment,
            MAX_RUNTIME_ENVIRONMENT,
            true,
            "environment",
        )?;
        anyhow::ensure!(
            valid_sha256(&self.environment_sha256)
                && self.environment_sha256 == json_sha256(&self.environment)?,
            "serving runtime environment digest mismatch"
        );

        validate_runtime_map(&self.packages, MAX_RUNTIME_PACKAGES, false, "packages")?;
        anyhow::ensure!(
            valid_sha256(&self.packages_sha256)
                && self.packages_sha256 == json_sha256(&self.packages)?,
            "serving runtime package digest mismatch"
        );

        anyhow::ensure!(
            (1..=MAX_RUNTIME_ARTIFACTS).contains(&self.artifacts.len()),
            "serving runtime artifact set is invalid"
        );
        let mut paths = HashSet::with_capacity(self.artifacts.len());
        for artifact in &self.artifacts {
            anyhow::ensure!(
                artifact.path.starts_with('/')
                    && artifact.path.len() <= MAX_RUNTIME_ARGUMENT_BYTES
                    && artifact.path.is_ascii()
                    && !artifact.path.as_bytes().contains(&0)
                    && !artifact.path.split('/').any(|part| part == "..")
                    && paths.insert(&artifact.path)
                    && valid_sha256(&artifact.sha256),
                "serving runtime artifact is invalid"
            );
        }
        anyhow::ensure!(
            valid_sha256(&self.artifacts_sha256)
                && self.artifacts_sha256 == json_sha256(&self.artifacts)?,
            "serving runtime artifact digest mismatch"
        );
        Ok(())
    }
}

fn validate_runtime_map(
    values: &BTreeMap<String, String>,
    limit: usize,
    environment: bool,
    description: &str,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        (1..=limit).contains(&values.len()),
        "serving runtime {description} set is invalid"
    );
    for (key, value) in values {
        let key_bytes = key.as_bytes();
        let key_valid = if environment {
            key_bytes.len() <= 128
                && key_bytes
                    .iter()
                    .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || *byte == b'_')
        } else {
            key_bytes.len() <= 256
                && key_bytes.iter().all(|byte| {
                    byte.is_ascii_alphanumeric() || matches!(*byte, b'.' | b'_' | b'+' | b'-')
                })
        };
        anyhow::ensure!(
            !key_bytes.is_empty()
                && key_valid
                && (!environment || !sensitive_runtime_environment_key(key))
                && !value.is_empty()
                && value.len() <= MAX_RUNTIME_ARGUMENT_BYTES
                && value.is_ascii()
                && !value.as_bytes().contains(&0),
            "serving runtime {description} entry is invalid"
        );
    }
    Ok(())
}

fn sensitive_runtime_argument(value: &str) -> bool {
    let name = value.split_once('=').map_or(value, |(name, _)| name);
    matches!(
        name,
        "--api-key" | "--token" | "--hf-token" | "--authorization"
    )
}

fn sensitive_runtime_environment_key(value: &str) -> bool {
    value.contains("SECRET")
        || value.contains("PASSWORD")
        || value.contains("CREDENTIAL")
        || value.contains("ACCESS_KEY")
        || value.contains("PRIVATE_KEY")
        || value.contains("BEARER")
        || value.ends_with("_TOKEN")
        || value.ends_with("_API_KEY")
        || value.ends_with("_AUTHORIZATION")
}

fn nul_joined_sha256(values: &[String]) -> String {
    let capacity = values.iter().map(String::len).sum::<usize>() + values.len();
    let mut bytes = Vec::with_capacity(capacity);
    for (index, value) in values.iter().enumerate() {
        if index > 0 {
            bytes.push(0);
        }
        bytes.extend_from_slice(value.as_bytes());
    }
    sha256_hex(&bytes)
}

fn json_sha256(value: &impl Serialize) -> anyhow::Result<String> {
    Ok(sha256_hex(&serde_json::to_vec(value)?))
}

impl KvEventsIdentity {
    fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(self.enable_kv_cache_events, "KV events are not enabled");
        anyhow::ensure!(self.publisher == "zmq", "unsupported KV event publisher");
        for (endpoint, name) in [(&self.endpoint, "live"), (&self.replay_endpoint, "replay")] {
            anyhow::ensure!(
                endpoint
                    .strip_prefix("tcp://*:")
                    .is_some_and(|port| port.parse::<u16>().is_ok_and(|port| port > 0)),
                "invalid KV event {name} endpoint"
            );
        }
        anyhow::ensure!(
            self.endpoint != self.replay_endpoint,
            "KV event endpoints must be distinct"
        );
        anyhow::ensure!(
            (1..=MAX_KV_EVENT_CAPACITY).contains(&self.buffer_steps)
                && (1..=MAX_KV_EVENT_CAPACITY).contains(&self.hwm)
                && (1..=MAX_KV_EVENT_CAPACITY).contains(&self.max_queue_size),
            "invalid KV event capacity"
        );
        anyhow::ensure!(self.topic.len() <= 4096, "KV event topic is oversized");
        Ok(())
    }
}

fn valid_incarnation(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_INCARNATION_BYTES
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b':' | b'-'))
}

fn unique_nonempty(values: &[String], kind: &str) -> anyhow::Result<HashSet<String>> {
    let mut unique = HashSet::with_capacity(values.len());
    for value in values {
        anyhow::ensure!(!value.is_empty(), "manifest {kind} is empty");
        anyhow::ensure!(unique.insert(value.clone()), "duplicate manifest {kind}");
    }
    Ok(unique)
}

#[must_use]
pub fn token_ids_sha256(token_ids: &[u32]) -> String {
    let mut digest = Sha256::new();
    for token in token_ids {
        digest.update(token.to_be_bytes());
    }
    hex_digest(digest.finalize().as_slice())
}

#[must_use]
pub fn sha256_hex(bytes: &[u8]) -> String {
    hex_digest(Sha256::digest(bytes).as_slice())
}

fn hex_digest(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(char::from(HEX[usize::from(byte >> 4)]));
        output.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    output
}

fn valid_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_compatibility_manifest() -> CompatibilityManifest {
        CompatibilityManifest {
            schema_version: 1,
            model: ModelIdentity {
                id: "model".to_owned(),
                root: "root".to_owned(),
                max_model_len: 4096,
            },
            engine: EngineIdentity {
                version: "v1".to_owned(),
                image_digest: format!("sha256:{}", "a".repeat(64)),
            },
            tokenizer: TokenizerIdentity {
                sha256: "b".repeat(64),
            },
            renderer: RendererIdentity {
                profile: "profile".to_owned(),
            },
            admitted_request_classes: vec!["plain".to_owned()],
            goldens: Vec::new(),
        }
    }

    fn test_process() -> ServingRuntimeProcess {
        let argv = vec!["serve".to_owned(), "model".to_owned()];
        let environment = BTreeMap::from([("MODE".to_owned(), "test".to_owned())]);
        let packages = BTreeMap::from([("vllm".to_owned(), "v1".to_owned())]);
        let artifacts = vec![ServingRuntimeArtifact {
            path: "/runtime".to_owned(),
            sha256: "d".repeat(64),
        }];
        ServingRuntimeProcess {
            argv_sha256: nul_joined_sha256(&argv),
            environment_sha256: json_sha256(&environment).unwrap(),
            packages_sha256: json_sha256(&packages).unwrap(),
            artifacts_sha256: json_sha256(&artifacts).unwrap(),
            argv,
            environment,
            packages,
            artifacts,
        }
    }

    #[test]
    fn token_vector_digest_is_unambiguous_big_endian_u32() {
        assert_eq!(
            token_ids_sha256(&[1, 256]),
            sha256_hex(&[0, 0, 0, 1, 0, 0, 1, 0])
        );
    }

    #[test]
    fn runtime_identity_requires_exact_model_and_version() {
        let manifest = test_compatibility_manifest();
        let models = br#"{"data":[{"id":"model","root":"root","max_model_len":4096}]}"#;
        assert_eq!(
            manifest.runtime_outcome(models, br#"{"version":"v1"}"#),
            RuntimeOutcome::Match
        );
        assert_eq!(
            manifest.runtime_outcome(models, br#"{"version":"v2"}"#),
            RuntimeOutcome::VersionMismatch
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)] // One exhaustive cross-layer identity contract.
    fn atomic_serving_identity_binds_every_compatibility_layer_and_incarnation() {
        let manifest = test_compatibility_manifest();
        let runtime = ServingRuntimeManifest {
            schema_version: 2,
            compatibility_manifest_sha256: "c".repeat(64),
            engine: ServingRuntimeEngine {
                core_process_count: 1,
                kv_events: KvEventsIdentity {
                    enable_kv_cache_events: true,
                    publisher: "zmq".to_owned(),
                    endpoint: "tcp://*:5557".to_owned(),
                    replay_endpoint: "tcp://*:5558".to_owned(),
                    buffer_steps: 10_000,
                    hwm: 100_000,
                    max_queue_size: 100_000,
                    topic: String::new(),
                },
            },
            process: test_process(),
        };
        let identity = serde_json::json!({
            "schema_version": 3,
            "incarnation": {
                "frontend": "boot-1234:9:100",
                "engine_core": ["boot-1234:10:101"],
            },
            "model": {"id": "model", "root": "root", "max_model_len": 4096},
            "engine": {
                "version": "v1",
                "image_digest": format!("sha256:{}", "a".repeat(64)),
                "core_process_count": 1,
                "kv_events": {
                    "enable_kv_cache_events": true,
                    "publisher": "zmq",
                    "endpoint": "tcp://*:5557",
                    "replay_endpoint": "tcp://*:5558",
                    "buffer_steps": 10000,
                    "hwm": 100_000,
                    "max_queue_size": 100_000,
                    "topic": "",
                },
            },
            "tokenizer": {"sha256": "b".repeat(64)},
            "renderer": {"profile": "profile"},
            "runtime": {
                "argv_sha256": runtime.process.argv_sha256,
                "environment_sha256": runtime.process.environment_sha256,
                "packages_sha256": runtime.process.packages_sha256,
                "artifacts_sha256": runtime.process.artifacts_sha256,
            },
        });
        let body = serde_json::to_vec(&identity).unwrap();
        assert_eq!(
            manifest.serving_identity_outcome(&runtime, &body),
            ServingIdentityOutcome::Match
        );
        let (_, commitment) = manifest.serving_identity_evidence(&runtime, &body);
        let pretty = serde_json::to_vec_pretty(&identity).unwrap();
        assert_eq!(
            manifest.serving_identity_evidence(&runtime, &pretty).1,
            commitment
        );
        let mut restarted = identity.clone();
        restarted["incarnation"]["frontend"] =
            serde_json::Value::String("boot-1234:11:102".to_owned());
        let (outcome, restarted_commitment) =
            manifest.serving_identity_evidence(&runtime, &serde_json::to_vec(&restarted).unwrap());
        assert_eq!(outcome, ServingIdentityOutcome::Match);
        assert_eq!(restarted_commitment, commitment);

        restarted["incarnation"]["engine_core"][0] =
            serde_json::Value::String("boot-1234:12:103".to_owned());
        let (outcome, restarted_core_commitment) =
            manifest.serving_identity_evidence(&runtime, &serde_json::to_vec(&restarted).unwrap());
        assert_eq!(outcome, ServingIdentityOutcome::Match);
        assert_ne!(restarted_core_commitment, commitment);

        let mut mismatched = identity;
        mismatched["engine"]["image_digest"] =
            serde_json::Value::String(format!("sha256:{}", "c".repeat(64)));
        assert_eq!(
            manifest.serving_identity_outcome(&runtime, &serde_json::to_vec(&mismatched).unwrap()),
            ServingIdentityOutcome::EngineMismatch
        );
        mismatched["engine"]["image_digest"] =
            serde_json::Value::String(format!("sha256:{}", "a".repeat(64)));
        mismatched["incarnation"]["engine_core"][0] =
            serde_json::Value::String("unsafe value".to_owned());
        assert_eq!(
            manifest.serving_identity_outcome(&runtime, &serde_json::to_vec(&mismatched).unwrap()),
            ServingIdentityOutcome::CoreIncarnationInvalid
        );
        mismatched["incarnation"]["engine_core"][0] =
            serde_json::Value::String("boot-1234:10:101".to_owned());
        mismatched["engine"]["kv_events"]["hwm"] = serde_json::json!(99_999);
        assert_eq!(
            manifest.serving_identity_outcome(&runtime, &serde_json::to_vec(&mismatched).unwrap()),
            ServingIdentityOutcome::KvEventsMismatch
        );
        mismatched["engine"]["kv_events"]["hwm"] = serde_json::json!(100_000);
        for (field, expected) in [
            ("argv_sha256", ServingIdentityOutcome::LaunchMismatch),
            (
                "environment_sha256",
                ServingIdentityOutcome::EnvironmentMismatch,
            ),
            ("packages_sha256", ServingIdentityOutcome::PackageMismatch),
            ("artifacts_sha256", ServingIdentityOutcome::ArtifactMismatch),
        ] {
            let original = mismatched["runtime"][field].clone();
            mismatched["runtime"][field] = serde_json::Value::String("f".repeat(64));
            assert_eq!(
                manifest
                    .serving_identity_outcome(&runtime, &serde_json::to_vec(&mismatched).unwrap()),
                expected
            );
            mismatched["runtime"][field] = original;
        }
    }

    #[test]
    fn engine_core_commitment_ignores_frontend_and_core_list_order() {
        let first = ServingIncarnation {
            frontend: "boot:1:10".to_owned(),
            engine_core: vec!["boot:2:20".to_owned(), "boot:3:30".to_owned()],
        };
        let reordered = ServingIncarnation {
            frontend: "boot:4:40".to_owned(),
            engine_core: vec!["boot:3:30".to_owned(), "boot:2:20".to_owned()],
        };
        assert_eq!(
            engine_core_incarnation_commitment(&first),
            engine_core_incarnation_commitment(&reordered)
        );
        let changed = ServingIncarnation {
            frontend: reordered.frontend,
            engine_core: vec!["boot:3:30".to_owned(), "boot:5:50".to_owned()],
        };
        assert_ne!(
            engine_core_incarnation_commitment(&first),
            engine_core_incarnation_commitment(&changed)
        );
    }

    #[test]
    fn serving_identity_rejects_legacy_schema_and_duplicate_cores() {
        let manifest: CompatibilityManifest =
            serde_json::from_slice(include_bytes!("../compat/deepseek-v4-r34.json")).unwrap();
        let runtime: ServingRuntimeManifest = serde_json::from_slice(include_bytes!(
            "../compat/deepseek-v4-r34-serving-runtime.json"
        ))
        .unwrap();
        assert_eq!(
            manifest.serving_identity_outcome(&runtime, br#"{"schema_version":1}"#),
            ServingIdentityOutcome::Decode
        );

        let mut identity = serde_json::json!({
            "schema_version": 3,
            "incarnation": {
                "frontend": "boot:1:10",
                "engine_core": ["boot:2:20", "boot:2:20"],
            },
            "model": {
                "id": manifest.model.id,
                "root": manifest.model.root,
                "max_model_len": manifest.model.max_model_len,
            },
            "engine": {
                "version": manifest.engine.version,
                "image_digest": manifest.engine.image_digest,
                "core_process_count": 2,
                "kv_events": runtime.engine.kv_events,
            },
            "tokenizer": {"sha256": manifest.tokenizer.sha256},
            "renderer": {"profile": manifest.renderer.profile},
            "runtime": {
                "argv_sha256": runtime.process.argv_sha256,
                "environment_sha256": runtime.process.environment_sha256,
                "packages_sha256": runtime.process.packages_sha256,
                "artifacts_sha256": runtime.process.artifacts_sha256,
            },
        });
        let two_core_runtime = ServingRuntimeManifest {
            schema_version: 2,
            compatibility_manifest_sha256: runtime.compatibility_manifest_sha256,
            engine: ServingRuntimeEngine {
                core_process_count: 2,
                kv_events: runtime.engine.kv_events,
            },
            process: test_process(),
        };
        assert_eq!(
            manifest.serving_identity_outcome(
                &two_core_runtime,
                &serde_json::to_vec(&identity).unwrap()
            ),
            ServingIdentityOutcome::CoreIncarnationInvalid
        );
        identity["schema_version"] = serde_json::json!(2);
        assert_eq!(
            manifest.serving_identity_outcome(
                &two_core_runtime,
                &serde_json::to_vec(&identity).unwrap()
            ),
            ServingIdentityOutcome::SchemaMismatch
        );
    }

    #[test]
    fn serving_runtime_manifest_rejects_malformed_or_divergent_authority() {
        let baseline: Value = serde_json::from_slice(include_bytes!(
            "../compat/deepseek-v4-r34-serving-runtime.json"
        ))
        .unwrap();
        let compatibility_sha = "4ae2503554fa7089bc455e2ee89af0677c5cabec523d6b08d91a93d9ec9259aa";
        let mut cases = Vec::new();
        for (pointer, value) in [
            ("/schema_version", serde_json::json!(1)),
            ("/engine/core_process_count", serde_json::json!(0)),
            ("/engine/kv_events/publisher", serde_json::json!("null")),
            (
                "/engine/kv_events/endpoint",
                serde_json::json!("tcp://*:٥٥٥٧"),
            ),
            (
                "/engine/kv_events/replay_endpoint",
                serde_json::json!("tcp://*:5557"),
            ),
            ("/engine/kv_events/buffer_steps", serde_json::json!(0)),
            (
                "/engine/kv_events/hwm",
                serde_json::json!(MAX_KV_EVENT_CAPACITY + 1),
            ),
            ("/process/argv/0", serde_json::json!("vllm")),
            ("/process/argv_sha256", serde_json::json!("0".repeat(64))),
            (
                "/process/environment_sha256",
                serde_json::json!("0".repeat(64)),
            ),
            (
                "/process/packages_sha256",
                serde_json::json!("0".repeat(64)),
            ),
            (
                "/process/artifacts/0/path",
                serde_json::json!("relative/runtime"),
            ),
        ] {
            let mut candidate = baseline.clone();
            *candidate.pointer_mut(pointer).unwrap() = value;
            cases.push(candidate);
        }
        let mut unknown = baseline;
        unknown["engine"]["kv_events"]["unknown"] = serde_json::json!(true);
        cases.push(unknown);
        for key in ["PRIVATE_API_KEY", "AWS_ACCESS_KEY_ID", "TLS_PRIVATE_KEY"] {
            let mut secret = serde_json::from_slice::<Value>(include_bytes!(
                "../compat/deepseek-v4-r34-serving-runtime.json"
            ))
            .unwrap();
            secret["process"]["environment"][key] = serde_json::json!("private");
            cases.push(secret);
        }

        for candidate in cases {
            let rejected = serde_json::from_value::<ServingRuntimeManifest>(candidate)
                .map_or(true, |manifest| {
                    manifest.validate(compatibility_sha).is_err()
                });
            assert!(rejected);
        }
    }

    #[test]
    fn committed_node06_manifest_is_structurally_valid() {
        let bytes = include_bytes!("../compat/deepseek-v4-r34.json");
        let manifest: CompatibilityManifest = serde_json::from_slice(bytes).unwrap();
        manifest
            .validate(
                "8f9f37ca37fdc4f5fd36d5cf4d3b0e8392edb4e894fd10cc0d70b4957c8633cf",
                "deepseek-v4-r34",
            )
            .unwrap();
        assert_eq!(manifest.goldens.len(), 10);
        assert_eq!(manifest.admitted_request_classes.len(), 9);

        let runtime_path = Path::new("compat/deepseek-v4-r34-serving-runtime.json");
        let runtime = ServingRuntimeManifest::load(
            runtime_path,
            "294b3130d696fdcfb2884f9e41bb705e439c63fd7c7c321a764121707af95ff4",
            "4ae2503554fa7089bc455e2ee89af0677c5cabec523d6b08d91a93d9ec9259aa",
        )
        .unwrap();
        assert_eq!(runtime.engine.core_process_count, 1);
        assert!(
            ServingRuntimeManifest::load(
                runtime_path,
                &"0".repeat(64),
                "4ae2503554fa7089bc455e2ee89af0677c5cabec523d6b08d91a93d9ec9259aa",
            )
            .is_err()
        );
        assert!(
            ServingRuntimeManifest::load(
                runtime_path,
                "294b3130d696fdcfb2884f9e41bb705e439c63fd7c7c321a764121707af95ff4",
                &"f".repeat(64),
            )
            .is_err()
        );
    }
}
