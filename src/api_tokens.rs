//! Minted client API tokens.
//!
//! A token is `rjk_<12 hex id>_<64 hex secret>`. Only the id and
//! `SHA-256(secret)` are stored, so the state file cannot be replayed as a
//! credential. Engines never see a minted token: the accounting layer swaps
//! it for the master bearer before dispatch.

use std::{
    collections::HashMap,
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::ui_auth::{constant_time_equal, hex};

pub const TOKEN_PREFIX: &str = "rjk_";
pub const MAX_TOKENS: usize = 10_000;
pub const MAX_NAME_CHARS: usize = 64;
pub const MAX_DESCRIPTION_CHARS: usize = 512;
const ID_BYTES: usize = 6;
const SECRET_BYTES: usize = 32;
const STATE_VERSION: u32 = 1;
pub const STATE_FILE: &str = "api-tokens.json";

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
struct StoredToken {
    id: String,
    name: String,
    #[serde(default)]
    description: String,
    created_at_ms: u64,
    #[serde(default)]
    expires_at_ms: Option<u64>,
    #[serde(default)]
    revoked_at_ms: Option<u64>,
    #[serde(default)]
    last_used_ms: Option<u64>,
    secret_sha256: String,
}

#[derive(Deserialize, Serialize)]
struct PersistedTokens {
    version: u32,
    tokens: Vec<StoredToken>,
}

struct Entry {
    token: StoredToken,
    /// Zero means never used. Updated on the request path without the write
    /// lock and folded into the state file by the periodic flush.
    last_used_ms: AtomicU64,
}

impl Entry {
    fn new(token: StoredToken) -> Arc<Self> {
        let last_used = token.last_used_ms.unwrap_or(0);
        Arc::new(Self {
            token,
            last_used_ms: AtomicU64::new(last_used),
        })
    }

    fn snapshot(&self) -> StoredToken {
        let mut token = self.token.clone();
        let last_used = self.last_used_ms.load(Ordering::Relaxed);
        token.last_used_ms = (last_used > 0).then_some(last_used);
        token
    }
}

/// The public view of one token. Never carries the secret or its hash.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct TokenView {
    pub id: String,
    pub prefix: String,
    pub name: String,
    pub description: String,
    pub created_at_ms: u64,
    pub expires_at_ms: Option<u64>,
    pub revoked_at_ms: Option<u64>,
    pub last_used_ms: Option<u64>,
    pub status: TokenStatus,
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TokenStatus {
    Active,
    Expired,
    Revoked,
}

impl TokenStatus {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Expired => "expired",
            Self::Revoked => "revoked",
        }
    }
}

/// Why a presented `rjk_` credential was refused. Labels are closed and
/// content-free so they can be exported as metric labels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VerifyError {
    Malformed,
    Unknown,
    Expired,
    Revoked,
}

impl VerifyError {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Malformed => "malformed",
            Self::Unknown => "unknown",
            Self::Expired => "expired",
            Self::Revoked => "revoked",
        }
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum MutationError {
    #[error("{0}")]
    Invalid(String),
    #[error("token not found")]
    NotFound,
    #[error("token limit of {MAX_TOKENS} reached")]
    Full,
    #[error("persisting token state failed: {0}")]
    Persist(String),
}

#[derive(Debug, Default)]
pub struct NewToken {
    pub name: String,
    pub description: String,
    pub expires_at_ms: Option<u64>,
}

#[derive(Debug, Default)]
pub struct TokenPatch {
    pub name: Option<String>,
    pub description: Option<String>,
    /// `Some(None)` clears the expiry.
    pub expires_at_ms: Option<Option<u64>>,
}

pub struct TokenStore {
    entries: RwLock<HashMap<String, Arc<Entry>>>,
    path: Option<PathBuf>,
    /// Serializes snapshot-and-write so an older snapshot can never land on
    /// disk after a newer one.
    persist: parking_lot::Mutex<()>,
    last_used_dirty: AtomicBool,
}

impl TokenStore {
    /// An unpersisted store, for tests and for deployments that only use the
    /// master token.
    #[must_use]
    pub fn in_memory() -> Self {
        Self {
            entries: RwLock::new(HashMap::new()),
            path: None,
            persist: parking_lot::Mutex::new(()),
            last_used_dirty: AtomicBool::new(false),
        }
    }

