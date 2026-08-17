import importlib.util
import pathlib
import shutil
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


@unittest.skipUnless(shutil.which("docker"), "Docker Compose is validated in the deployment lane")
class SnapshotProductionComposeTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.companion = validator.render(companion=True, attestation=False)
        cls.full = validator.render(companion=True, attestation=True)

    def test_real_production_overlay_is_valid(self):
        validator.validate_source_bind_policy()
        validator.validate_source_bind_policy(validator.LB_OVERLAY)
        validator.validate_documents(self.companion, self.full)
        validator.validate_caddy()

    def test_serving_path_has_no_tmpfs_mounts_without_the_lb_overlay(self):
        # The #156 recurrence guard: a reboot wipes /run, so the serving LB
        # must be creatable with none of that state present.
        validator.validate_serving_path_isolation()
        for document in (
            validator.render(companion=False, attestation=False, lb_overlay=False),
            validator.render(companion=True, attestation=True, lb_overlay=False),
        ):
            service = document["services"]["ds4-loadbalancer"]
            self.assertEqual(validator.tmpfs_bind_sources(service), [])

    def test_companion_overlay_alone_leaves_the_load_balancer_untouched(self):
        base = validator.render(companion=False, attestation=False, lb_overlay=False)
        with_companions = validator.render(
            companion=True, attestation=True, lb_overlay=False
        )
        self.assertEqual(
            base["services"]["ds4-loadbalancer"],
            with_companions["services"]["ds4-loadbalancer"],
        )
        self.assertIn("snapshot-companion-a", with_companions["services"])

    def test_unguarded_serving_tmpfs_mount_is_rejected(self):
        document = validator.render(companion=False, attestation=False, lb_overlay=False)
        document["services"]["ds4-loadbalancer"].setdefault("volumes", []).append(
            {
                "type": "bind",
                "source": "/run/mini-dynamo-not-provisioned",
                "target": "/run/mini-dynamo-not-provisioned",
                "read_only": True,
                "bind": {"create_host_path": False},
            }
        )
        with self.assertRaisesRegex(validator.ValidationError, "no boot-time unit"):
            validator.validate_boot_authority(document)

    def test_every_lb_authority_mount_is_boot_provisioned(self):
        # With the LB overlay applied the mounts are legitimate, but each one
        # must have a tmpfiles.d parent behind it.
        validator.validate_boot_authority(self.companion)
        service = self.companion["services"]["ds4-loadbalancer"]
        self.assertTrue(validator.tmpfs_bind_sources(service))

    def test_authority_unit_orders_but_does_not_gate_docker(self):
        directives = validator.unit_directives()
        self.assertIn("docker.service", directives["Before"])
        self.assertIn("docker.service", directives["WantedBy"])
        # RequiredBy would make provisioner failure block serving. The unit
        # explains that in prose, so assert on directives rather than text.
        self.assertNotIn("docker.service", directives.get("RequiredBy", set()))

    def test_missing_boot_provisioner_is_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            empty = pathlib.Path(directory) / "absent.conf"
            with self.assertRaisesRegex(validator.ValidationError, "boot-time tmpfiles"):
                validator.boot_provisioned_paths(empty)

    def test_explicit_shadow_render_is_valid_and_still_disables_raw_events(self):
        companion = validator.render(
            companion=True, attestation=False, route_mode="shadow"
        )
        full = validator.render(companion=True, attestation=True, route_mode="shadow")
        validator.validate_documents(companion, full, route_mode="shadow")
        environment = companion["services"]["ds4-loadbalancer"]["environment"]
        self.assertEqual(environment["RJ_EXACT_ROUTE_MODE"], "shadow")
        self.assertEqual(environment["RJ_SNAPSHOT_ROUTE_MODE"], "shadow")
        self.assertEqual(environment["RJ_KV_EVENT_MODE"], "off")

    def test_off_render_disables_router_and_snapshot_exact_modes_together(self):
        environment = self.companion["services"]["ds4-loadbalancer"]["environment"]
        self.assertEqual(environment["RJ_EXACT_ROUTE_MODE"], "off")
        self.assertEqual(environment["RJ_SNAPSHOT_ROUTE_MODE"], "off")
        self.assertEqual(environment["RJ_KV_EVENT_MODE"], "off")

    def test_router_and_snapshot_exact_modes_cannot_diverge(self):
        document = validator.render(companion=True, attestation=False)
        document["services"]["ds4-loadbalancer"]["environment"][
            "RJ_EXACT_ROUTE_MODE"
        ] = "shadow"
        with self.assertRaisesRegex(validator.ValidationError, "RJ_EXACT_ROUTE_MODE"):
            validator.validate_documents(document, self.full)

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
        service["environment"]["RJ_SNAPSHOT_METRICS_BIND"] = "127.0.0.1:9091"
        with self.assertRaisesRegex(validator.ValidationError, "TCP metrics"):
            validator.validate_documents(document, self.full)

        document = validator.render(companion=True, attestation=False)
        service = document["services"]["snapshot-companion-a"]
        service["environment"]["RJ_SNAPSHOT_METRICS_GROUP_GID"] = validator.SESSION_GID
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
            "RJ_KV_EVENT_MODE"
        ] = "shadow"
        with self.assertRaisesRegex(validator.ValidationError, "RJ_KV_EVENT_MODE"):
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

            candidate.write_text(
                original
                + "\nhandle /metrics/extra {\n"
                + "\treverse_proxy unix//run/mini-dynamo-snapshot-b/companion.sock\n"
                + "}\n",
                encoding="utf-8",
            )
            with self.assertRaisesRegex(
                validator.ValidationError, "non-metrics upstream"
            ):
                validator.validate_caddy(candidate)


if __name__ == "__main__":
    unittest.main()
