//! Safe filesystem lifecycle for the snapshot companion's Unix listener.
//!
//! The public socket is created under a companion-owned directory that other
//! processes cannot modify. A listener is first bound at a unique private
//! pathname, assigned its final permissions, and then atomically published
//! with a hard link. Unlike `rename(2)`, hard-link publication cannot replace
//! an existing target. Cleanup removes a pathname only while it still names
//! the socket inode created by this module.
//!
//! The inode check and unlink are separate standard-library operations. Their
//! safety therefore relies on the validated parent remaining exclusively
//! companion-writable for the lifetime of the guard. Do not relax that
//! deployment invariant or share the companion UID with another writer.

use std::{
    fs, io,
    os::unix::{
        fs::{FileTypeExt, MetadataExt, PermissionsExt},
        net::UnixListener,
    },
    path::{Component, Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use thiserror::Error;

const SOCKET_MODE: u32 = 0o660;
const UNSAFE_PARENT_WRITE_BITS: u32 = 0o022;
const TEMP_BIND_ATTEMPTS: usize = 64;
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SocketParentPolicy {
    /// Numeric UID of the companion process and trusted directory owner.
    pub owner_uid: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SocketIdentity {
    device: u64,
    inode: u64,
}

impl SocketIdentity {
    #[must_use]
    pub const fn device(self) -> u64 {
        self.device
    }

    #[must_use]
    pub const fn inode(self) -> u64 {
        self.inode
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CleanupOutcome {
    Removed,
    Missing,
    PreservedDifferentEntry,
}

#[derive(Debug, Error)]
pub enum SnapshotSocketPathError {
    #[error("snapshot socket path must be absolute and normalized")]
    InvalidPath,
    #[error("snapshot socket parent metadata is unavailable")]
    ParentMetadata,
    #[error("snapshot socket parent contains a symbolic link")]
    ParentSymlink,
    #[error("snapshot socket parent is not a directory")]
    ParentNotDirectory,
    #[error("snapshot socket parent owner does not match companion UID")]
    ParentOwnerMismatch,
    #[error("snapshot socket parent is writable by group or other")]
    UnsafeParentPermissions,
    #[error("snapshot socket public target already exists")]
    TargetExists,
    #[error("snapshot socket temporary bind failed")]
    BindFailed,
    #[error("snapshot socket permission update failed")]
    PermissionFailed,
    #[error("snapshot socket metadata is invalid")]
    InvalidSocketMetadata,
    #[error("snapshot socket atomic publication failed")]
    PublishFailed,
    #[error("snapshot socket temporary cleanup failed")]
    TemporaryCleanupFailed,
}

impl SnapshotSocketPathError {
    #[must_use]
    pub const fn reason(&self) -> &'static str {
        match self {
            Self::InvalidPath => "invalid_path",
            Self::ParentMetadata => "parent_metadata",
            Self::ParentSymlink => "parent_symlink",
            Self::ParentNotDirectory => "parent_not_directory",
            Self::ParentOwnerMismatch => "parent_owner_mismatch",
            Self::UnsafeParentPermissions => "unsafe_parent_permissions",
            Self::TargetExists => "target_exists",
            Self::BindFailed => "bind_failed",
            Self::PermissionFailed => "permission_failed",
            Self::InvalidSocketMetadata => "invalid_socket_metadata",
            Self::PublishFailed => "publish_failed",
            Self::TemporaryCleanupFailed => "temporary_cleanup_failed",
        }
    }
}

/// A published listener plus an inode-checked pathname guard.
pub struct PublishedUnixListener {
    listener: UnixListener,
    path_guard: PublishedSocketPath,
}

impl PublishedUnixListener {
    #[must_use]
    pub const fn listener(&self) -> &UnixListener {
        &self.listener
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        self.path_guard.path()
    }

    #[must_use]
    pub const fn identity(&self) -> SocketIdentity {
        self.path_guard.identity()
    }

    /// Split the standard listener from its pathname guard. The caller must
    /// retain the guard for as long as new clients should be able to connect.
    #[must_use]
    pub fn into_parts(self) -> (UnixListener, PublishedSocketPath) {
        (self.listener, self.path_guard)
    }
}

/// Keeps the public pathname alive and removes only the originally published
/// socket inode when dropped.
pub struct PublishedSocketPath {
    path: PathBuf,
    identity: SocketIdentity,
    armed: bool,
}

impl PublishedSocketPath {
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    #[must_use]
    pub const fn identity(&self) -> SocketIdentity {
        self.identity
    }

    /// Remove the published path if it still identifies this guard's socket.
    ///
    /// # Errors
    ///
    /// Returns an I/O error when pathname metadata cannot be inspected or the
    /// matching socket cannot be removed.
    pub fn cleanup(&mut self) -> io::Result<CleanupOutcome> {
        let outcome = remove_same_socket(&self.path, self.identity)?;
        self.armed = false;
        Ok(outcome)
    }
}

impl Drop for PublishedSocketPath {
    fn drop(&mut self) {
        if self.armed {
            let _ = remove_same_socket(&self.path, self.identity);
        }
    }
}

/// Validate that `parent` is an absolute, symlink-free directory owned by the
/// companion UID and not writable by group or other.
///
/// # Errors
///
/// Returns [`SnapshotSocketPathError`] for missing, linked, wrongly owned,
/// non-directory, writable, relative, or non-normalized parent paths.
pub fn validate_socket_parent(
    parent: &Path,
    policy: SocketParentPolicy,
) -> Result<(), SnapshotSocketPathError> {
    if !parent.is_absolute() {
        return Err(SnapshotSocketPathError::InvalidPath);
    }

    let mut current = PathBuf::new();
    for component in parent.components() {
        match component {
            Component::RootDir => current.push(component.as_os_str()),
            Component::Normal(value) => current.push(value),
            Component::CurDir | Component::ParentDir | Component::Prefix(_) => {
                return Err(SnapshotSocketPathError::InvalidPath);
            }
        }
        let metadata =
            fs::symlink_metadata(&current).map_err(|_| SnapshotSocketPathError::ParentMetadata)?;
        if metadata.file_type().is_symlink() {
            return Err(SnapshotSocketPathError::ParentSymlink);
        }
    }

    let metadata =
        fs::symlink_metadata(parent).map_err(|_| SnapshotSocketPathError::ParentMetadata)?;
    if !metadata.file_type().is_dir() {
        return Err(SnapshotSocketPathError::ParentNotDirectory);
    }
    if metadata.uid() != policy.owner_uid {
        return Err(SnapshotSocketPathError::ParentOwnerMismatch);
    }
    if metadata.mode() & UNSAFE_PARENT_WRITE_BITS != 0 {
        return Err(SnapshotSocketPathError::UnsafeParentPermissions);
    }
    Ok(())
}

/// Bind a unique private socket, set mode `0660`, and atomically publish it
/// without replacing any existing public target.
///
/// # Errors
///
/// Returns [`SnapshotSocketPathError`] if parent policy, binding, permission,
/// metadata, atomic publication, or private-path cleanup fails.
pub fn bind_and_publish(
    public_path: &Path,
    policy: SocketParentPolicy,
) -> Result<PublishedUnixListener, SnapshotSocketPathError> {
    let parent = normalized_parent(public_path)?;
    validate_socket_parent(parent, policy)?;

    match fs::symlink_metadata(public_path) {
        Ok(_) => return Err(SnapshotSocketPathError::TargetExists),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(_) => return Err(SnapshotSocketPathError::ParentMetadata),
    }

    let (listener, temporary_path, identity) = bind_unique_temporary(parent)?;
    let mut temporary_guard = PublishedSocketPath {
        path: temporary_path,
        identity,
        armed: true,
    };

    fs::set_permissions(
        temporary_guard.path(),
        fs::Permissions::from_mode(SOCKET_MODE),
    )
    .map_err(|_| SnapshotSocketPathError::PermissionFailed)?;
    let after_chmod = socket_identity(temporary_guard.path())?;
    if after_chmod != identity {
        return Err(SnapshotSocketPathError::InvalidSocketMetadata);
    }

    match fs::hard_link(temporary_guard.path(), public_path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            return Err(SnapshotSocketPathError::TargetExists);
        }
        Err(_) => return Err(SnapshotSocketPathError::PublishFailed),
    }

    let mut public_guard = PublishedSocketPath {
        path: public_path.to_path_buf(),
        identity,
        armed: true,
    };
    let published_identity = match socket_identity(public_path) {
        Ok(identity) => identity,
        Err(error) => {
            let _ = public_guard.cleanup();
            return Err(error);
        }
    };
    if published_identity != identity {
        let _ = public_guard.cleanup();
        return Err(SnapshotSocketPathError::InvalidSocketMetadata);
    }
    if temporary_guard
        .cleanup()
        .map_err(|_| SnapshotSocketPathError::TemporaryCleanupFailed)?
        != CleanupOutcome::Removed
    {
        let _ = public_guard.cleanup();
        return Err(SnapshotSocketPathError::TemporaryCleanupFailed);
    }

    Ok(PublishedUnixListener {
        listener,
        path_guard: public_guard,
    })
}

/// Remove `path` only if it still names the expected Unix socket inode.
///
/// # Errors
///
/// Returns an I/O error when pathname metadata cannot be inspected or the
/// matching socket cannot be removed.
pub fn remove_same_socket(path: &Path, expected: SocketIdentity) -> io::Result<CleanupOutcome> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(CleanupOutcome::Missing);
        }
        Err(error) => return Err(error),
    };
    if !metadata.file_type().is_socket()
        || metadata.dev() != expected.device
        || metadata.ino() != expected.inode
    {
        return Ok(CleanupOutcome::PreservedDifferentEntry);
    }
    fs::remove_file(path)?;
    Ok(CleanupOutcome::Removed)
}

