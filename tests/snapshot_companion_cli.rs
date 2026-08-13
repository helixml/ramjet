use std::{
    fs,
    os::unix::{fs::PermissionsExt, net::UnixListener},
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::atomic::{AtomicU64, Ordering},
};

static TEST_SEQUENCE: AtomicU64 = AtomicU64::new(1);

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new() -> Self {
        let sequence = TEST_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "md-companion-cli-{:x}-{sequence:x}",
            std::process::id()
        ));
        fs::create_dir(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn companion(arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_mini-dynamo-snapshot-companion"))
        .args(arguments)
        .env_remove("DS4_SNAPSHOT_COMPANION_MODE")
        .output()
        .unwrap()
}

#[test]
fn healthcheck_accepts_only_exact_published_socket_metadata() {
    let directory = TestDirectory::new();
    let socket = directory.path().join("companion.sock");
    let _listener = UnixListener::bind(&socket).unwrap();
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o660)).unwrap();

    let output = companion(&["healthcheck", socket.to_str().unwrap()]);
    assert!(output.status.success(), "{output:?}");
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
}

#[test]
fn healthcheck_rejects_missing_path_and_extra_arguments() {
    let directory = TestDirectory::new();
    let missing = directory.path().join("missing.sock");
    assert!(
        !companion(&["healthcheck", missing.to_str().unwrap()])
            .status
            .success()
    );
    assert!(!companion(&["healthcheck"]).status.success());
    assert!(
        !companion(&["healthcheck", "/socket", "extra"])
            .status
            .success()
    );
}

#[test]
fn fixture_and_unknown_subcommands_fail_closed() {
    assert!(!companion(&["snapshot-companion-fixture"]).status.success());
    assert!(!companion(&["unknown"]).status.success());
}

#[test]
fn no_arguments_preserves_default_off_mode() {
    let output = companion(&[]);
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("\"enabled\":false"));
    assert!(stdout.contains("\"report\":\"Off\""));
}
