//! Fail-closed loading of the snapshot session authentication secret.
//!
//! The startup loader accepts exactly 32 raw bytes from a regular, singly
//! linked file below a trusted directory path. It deliberately does not accept
//! text, hexadecimal, or newline-terminated encodings. Errors never contain a
//! path, owner, permission mode, file content, or other source value.

use std::{
    fs::{self, File, Metadata},
    io::Read,
    os::unix::fs::MetadataExt,
    path::{Component, Path, PathBuf},
};

use thiserror::Error;

use crate::snapshot_session::{SNAPSHOT_SESSION_SECRET_BYTES, SnapshotSessionSecret};

const MAX_SECRET_READ_BYTES: usize = SNAPSHOT_SESSION_SECRET_BYTES + 1;
const MAX_CONTROL_FILE_BYTES: usize = 64 * 1024;
const GROUP_OR_WORLD_WRITE: u32 = 0o022;
const STICKY_BIT: u32 = 0o1000;

/// Ownership expected for the secret file and its immediate parent.
///
/// Root-owned ancestors are also trusted. A root-owned sticky ancestor such as
/// `/tmp` may be group/world writable, but the immediate parent containing the
/// secret must never be group/world writable.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SnapshotSecretFilePolicy {
    pub expected_owner_uid: u32,
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum SnapshotSecretFileError {
    #[error("snapshot session secret path is invalid")]
    InvalidPath,
    #[error("snapshot session secret parent is not trusted")]
    UntrustedParent,
    #[error("snapshot session secret metadata is invalid")]
    InvalidMetadata,
    #[error("snapshot session secret is not a regular file")]
    NotRegularFile,
    #[error("snapshot session secret has an invalid link count")]
    InvalidLinkCount,
    #[error("snapshot session secret has an unexpected owner")]
    UnexpectedOwner,
    #[error("snapshot session secret has unsafe permissions")]
    UnsafePermissions,
    #[error("snapshot session secret changed during open")]
    FileIdentityChanged,
    #[error("snapshot session secret has an invalid byte length")]
    InvalidLength,
    #[error("snapshot session secret could not be opened")]
    OpenFailed,
    #[error("snapshot session secret could not be read")]
    ReadFailed,
}

impl SnapshotSecretFileError {
    /// Stable, content-free label suitable for startup logs and metrics.
    #[must_use]
    pub const fn reason(self) -> &'static str {
        match self {
            Self::InvalidPath => "invalid_path",
            Self::UntrustedParent => "untrusted_parent",
            Self::InvalidMetadata => "invalid_metadata",
            Self::NotRegularFile => "not_regular_file",
            Self::InvalidLinkCount => "invalid_link_count",
            Self::UnexpectedOwner => "unexpected_owner",
            Self::UnsafePermissions => "unsafe_permissions",
            Self::FileIdentityChanged => "file_identity_changed",
            Self::InvalidLength => "invalid_length",
            Self::OpenFailed => "open_failed",
            Self::ReadFailed => "read_failed",
        }
    }
}

