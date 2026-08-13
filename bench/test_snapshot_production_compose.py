import importlib.util
import pathlib
import tempfile
import unittest


ROOT = pathlib.Path(__file__).resolve().parents[1]
VALIDATOR = (
    ROOT
    / "deploy"
    / "dspark_0731"
    / "validate-snapshot-production-compose.py"
)
SPEC = importlib.util.spec_from_file_location("snapshot_production_validator", VALIDATOR)
validator = importlib.util.module_from_spec(SPEC)
assert SPEC.loader is not None
SPEC.loader.exec_module(validator)


class SnapshotProductionComposeTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.companion = validator.render(companion=True, attestation=False)
        cls.full = validator.render(companion=True, attestation=True)

    def test_real_production_overlay_is_valid(self):
        validator.validate_source_bind_policy()
        validator.validate_documents(self.companion, self.full)
        validator.validate_caddy()

    def test_explicit_shadow_render_is_valid_and_still_disables_raw_events(self):
        companion = validator.render(
            companion=True, attestation=False, route_mode="shadow"
        )
        full = validator.render(companion=True, attestation=True, route_mode="shadow")
        validator.validate_documents(companion, full, route_mode="shadow")
        environment = companion["services"]["ds4-loadbalancer"]["environment"]
        self.assertEqual(environment["DS4_SNAPSHOT_ROUTE_MODE"], "shadow")
        self.assertEqual(environment["DS4_KV_EVENT_MODE"], "off")

    def test_companion_profile_does_not_run_privileged_provisioners(self):
        services = self.companion["services"]
        self.assertIn("snapshot-companion-a", services)
        self.assertIn("snapshot-companion-b", services)
        self.assertNotIn("snapshot-attestation-a", services)
        self.assertNotIn("snapshot-attestation-b", services)

    def test_peer_authority_mount_is_rejected(self):
        document = validator.render(companion=True, attestation=True)
        service = document["services"]["snapshot-companion-a"]
        mount = validator.volume_by_target(service, "/run/secrets/snapshot-digest")
        mount["source"] = validator.DOMAINS["engine-b"]["digest_source"]
        with self.assertRaises(validator.ValidationError):
            validator.validate_documents(self.companion, document)

    def test_tcp_metrics_and_session_group_are_rejected(self):
        document = validator.render(companion=True, attestation=False)
        service = document["services"]["snapshot-companion-a"]
        service["environment"]["DS4_SNAPSHOT_METRICS_BIND"] = "127.0.0.1:9091"
        with self.assertRaisesRegex(validator.ValidationError, "TCP metrics"):
            validator.validate_documents(document, self.full)

        document = validator.render(companion=True, attestation=False)
        service = document["services"]["snapshot-companion-a"]
        service["environment"]["DS4_SNAPSHOT_METRICS_GROUP_GID"] = validator.SESSION_GID
        with self.assertRaises(validator.ValidationError):
            validator.validate_documents(document, self.full)

    def test_companion_gpu_or_docker_socket_is_rejected(self):
        document = validator.render(companion=True, attestation=False)
        service = document["services"]["snapshot-companion-a"]
        service["gpus"] = "all"
        with self.assertRaisesRegex(validator.ValidationError, "GPU"):
            validator.validate_documents(document, self.full)

        document = validator.render(companion=True, attestation=False)
        service = document["services"]["snapshot-companion-a"]
        service["volumes"].append(
            {
                "type": "bind",
                "source": "/var/run/docker.sock",
                "target": "/var/run/docker.sock",
                "bind": {"create_host_path": False},
            }
        )
        with self.assertRaisesRegex(validator.ValidationError, "privileged host mount"):
            validator.validate_documents(document, self.full)

    def test_raw_kv_and_snapshot_authority_cannot_coexist(self):
        document = validator.render(companion=True, attestation=False)
        document["services"]["ds4-loadbalancer"]["environment"][
            "DS4_KV_EVENT_MODE"
        ] = "shadow"
        with self.assertRaisesRegex(validator.ValidationError, "DS4_KV_EVENT_MODE"):
            validator.validate_documents(document, self.full)

    def test_caddy_can_only_reach_metrics_sockets(self):
        original = validator.CADDY.read_text(encoding="utf-8")
        with tempfile.TemporaryDirectory() as directory:
            candidate = pathlib.Path(directory) / "Caddyfile"
            candidate.write_text(
                original.replace(
                    "/run/mini-dynamo-snapshot-metrics-a/metrics.sock",
                    "/run/secrets/snapshot-session-a",
                ),
                encoding="utf-8",
            )
            with self.assertRaises(validator.ValidationError):
                validator.validate_caddy(candidate)


if __name__ == "__main__":
    unittest.main()