    /// Opens `dir/api-tokens.json`, creating an empty store when it does not
    /// exist yet. The directory itself must already exist.
    ///
    /// # Errors
    ///
    /// Returns an error when the directory is missing or the existing state
    /// file is unreadable or malformed. A corrupt token file fails closed
    /// rather than silently dropping every minted credential.
    pub fn open(dir: &Path) -> anyhow::Result<Self> {
        anyhow::ensure!(
            dir.is_dir(),
            "API state directory {} does not exist",
            dir.display()
        );
        let path = dir.join(STATE_FILE);
        let mut entries = HashMap::new();
        match fs::read(&path) {
            Ok(body) => {
                let state: PersistedTokens = serde_json::from_slice(&body)
                    .map_err(|error| anyhow::anyhow!("parse {}: {error}", path.display()))?;
                anyhow::ensure!(
                    state.version == STATE_VERSION,
                    "unsupported token state version {}",
                    state.version
                );
                for token in state.tokens {
                    anyhow::ensure!(valid_id(&token.id), "token state holds a malformed id");
                    anyhow::ensure!(
                        token.secret_sha256.len() == 64,
                        "token state holds a malformed hash"
                    );
                    entries.insert(token.id.clone(), Entry::new(token));
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => anyhow::bail!("read {}: {error}", path.display()),
        }
        Ok(Self {
            entries: RwLock::new(entries),
            path: Some(path),
            persist: parking_lot::Mutex::new(()),
            last_used_dirty: AtomicBool::new(false),
        })
    }

    /// Verifies a presented `rjk_` credential and returns its id. Successful
    /// verification records `now_ms` as the token's last use.
    ///
    /// # Errors
    ///
    /// Returns the closed refusal reason.
    pub fn verify(&self, presented: &str, now_ms: u64) -> Result<Arc<str>, VerifyError> {
        let (id, secret) = parse_token(presented).ok_or(VerifyError::Malformed)?;
        let entries = self.entries.read();
        let entry = entries.get(id).ok_or(VerifyError::Unknown)?;
        let digest = hex(&Sha256::digest(secret.as_bytes()));
        if !constant_time_equal(digest.as_bytes(), entry.token.secret_sha256.as_bytes()) {
            return Err(VerifyError::Unknown);
        }
        match status(&entry.token, now_ms) {
            TokenStatus::Active => {}
            TokenStatus::Expired => return Err(VerifyError::Expired),
            TokenStatus::Revoked => return Err(VerifyError::Revoked),
        }
        // Coarsen to the second so a busy token does not dirty the state on
        // every request.
        let previous = entry.last_used_ms.load(Ordering::Relaxed);
        if now_ms.saturating_sub(previous) >= 1_000 {
            entry.last_used_ms.store(now_ms, Ordering::Relaxed);
            self.last_used_dirty.store(true, Ordering::Relaxed);
        }
        Ok(Arc::from(id))
    }

    /// Every token, newest first.
    #[must_use]
    pub fn list(&self, now_ms: u64) -> Vec<TokenView> {
        let mut views = self
            .entries
            .read()
            .values()
            .map(|entry| view(&entry.snapshot(), now_ms))
            .collect::<Vec<_>>();
        views.sort_by(|left, right| {
            right
                .created_at_ms
                .cmp(&left.created_at_ms)
                .then_with(|| left.id.cmp(&right.id))
        });
        views
    }

    #[must_use]
    pub fn get(&self, id: &str, now_ms: u64) -> Option<TokenView> {
        self.entries
            .read()
            .get(id)
            .map(|entry| view(&entry.snapshot(), now_ms))
    }

    #[must_use]
    pub fn status_counts(&self, now_ms: u64) -> [(TokenStatus, usize); 3] {
        let mut counts = [
            (TokenStatus::Active, 0),
            (TokenStatus::Expired, 0),
            (TokenStatus::Revoked, 0),
        ];
        for entry in self.entries.read().values() {
            let status = status(&entry.token, now_ms);
            if let Some(slot) = counts.iter_mut().find(|(kind, _)| *kind == status) {
                slot.1 += 1;
            }
        }
        counts
    }

    /// Mints a token and returns its view plus the one-time full secret.
    ///
    /// # Errors
    ///
    /// Rejects invalid fields, a full store, or a persistence failure (in
    /// which case nothing is minted).
    pub fn create(
        &self,
        request: &NewToken,
        now_ms: u64,
    ) -> Result<(TokenView, String), MutationError> {
        let name = validate_name(&request.name)?;
        let description = validate_description(&request.description)?;
        validate_expiry(request.expires_at_ms, now_ms)?;
        let _persist = self.persist.lock();
        let (token, secret) = {
            let mut entries = self.entries.write();
            if entries.len() >= MAX_TOKENS {
                return Err(MutationError::Full);
            }
            let (id, secret) = loop {
                let id = random_hex(ID_BYTES)?;
                if !entries.contains_key(&id) {
                    break (id, random_hex(SECRET_BYTES)?);
                }
            };
            let token = StoredToken {
                id: id.clone(),
                name,
                description,
                created_at_ms: now_ms,
                expires_at_ms: request.expires_at_ms,
                revoked_at_ms: None,
                last_used_ms: None,
                secret_sha256: hex(&Sha256::digest(secret.as_bytes())),
            };
            entries.insert(id.clone(), Entry::new(token.clone()));
            (token, format!("{TOKEN_PREFIX}{id}_{secret}"))
        };
        if let Err(error) = self.write_locked() {
            self.entries.write().remove(&token.id);
            return Err(error);
        }
        Ok((view(&token, now_ms), secret))
    }

    /// Applies a partial update.
    ///
    /// # Errors
    ///
    /// Rejects invalid fields, unknown ids, or a persistence failure (which
    /// restores the previous record).
    pub fn update(
        &self,
        id: &str,
        patch: &TokenPatch,
        now_ms: u64,
    ) -> Result<TokenView, MutationError> {
        let name = patch.name.as_deref().map(validate_name).transpose()?;
        let description = patch
            .description
            .as_deref()
            .map(validate_description)
            .transpose()?;
        if let Some(expiry) = patch.expires_at_ms {
            validate_expiry(expiry, now_ms)?;
        }
        self.replace(id, now_ms, |token| {
            if let Some(name) = name {
                token.name = name;
            }
            if let Some(description) = description {
                token.description = description;
            }
            if let Some(expiry) = patch.expires_at_ms {
                token.expires_at_ms = expiry;
            }
        })
    }

    /// Revokes a token. Revoking twice keeps the first revocation time.
    ///
    /// # Errors
    ///
    /// Unknown id or persistence failure.
    pub fn revoke(&self, id: &str, now_ms: u64) -> Result<TokenView, MutationError> {
        self.replace(id, now_ms, |token| {
            token.revoked_at_ms.get_or_insert(now_ms);
        })
    }

    /// Deletes a token. Its usage history stays attributed to the id.
    ///
    /// # Errors
    ///
    /// Unknown id or persistence failure.
    pub fn delete(&self, id: &str) -> Result<(), MutationError> {
        let _persist = self.persist.lock();
        let previous = self
            .entries
            .write()
            .remove(id)
            .ok_or(MutationError::NotFound)?;
        if let Err(error) = self.write_locked() {
            self.entries.write().insert(id.to_owned(), previous);
            return Err(error);
        }
        Ok(())
    }

    /// Writes last-use times if any changed since the previous flush.
    ///
    /// # Errors
    ///
    /// Persistence failure; the dirty bit is restored for the next attempt.
    pub fn flush_last_used(&self) -> Result<(), MutationError> {
        if !self.last_used_dirty.swap(false, Ordering::Relaxed) {
            return Ok(());
        }
        let _persist = self.persist.lock();
        self.write_locked().inspect_err(|_| {
            self.last_used_dirty.store(true, Ordering::Relaxed);
        })
    }

    fn replace(
        &self,
        id: &str,
        now_ms: u64,
        apply: impl FnOnce(&mut StoredToken),
    ) -> Result<TokenView, MutationError> {
        let _persist = self.persist.lock();
        let (previous, updated) = {
            let mut entries = self.entries.write();
            let previous = entries.get(id).cloned().ok_or(MutationError::NotFound)?;
            let mut token = previous.snapshot();
            apply(&mut token);
            entries.insert(id.to_owned(), Entry::new(token.clone()));
            (previous, token)
        };
        if let Err(error) = self.write_locked() {
            self.entries.write().insert(id.to_owned(), previous);
            return Err(error);
        }
        Ok(view(&updated, now_ms))
    }

    /// Caller holds `self.persist`.
    fn write_locked(&self) -> Result<(), MutationError> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        let mut tokens = self
            .entries
            .read()
            .values()
            .map(|entry| entry.snapshot())
            .collect::<Vec<_>>();
        tokens.sort_by(|left, right| left.id.cmp(&right.id));
        let body = serde_json::to_vec_pretty(&PersistedTokens {
            version: STATE_VERSION,
            tokens,
        })
        .map_err(|error| MutationError::Persist(error.to_string()))?;
        write_private_atomic(path, &body).map_err(|error| MutationError::Persist(error.to_string()))
    }
}

/// Writes `body` to `path` through a same-directory mode-0600 temporary file,
/// fsync, and rename, so a crash leaves either the old or the new state.
///
/// # Errors
///
/// Any I/O failure.
pub fn write_private_atomic(path: &Path, body: &[u8]) -> std::io::Result<()> {
    let mut temporary = path.as_os_str().to_owned();
    temporary.push(".tmp");
    let temporary = PathBuf::from(temporary);
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary)?;
    file.write_all(body)?;
    file.sync_all()?;
    drop(file);
    fs::rename(&temporary, path)?;
    if let Some(parent) = path.parent()
        && let Ok(directory) = fs::File::open(parent)
    {
        let _ = directory.sync_all();
    }
    Ok(())
}