/// Load an exact raw 256-bit snapshot-session authentication secret.
///
/// This is a synchronous startup operation. It validates every path component,
/// rejects a symlink at any component, opens the target, and compares the
/// opened descriptor's device/inode with both pre-open and post-open path
/// metadata. At most 33 bytes are read, and the temporary read buffer is
/// cleared before returning.
///
/// # Errors
///
/// Returns a content-free [`SnapshotSecretFileError`] for an unsafe path,
/// metadata or identity mismatch, I/O failure, or any length other than exactly
/// 32 raw bytes.
pub fn load_snapshot_session_secret(
    path: &Path,
    policy: SnapshotSecretFilePolicy,
) -> Result<SnapshotSessionSecret, SnapshotSecretFileError> {
    validate_trusted_parents(path, policy)?;

    let before =
        fs::symlink_metadata(path).map_err(|_| SnapshotSecretFileError::InvalidMetadata)?;
    validate_secret_metadata(&before, policy)?;

    let mut file = File::open(path).map_err(|_| SnapshotSecretFileError::OpenFailed)?;
    let opened = file
        .metadata()
        .map_err(|_| SnapshotSecretFileError::InvalidMetadata)?;
    validate_secret_metadata(&opened, policy)?;
    require_same_file(&before, &opened)?;

    let after_open =
        fs::symlink_metadata(path).map_err(|_| SnapshotSecretFileError::InvalidMetadata)?;
    validate_secret_metadata(&after_open, policy)?;
    require_same_file(&opened, &after_open)?;

    let mut raw = [0_u8; MAX_SECRET_READ_BYTES];
    let read_result = read_bounded(&mut file, &mut raw);
    let bytes_read = match read_result {
        Ok(bytes_read) => bytes_read,
        Err(error) => {
            raw.fill(0);
            return Err(error);
        }
    };
    if bytes_read != SNAPSHOT_SESSION_SECRET_BYTES {
        raw.fill(0);
        return Err(SnapshotSecretFileError::InvalidLength);
    }

    let post_read_validation = fs::symlink_metadata(path)
        .map_err(|_| SnapshotSecretFileError::InvalidMetadata)
        .and_then(|after_read| {
            validate_secret_metadata(&after_read, policy)
                .and_then(|()| require_same_file(&opened, &after_read))
        })
        .and_then(|()| validate_trusted_parents(path, policy));
    if let Err(error) = post_read_validation {
        raw.fill(0);
        return Err(error);
    }

    let secret_bytes: [u8; SNAPSHOT_SESSION_SECRET_BYTES] = raw[..SNAPSHOT_SESSION_SECRET_BYTES]
        .try_into()
        .map_err(|_| SnapshotSecretFileError::InvalidLength)?;
    raw.fill(0);
    Ok(SnapshotSessionSecret::new(secret_bytes))
}

/// Load a bounded, protected control-plane file with the same ownership,
/// symlink, link-count, permission, and inode-stability policy as a secret.
///
/// This is used for authenticated incarnation envelopes. It returns bytes only
/// after the post-read path and parent checks succeed.
///
/// # Errors
///
/// Returns a content-free error if `max_bytes` is zero/too large, the file is
/// empty or oversized, or any hardened file invariant fails.
pub fn load_snapshot_control_file(
    path: &Path,
    policy: SnapshotSecretFilePolicy,
    max_bytes: usize,
) -> Result<Vec<u8>, SnapshotSecretFileError> {
    if max_bytes == 0 || max_bytes > MAX_CONTROL_FILE_BYTES {
        return Err(SnapshotSecretFileError::InvalidLength);
    }
    validate_trusted_parents(path, policy)?;
    let before =
        fs::symlink_metadata(path).map_err(|_| SnapshotSecretFileError::InvalidMetadata)?;
    validate_control_metadata(&before, policy, max_bytes)?;
    let mut file = File::open(path).map_err(|_| SnapshotSecretFileError::OpenFailed)?;
    let opened = file
        .metadata()
        .map_err(|_| SnapshotSecretFileError::InvalidMetadata)?;
    validate_control_metadata(&opened, policy, max_bytes)?;
    require_same_file(&before, &opened)?;

    let opened_len = usize::try_from(opened.len()).unwrap_or(max_bytes);
    let mut bytes = Vec::with_capacity(max_bytes.min(opened_len).saturating_add(1));
    file.by_ref()
        .take(
            u64::try_from(max_bytes)
                .unwrap_or(u64::MAX)
                .saturating_add(1),
        )
        .read_to_end(&mut bytes)
        .map_err(|_| SnapshotSecretFileError::ReadFailed)?;
    if bytes.is_empty() || bytes.len() > max_bytes {
        bytes.fill(0);
        return Err(SnapshotSecretFileError::InvalidLength);
    }
    let post_read = fs::symlink_metadata(path)
        .map_err(|_| SnapshotSecretFileError::InvalidMetadata)
        .and_then(|after| {
            validate_control_metadata(&after, policy, max_bytes)
                .and_then(|()| require_same_file(&opened, &after))
        })
        .and_then(|()| validate_trusted_parents(path, policy));
    if let Err(error) = post_read {
        bytes.fill(0);
        return Err(error);
    }
    Ok(bytes)
}

fn read_bounded(
    file: &mut File,
    destination: &mut [u8; MAX_SECRET_READ_BYTES],
) -> Result<usize, SnapshotSecretFileError> {
    let mut filled = 0;
    while filled < destination.len() {
        match file.read(&mut destination[filled..]) {
            Ok(0) => break,
            Ok(count) => filled += count,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return Err(SnapshotSecretFileError::ReadFailed),
        }
    }
    Ok(filled)
}