fn normalized_parent(public_path: &Path) -> Result<&Path, SnapshotSocketPathError> {
    if !public_path.is_absolute()
        || public_path.file_name().is_none()
        || public_path
            .components()
            .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
    {
        return Err(SnapshotSocketPathError::InvalidPath);
    }
    public_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or(SnapshotSocketPathError::InvalidPath)
}

fn bind_unique_temporary(
    parent: &Path,
) -> Result<(UnixListener, PathBuf, SocketIdentity), SnapshotSocketPathError> {
    for _ in 0..TEMP_BIND_ATTEMPTS {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let temporary_path = parent.join(format!(".mds-{:x}-{sequence:x}", std::process::id()));
        match UnixListener::bind(&temporary_path) {
            Ok(listener) => {
                let identity = match socket_identity(&temporary_path) {
                    Ok(identity) => identity,
                    Err(error) => {
                        drop(listener);
                        let _ = fs::remove_file(&temporary_path);
                        return Err(error);
                    }
                };
                return Ok((listener, temporary_path, identity));
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::AddrInUse | io::ErrorKind::AlreadyExists
                ) => {}
            Err(_) => return Err(SnapshotSocketPathError::BindFailed),
        }
    }
    Err(SnapshotSocketPathError::BindFailed)
}

fn socket_identity(path: &Path) -> Result<SocketIdentity, SnapshotSocketPathError> {
    let metadata =
        fs::symlink_metadata(path).map_err(|_| SnapshotSocketPathError::InvalidSocketMetadata)?;
    if !metadata.file_type().is_socket() {
        return Err(SnapshotSocketPathError::InvalidSocketMetadata);
    }
    Ok(SocketIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

#[cfg(test)]
mod tests {
    use std::{
        os::unix::{fs::symlink, net::UnixStream},
        sync::atomic::{AtomicU64, Ordering},
    };

    use super::*;

    static TEST_SEQUENCE: AtomicU64 = AtomicU64::new(1);

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let sequence = TEST_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("mdsp-{:x}-{sequence:x}", std::process::id()));
            fs::create_dir_all(&path).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }

        fn policy(&self) -> SocketParentPolicy {
            SocketParentPolicy {
                owner_uid: fs::symlink_metadata(&self.0).unwrap().uid(),
            }
        }

        fn assert_no_private_socket(&self) {
            let has_private = fs::read_dir(&self.0).unwrap().any(|entry| {
                entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".mds-")
            });
            assert!(!has_private);
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn publishes_real_connectable_socket_with_exact_mode() {
        let directory = TestDirectory::new();
        let path = directory.path().join("companion.sock");
        let published = bind_and_publish(&path, directory.policy()).unwrap();

        let metadata = fs::symlink_metadata(&path).unwrap();
        assert!(metadata.file_type().is_socket());
        assert_eq!(metadata.mode() & 0o777, SOCKET_MODE);
        assert_eq!(metadata.dev(), published.identity().device());
        assert_eq!(metadata.ino(), published.identity().inode());

        let client = UnixStream::connect(&path).unwrap();
        let _server = published.listener().accept().unwrap().0;
        directory.assert_no_private_socket();
        drop(client);
        drop(published);
        assert!(!path.exists());
    }

    #[test]
    fn refuses_and_preserves_every_preexisting_target_kind() {
        let directory = TestDirectory::new();
        for name in ["file", "directory", "symlink", "socket"] {
            let path = directory.path().join(name);
            match name {
                "file" => fs::write(&path, b"preserve").unwrap(),
                "directory" => fs::create_dir(&path).unwrap(),
                "symlink" => symlink("missing-target", &path).unwrap(),
                "socket" => {
                    let _listener = UnixListener::bind(&path).unwrap();
                    assert!(matches!(
                        bind_and_publish(&path, directory.policy()),
                        Err(SnapshotSocketPathError::TargetExists)
                    ));
                    assert!(fs::symlink_metadata(&path).unwrap().file_type().is_socket());
                    continue;
                }
                _ => unreachable!(),
            }
            assert!(matches!(
                bind_and_publish(&path, directory.policy()),
                Err(SnapshotSocketPathError::TargetExists)
            ));
            assert!(fs::symlink_metadata(&path).is_ok());
        }
        assert_eq!(
            fs::read(directory.path().join("file")).unwrap(),
            b"preserve"
        );
        directory.assert_no_private_socket();
    }

    #[test]
    fn rejects_symlink_non_directory_and_unsafe_writable_parent() {
        let directory = TestDirectory::new();
        let linked_parent = directory.path().with_extension("link");
        symlink(directory.path(), &linked_parent).unwrap();
        assert!(matches!(
            bind_and_publish(&linked_parent.join("socket"), directory.policy()),
            Err(SnapshotSocketPathError::ParentSymlink)
        ));
        fs::remove_file(&linked_parent).unwrap();

        let file_parent = directory.path().join("not-a-directory");
        fs::write(&file_parent, b"file").unwrap();
        assert!(matches!(
            bind_and_publish(&file_parent.join("socket"), directory.policy()),
            Err(SnapshotSocketPathError::ParentNotDirectory)
        ));

        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o720)).unwrap();
        assert!(matches!(
            bind_and_publish(&directory.path().join("socket"), directory.policy()),
            Err(SnapshotSocketPathError::UnsafeParentPermissions)
        ));
    }

    #[test]
    fn rejects_wrong_parent_owner_policy_and_relative_paths() {
        let directory = TestDirectory::new();
        let owner = directory.policy().owner_uid;
        assert!(matches!(
            validate_socket_parent(
                directory.path(),
                SocketParentPolicy {
                    owner_uid: owner.wrapping_add(1),
                },
            ),
            Err(SnapshotSocketPathError::ParentOwnerMismatch)
        ));
        assert!(matches!(
            bind_and_publish(Path::new("relative.sock"), directory.policy()),
            Err(SnapshotSocketPathError::InvalidPath)
        ));
    }

    #[test]
    fn dropping_old_guard_preserves_replacement_inode() {
        let directory = TestDirectory::new();
        let path = directory.path().join("companion.sock");
        let published = bind_and_publish(&path, directory.policy()).unwrap();
        let old_identity = published.identity();

        fs::remove_file(&path).unwrap();
        let replacement = UnixListener::bind(&path).unwrap();
        let replacement_metadata = fs::symlink_metadata(&path).unwrap();
        assert_ne!(replacement_metadata.ino(), old_identity.inode());

        drop(published);
        let preserved = fs::symlink_metadata(&path).unwrap();
        assert_eq!(preserved.ino(), replacement_metadata.ino());
        drop(replacement);
    }

    #[test]
    fn explicit_cleanup_is_idempotent_and_inode_checked() {
        let directory = TestDirectory::new();
        let path = directory.path().join("companion.sock");
        let published = bind_and_publish(&path, directory.policy()).unwrap();
        let (_listener, mut guard) = published.into_parts();
        assert_eq!(guard.cleanup().unwrap(), CleanupOutcome::Removed);
        assert_eq!(guard.cleanup().unwrap(), CleanupOutcome::Missing);
    }
}