fn view(token: &StoredToken, now_ms: u64) -> TokenView {
    TokenView {
        id: token.id.clone(),
        prefix: format!("{TOKEN_PREFIX}{}", token.id),
        name: token.name.clone(),
        description: token.description.clone(),
        created_at_ms: token.created_at_ms,
        expires_at_ms: token.expires_at_ms,
        revoked_at_ms: token.revoked_at_ms,
        last_used_ms: token.last_used_ms,
        status: status(token, now_ms),
    }
}

fn status(token: &StoredToken, now_ms: u64) -> TokenStatus {
    if token.revoked_at_ms.is_some() {
        TokenStatus::Revoked
    } else if token.expires_at_ms.is_some_and(|expiry| expiry <= now_ms) {
        TokenStatus::Expired
    } else {
        TokenStatus::Active
    }
}

fn parse_token(presented: &str) -> Option<(&str, &str)> {
    let rest = presented.strip_prefix(TOKEN_PREFIX)?;
    let (id, secret) = rest.split_once('_')?;
    (valid_id(id) && secret.len() == SECRET_BYTES * 2 && is_lower_hex(secret))
        .then_some((id, secret))
}

fn valid_id(id: &str) -> bool {
    id.len() == ID_BYTES * 2 && is_lower_hex(id)
}

