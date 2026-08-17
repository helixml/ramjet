use std::{
    fs,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::PathBuf,
    process::Command,
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use ramjet::{
    companion_attestation::{load_authenticated_engine_incarnation, load_companion_digest_secret},
    snapshot_secret_file::SnapshotSecretFilePolicy,
};

static TEST_ID: AtomicU64 = AtomicU64::new(1);

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new() -> Self {
        let id = TEST_ID.fetch_add(1, Ordering::Relaxed);
        let path =
            PathBuf::from("/tmp").join(format!("md-attestation-cli-{}-{id}", std::process::id()));
        fs::create_dir(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        Self(path)
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn command_line_rejects_all_content_arguments() {
    let output = Command::new(env!("CARGO_BIN_EXE_ramjet-attestation-provisioner"))
        .arg("secret-or-identity-content")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert_eq!(
        stderr,
        "engine attestation provisioning failed: invalid_arguments\n"
    );
    assert!(!stderr.contains("secret-or-identity-content"));
}

#[test]
fn missing_environment_is_content_free() {
    let output = Command::new(env!("CARGO_BIN_EXE_ramjet-attestation-provisioner"))
        .env_clear()
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        "engine attestation provisioning failed: missing_setting\n"
    );
}

#[test]
fn fresh_explicit_metadata_provisions_authenticated_output_silently() {
    let directory = TestDirectory::new();
    let metadata_path = directory.0.join("metadata.json");
    let secret_path = directory.0.join("digest-secret");
    let output_path = directory.0.join("attestation.json");
    let identity = fs::metadata(&directory.0).unwrap();
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
    let now_ns = u64::try_from(now.as_nanos()).unwrap();
    let captured = chrono::DateTime::from_timestamp(
        i64::try_from(now_ns / 1_000_000_000).unwrap(),
        u32::try_from(now_ns % 1_000_000_000).unwrap(),
    )
    .unwrap()
    .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
    let started_ns = now_ns - 10_000_000_000;
    let started = chrono::DateTime::from_timestamp(
        i64::try_from(started_ns / 1_000_000_000).unwrap(),
        u32::try_from(started_ns % 1_000_000_000).unwrap(),
    )
    .unwrap()
    .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
    let metadata = serde_json::json!({
        "schema_version": 1,
        "live": {
            "captured_utc": captured,
            "container": "engine-a",
            "configured_image": "example.invalid/engine:r34",
            "image_id": format!("sha256:{}", "1".repeat(64)),
            "image_descriptor_digest": "",
            "image_config_digest": "",
            "repo_digests": [],
            "model_revision": "model-revision",
            "tokenizer_revision": "tokenizer-revision",
            "tokenizer_sha256": "2".repeat(64),
            "config_sha256": "3".repeat(64),
            "driver": "595.84",
            "topology_sha256": "4".repeat(64),
            "started_at": started,
            "process_started_unix_ns": started_ns,
            "restart_count": 0,
            "cpuset_cpus": "0-11,24-35",
            "cpuset_mems": "",
            "runtime_packages": {"vllm": "0.13.1"},
            "argv_sha256": "5".repeat(64),
            "effective_contract": {"tensor_parallel_size": "4"}
        },
        "receipt": null,
        "verified": null
    });
    fs::write(&metadata_path, serde_json::to_vec(&metadata).unwrap()).unwrap();
    fs::set_permissions(&metadata_path, fs::Permissions::from_mode(0o600)).unwrap();
    fs::write(&secret_path, [0x51; 32]).unwrap();
    fs::set_permissions(&secret_path, fs::Permissions::from_mode(0o600)).unwrap();

    for _ in 0..2 {
        let output = Command::new(env!("CARGO_BIN_EXE_ramjet-attestation-provisioner"))
            .env_clear()
            .env("RJ_SNAPSHOT_ENGINE_METADATA_PATH", &metadata_path)
            .env("RJ_SNAPSHOT_DIGEST_SECRET_PATH", &secret_path)
            .env("RJ_SNAPSHOT_ATTESTATION_PATH", &output_path)
            .env("RJ_SNAPSHOT_SECRET_OWNER_UID", identity.uid().to_string())
            .env("RJ_SNAPSHOT_SECRET_GROUP_GID", identity.gid().to_string())
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        assert!(output.stdout.is_empty());
        assert!(output.stderr.is_empty());
    }

    let output_metadata = fs::metadata(&output_path).unwrap();
    assert_eq!(output_metadata.mode() & 0o777, 0o440);
    assert_eq!(output_metadata.uid(), identity.uid());
    assert_eq!(output_metadata.gid(), identity.gid());
    let policy = SnapshotSecretFilePolicy {
        expected_owner_uid: identity.uid(),
    };
    let secret = load_companion_digest_secret(&secret_path, policy).unwrap();
    let incarnation = load_authenticated_engine_incarnation(&output_path, policy, &secret).unwrap();
    assert_eq!(incarnation.engine_id, "engine-a");
    assert_eq!(incarnation.process_started_unix_ns, started_ns);
}
