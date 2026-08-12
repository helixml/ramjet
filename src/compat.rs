//! Versioned compatibility evidence for local request tokenization.
//!
//! A manifest binds the local renderer/tokenizer to the runtime identity of the
//! engines and to synthetic token-vector goldens. Exact token IDs may be used
//! for shadow scoring only while every layer matches.

use std::{collections::HashSet, path::Path};

use anyhow::Context;
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

const MAX_MANIFEST_BYTES: u64 = 1 << 20;
const MAX_GOLDENS: usize = 64;
const MAX_ADMITTED_CLASSES: usize = 32;

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
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_vector_digest_is_unambiguous_big_endian_u32() {
        assert_eq!(
            token_ids_sha256(&[1, 256]),
            sha256_hex(&[0, 0, 0, 1, 0, 0, 1, 0])
        );
    }

    #[test]
    fn runtime_identity_requires_exact_model_and_version() {
        let manifest = CompatibilityManifest {
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
        };
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
    }
}