fn validate_secret_metadata(
    metadata: &Metadata,
    policy: SnapshotSecretFilePolicy,
) -> Result<(), SnapshotSecretFileError> {
    if !metadata.file_type().is_file() {
        return Err(SnapshotSecretFileError::NotRegularFile);
    }
    if metadata.nlink() != 1 {
        return Err(SnapshotSecretFileError::InvalidLinkCount);
    }
    if metadata.uid() != policy.expected_owner_uid {
        return Err(SnapshotSecretFileError::UnexpectedOwner);
    }
    if metadata.mode() & GROUP_OR_WORLD_WRITE != 0 {
        return Err(SnapshotSecretFileError::UnsafePermissions);
    }
    if metadata.len() != u64::try_from(SNAPSHOT_SESSION_SECRET_BYTES).unwrap_or(u64::MAX) {
        return Err(SnapshotSecretFileError::InvalidLength);
    }
    Ok(())
}

fn validate_control_metadata(
    metadata: &Metadata,
    policy: SnapshotSecretFilePolicy,
    max_bytes: usize,
) -> Result<(), SnapshotSecretFileError> {
    if !metadata.file_type().is_file() {
        return Err(SnapshotSecretFileError::NotRegularFile);
    }
    if metadata.nlink() != 1 {
        return Err(SnapshotSecretFileError::InvalidLinkCount);
    }
    if metadata.uid() != policy.expected_owner_uid {
        return Err(SnapshotSecretFileError::UnexpectedOwner);
    }
    if metadata.mode() & GROUP_OR_WORLD_WRITE != 0 {
        return Err(SnapshotSecretFileError::UnsafePermissions);
    }
    if metadata.len() == 0 || metadata.len() > u64::try_from(max_bytes).unwrap_or(u64::MAX) {
        return Err(SnapshotSecretFileError::InvalidLength);
    }
    Ok(())
}

fn require_same_file(left: &Metadata, right: &Metadata) -> Result<(), SnapshotSecretFileError> {
    if left.dev() != right.dev() || left.ino() != right.ino() {
        return Err(SnapshotSecretFileError::FileIdentityChanged);
    }
    Ok(())
}