fn is_lower_hex(value: &str) -> bool {
    value
        .bytes()
        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn validate_name(name: &str) -> Result<String, MutationError> {
    let name = name.trim();
    let chars = name.chars().count();
    if chars == 0 || chars > MAX_NAME_CHARS {
        return Err(MutationError::Invalid(format!(
            "name must be 1-{MAX_NAME_CHARS} characters"
        )));
    }
    if name.chars().any(char::is_control) {
        return Err(MutationError::Invalid(
            "name must not contain control characters".to_owned(),
        ));
    }
    Ok(name.to_owned())
}

fn validate_description(description: &str) -> Result<String, MutationError> {
    let description = description.trim();
    if description.chars().count() > MAX_DESCRIPTION_CHARS {
        return Err(MutationError::Invalid(format!(
            "description must be at most {MAX_DESCRIPTION_CHARS} characters"
        )));
    }
    if description
        .chars()
        .any(|character| character.is_control() && character != '\n')
    {
        return Err(MutationError::Invalid(
            "description must not contain control characters".to_owned(),
        ));
    }
    Ok(description.to_owned())
}

fn validate_expiry(expires_at_ms: Option<u64>, now_ms: u64) -> Result<(), MutationError> {
    match expires_at_ms {
        Some(expiry) if expiry <= now_ms => Err(MutationError::Invalid(
            "expires_at_ms must be in the future".to_owned(),
        )),
        _ => Ok(()),
    }
}

fn random_hex(bytes: usize) -> Result<String, MutationError> {
    let mut buffer = vec![0_u8; bytes];
    getrandom::fill(&mut buffer)
        .map_err(|error| MutationError::Persist(format!("random source failed: {error}")))?;
    Ok(hex(&buffer))
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_760_000_000_000;

    fn new_token(name: &str) -> NewToken {
        NewToken {
            name: name.to_owned(),
            ..NewToken::default()
        }
    }

    #[test]
    fn api_tokens_mint_verify_and_never_store_the_secret() {
        let store = TokenStore::in_memory();
        let (view, secret) = store.create(&new_token("helix"), NOW).unwrap();
        assert!(secret.starts_with(&format!("rjk_{}_", view.id)));
        assert_eq!(view.status, TokenStatus::Active);
        assert_eq!(
            store.verify(&secret, NOW + 5_000).unwrap().as_ref(),
            view.id
        );
        assert_eq!(
            store.get(&view.id, NOW).unwrap().last_used_ms,
            Some(NOW + 5_000)
        );
        let snapshot = store.entries.read()[&view.id].snapshot();
        assert!(
            !serde_json::to_string(&snapshot)
                .unwrap()
                .contains(&secret[17..])
        );
    }

    #[test]
    fn api_tokens_reject_wrong_secret_expired_and_revoked() {
        let store = TokenStore::in_memory();
        let (view, secret) = store
            .create(
                &NewToken {
                    name: "short".to_owned(),
                    expires_at_ms: Some(NOW + 1_000),
                    ..NewToken::default()
                },
                NOW,
            )
            .unwrap();
        let mut wrong = secret.clone();
        let last = wrong.pop().unwrap();
        wrong.push(if last == '0' { '1' } else { '0' });
        assert_eq!(store.verify(&wrong, NOW), Err(VerifyError::Unknown));
        assert_eq!(store.verify("rjk_nothex", NOW), Err(VerifyError::Malformed));
        assert_eq!(
            store.verify(&secret, NOW + 1_000),
            Err(VerifyError::Expired)
        );
        let (other, other_secret) = store.create(&new_token("other"), NOW).unwrap();
        store.revoke(&other.id, NOW + 10).unwrap();
        assert_eq!(
            store.verify(&other_secret, NOW + 20),
            Err(VerifyError::Revoked)
        );
        assert_eq!(
            store.get(&view.id, NOW + 2_000).unwrap().status,
            TokenStatus::Expired
        );
    }

    #[test]
    fn api_tokens_validate_fields() {
        let store = TokenStore::in_memory();
        assert!(matches!(
            store.create(&new_token("  "), NOW),
            Err(MutationError::Invalid(_))
        ));
        assert!(matches!(
            store.create(&new_token(&"x".repeat(65)), NOW),
            Err(MutationError::Invalid(_))
        ));
        assert!(matches!(
            store.create(
                &NewToken {
                    name: "past".to_owned(),
                    expires_at_ms: Some(NOW),
                    ..NewToken::default()
                },
                NOW
            ),
            Err(MutationError::Invalid(_))
        ));
        let (view, _) = store.create(&new_token("ok"), NOW).unwrap();
        let updated = store
            .update(
                &view.id,
                &TokenPatch {
                    description: Some("fleet".to_owned()),
                    expires_at_ms: Some(Some(NOW + 60_000)),
                    ..TokenPatch::default()
                },
                NOW,
            )
            .unwrap();
        assert_eq!(updated.description, "fleet");
        assert_eq!(updated.expires_at_ms, Some(NOW + 60_000));
        let cleared = store
            .update(
                &view.id,
                &TokenPatch {
                    expires_at_ms: Some(None),
                    ..TokenPatch::default()
                },
                NOW,
            )
            .unwrap();
        assert_eq!(cleared.expires_at_ms, None);
        assert_eq!(
            store.update("000000000000", &TokenPatch::default(), NOW),
            Err(MutationError::NotFound)
        );
    }

    #[test]
    fn api_tokens_persist_and_reload() {
        let dir = std::env::temp_dir().join(format!("rj-tokens-{}", random_hex(6).unwrap()));
        fs::create_dir(&dir).unwrap();
        let store = TokenStore::open(&dir).unwrap();
        let (kept, secret) = store.create(&new_token("kept"), NOW).unwrap();
        let (gone, _) = store.create(&new_token("gone"), NOW + 1).unwrap();
        store.delete(&gone.id).unwrap();
        store.verify(&secret, NOW + 9_000).unwrap();
        store.flush_last_used().unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(dir.join(STATE_FILE))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let reloaded = TokenStore::open(&dir).unwrap();
        let views = reloaded.list(NOW);
        assert_eq!(views.len(), 1);
        assert_eq!(views[0].id, kept.id);
        assert_eq!(views[0].last_used_ms, Some(NOW + 9_000));
        assert!(reloaded.verify(&secret, NOW + 10_000).is_ok());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn api_tokens_corrupt_state_fails_closed() {
        let dir = std::env::temp_dir().join(format!("rj-tokens-{}", random_hex(6).unwrap()));
        fs::create_dir(&dir).unwrap();
        fs::write(dir.join(STATE_FILE), b"{not json").unwrap();
        assert!(TokenStore::open(&dir).is_err());
        fs::remove_dir_all(&dir).unwrap();
    }
}