fn validate_trusted_parents(
    path: &Path,
    policy: SnapshotSecretFilePolicy,
) -> Result<(), SnapshotSecretFileError> {
    if !path.is_absolute() || path.file_name().is_none() {
        return Err(SnapshotSecretFileError::InvalidPath);
    }
    let parent = path.parent().ok_or(SnapshotSecretFileError::InvalidPath)?;
    let mut current = PathBuf::new();
    for component in parent.components() {
        match component {
            Component::RootDir => current.push(Path::new("/")),
            Component::Normal(value) => current.push(value),
            Component::CurDir | Component::ParentDir | Component::Prefix(_) => {
                return Err(SnapshotSecretFileError::InvalidPath);
            }
        }
        let metadata =
            fs::symlink_metadata(&current).map_err(|_| SnapshotSecretFileError::UntrustedParent)?;
        if !metadata.file_type().is_dir() {
            return Err(SnapshotSecretFileError::UntrustedParent);
        }
        let owner = metadata.uid();
        if owner != 0 && owner != policy.expected_owner_uid {
            return Err(SnapshotSecretFileError::UntrustedParent);
        }
        if metadata.mode() & GROUP_OR_WORLD_WRITE != 0 {
            let trusted_sticky_root = owner == 0 && metadata.mode() & STICKY_BIT != 0;
            if current == parent || !trusted_sticky_root {
                return Err(SnapshotSecretFileError::UntrustedParent);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{
        os::unix::fs::{PermissionsExt, symlink},
        sync::atomic::{AtomicU64, Ordering},
    };

    use super::*;

    const SECRET: [u8; SNAPSHOT_SESSION_SECRET_BYTES] = *b"snapshot-session-secret-32-byte!";
    static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(1);

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let suffix = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "mini-dynamo-secret-loader-{}-{suffix}",
                std::process::id()
            ));
            fs::create_dir(&path).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
            Self(path)
        }

        fn path(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn write_secret(path: &Path, bytes: &[u8]) {
        fs::write(path, bytes).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
    }

    fn policy(path: &Path) -> SnapshotSecretFilePolicy {
        let metadata = fs::metadata(path.parent().unwrap()).unwrap();
        SnapshotSecretFilePolicy {
            expected_owner_uid: metadata.uid(),
        }
    }

    #[test]
    fn exact_raw_secret_loads_and_debug_is_redacted() {
        let directory = TestDirectory::new();
        let path = directory.path("secret");
        write_secret(&path, &SECRET);
        let secret = load_snapshot_session_secret(&path, policy(&path)).unwrap();
        assert_eq!(format!("{secret:?}"), "SnapshotSessionSecret([REDACTED])");
    }

    #[test]
    fn rejects_short_long_newline_and_hex_encodings() {
        for (name, bytes) in [
            ("short", vec![7; 31]),
            ("long", vec![7; 33]),
            ("newline", [SECRET.as_slice(), b"\n"].concat()),
            ("hex", vec![b'a'; 64]),
        ] {
            let directory = TestDirectory::new();
            let path = directory.path(name);
            write_secret(&path, &bytes);
            assert_eq!(
                load_snapshot_session_secret(&path, policy(&path)).unwrap_err(),
                SnapshotSecretFileError::InvalidLength
            );
        }
    }

    #[test]
    fn rejects_target_and_parent_symlinks() {
        let directory = TestDirectory::new();
        let target = directory.path("target");
        let link = directory.path("link");
        write_secret(&target, &SECRET);
        symlink(&target, &link).unwrap();
        assert_eq!(
            load_snapshot_session_secret(&link, policy(&link)).unwrap_err(),
            SnapshotSecretFileError::NotRegularFile
        );

        let real_parent = directory.path("real-parent");
        fs::create_dir(&real_parent).unwrap();
        fs::set_permissions(&real_parent, fs::Permissions::from_mode(0o700)).unwrap();
        let nested = real_parent.join("secret");
        write_secret(&nested, &SECRET);
        let linked_parent = directory.path("linked-parent");
        symlink(&real_parent, &linked_parent).unwrap();
        let linked_path = linked_parent.join("secret");
        assert_eq!(
            load_snapshot_session_secret(&linked_path, policy(&nested)).unwrap_err(),
            SnapshotSecretFileError::UntrustedParent
        );
    }

    #[test]
    fn rejects_group_or_world_writable_file_and_parent() {
        let directory = TestDirectory::new();
        let path = directory.path("secret");
        write_secret(&path, &SECRET);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o620)).unwrap();
        assert_eq!(
            load_snapshot_session_secret(&path, policy(&path)).unwrap_err(),
            SnapshotSecretFileError::UnsafePermissions
        );

        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        fs::set_permissions(&directory.0, fs::Permissions::from_mode(0o720)).unwrap();
        assert_eq!(
            load_snapshot_session_secret(&path, policy(&path)).unwrap_err(),
            SnapshotSecretFileError::UntrustedParent
        );
    }

    #[test]
    fn rejects_hard_link_and_non_regular_target() {
        let directory = TestDirectory::new();
        let path = directory.path("secret");
        let hard_link = directory.path("hard-link");
        write_secret(&path, &SECRET);
        fs::hard_link(&path, &hard_link).unwrap();
        assert_eq!(
            load_snapshot_session_secret(&path, policy(&path)).unwrap_err(),
            SnapshotSecretFileError::InvalidLinkCount
        );
        let directory_target = directory.path("directory-target");
        fs::create_dir(&directory_target).unwrap();
        assert_eq!(
            load_snapshot_session_secret(&directory_target, policy(&path)).unwrap_err(),
            SnapshotSecretFileError::NotRegularFile
        );
    }

    #[test]
    fn rejects_unexpected_owner_policy_and_relative_path() {
        let directory = TestDirectory::new();
        let path = directory.path("secret");
        write_secret(&path, &SECRET);
        let actual = policy(&path).expected_owner_uid;
        assert!(matches!(
            load_snapshot_session_secret(
                &path,
                SnapshotSecretFilePolicy {
                    expected_owner_uid: actual.wrapping_add(1),
                },
            )
            .unwrap_err(),
            SnapshotSecretFileError::UntrustedParent | SnapshotSecretFileError::UnexpectedOwner
        ));
        assert_eq!(
            load_snapshot_session_secret(Path::new("relative-secret"), policy(&path)).unwrap_err(),
            SnapshotSecretFileError::InvalidPath
        );
    }

    #[test]
    fn errors_are_content_free() {
        let error = SnapshotSecretFileError::OpenFailed;
        assert_eq!(format!("{error:?}"), "OpenFailed");
        assert_eq!(
            error.to_string(),
            "snapshot session secret could not be opened"
        );
        assert_eq!(error.reason(), "open_failed");
    }
}
