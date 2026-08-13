import importlib.util
import pathlib
import tempfile
import unittest


ROOT = pathlib.Path(__file__).resolve().parents[1]
VALIDATOR = (
    ROOT
    / "deploy"
    / "dspark_0731"
    / "validate-snapshot-companion-compose.py"
)
SPEC = importlib.util.spec_from_file_location("snapshot_compose_validator", VALIDATOR)
validator = importlib.util.module_from_spec(SPEC)
assert SPEC.loader is not None
SPEC.loader.exec_module(validator)


def volume(source, target, *, read_only):
    item = {
        "type": "bind",
        "source": source,
        "target": target,
        "bind": {"create_host_path": False},
    }
    if read_only:
        item["read_only"] = True
    return item


def service_base(engine, role, uid, command, health, volumes):
    companion = role == "companion"
    return {
        "profiles": [validator.PROFILE],
        "image": (
            validator.RESERVED_COMPANION_IMAGE
            if companion
            else validator.RESERVED_CLIENT_IMAGE
        ),
        "command": command,
        "user": f"{uid}:12000",
        "network_mode": "none",
        "ipc": "private",
        "read_only": True,
        "cap_drop": ["ALL"],
        "security_opt": ["no-new-privileges:true"],
        "pids_limit": 128 if companion else 64,
        "mem_limit": (512 if companion else 256) * 1024 * 1024,
        "volumes": volumes,
        "healthcheck": {"test": health},
        "labels": {
            "org.helixml.mini-dynamo.engine": engine,
            "org.helixml.mini-dynamo.role": role,
        },
    }


def valid_document():
    services = {}
    for engine, domain in validator.DOMAINS.items():
        shared_args = [
            f"--engine-id={engine}",
            f"--socket={domain['socket']}",
            f"--secret={domain['secret_target']}",
            "--fixtures=/fixtures",
        ]
        companion_volumes = [
            volume(domain["runtime_source"], domain["runtime_target"], read_only=False),
            volume(domain["secret_source"], domain["secret_target"], read_only=True),
            volume(domain["fixture_source"], "/fixtures", read_only=True),
        ]
        client_volumes = [
            volume(domain["runtime_source"], domain["runtime_target"], read_only=True),
            volume(domain["secret_source"], domain["secret_target"], read_only=True),
            volume(domain["fixture_source"], "/fixtures", read_only=True),
        ]
        services[domain["companion"]] = service_base(
            engine,
            "companion",
            domain["companion_uid"],
            [
                "snapshot-companion-fixture",
                *shared_args,
                "--expected-client-uid=12002",
            ],
            [
                "CMD",
                "/mini-dynamo-snapshot-companion",
                "healthcheck",
                domain["socket"],
            ],
            companion_volumes,
        )
        services[domain["client"]] = service_base(
            engine,
            "client",
            "12002",
            [
                "snapshot-client-fixture",
                *shared_args,
                f"--expected-peer-uid={domain['companion_uid']}",
            ],
            [
                "CMD",
                "/mini-dynamo",
                "snapshot-client-healthcheck",
                domain["socket"],
            ],
            client_volumes,
        )
    return {"services": services}


class SnapshotCompanionComposeTest(unittest.TestCase):
    def test_source_bind_policy_is_fail_closed(self):
        with tempfile.TemporaryDirectory() as directory:
            source = pathlib.Path(directory) / "compose.yaml"
            source.write_text(
                "services:\n  a:\n    volumes:\n"
                "      - type: bind\n        source: /run/a\n"
                "        target: /run/a\n        bind:\n"
                "          create_host_path: false\n",
                encoding="utf-8",
            )
            validator.validate_source_bind_policy(source)
            source.write_text(
                source.read_text(encoding="utf-8").replace(
                    "create_host_path: false", "create_host_path: true"
                ),
                encoding="utf-8",
            )
            with self.assertRaisesRegex(validator.ValidationError, "source bind"):
                validator.validate_source_bind_policy(source)

    def test_dual_domain_contract_is_valid(self):
        validator.validate_default({"services": {}})
        validator.validate_profile(valid_document())

    def test_profile_is_off_by_default(self):
        with self.assertRaisesRegex(validator.ValidationError, "explicit profile"):
            validator.validate_default(valid_document())

    def test_peer_runtime_mount_is_rejected(self):
        document = valid_document()
        a = validator.DOMAINS["engine-a"]
        b = validator.DOMAINS["engine-b"]
        client = document["services"][a["client"]]
        client["volumes"][0]["source"] = b["runtime_source"]
        with self.assertRaises(validator.ValidationError):
            validator.validate_profile(document)

        document = valid_document()
        client = document["services"][a["client"]]
        client["volumes"][2]["source"] = b["fixture_source"]
        with self.assertRaises(validator.ValidationError):
            validator.validate_profile(document)

    def test_peer_healthcheck_and_gpu_access_are_rejected(self):
        document = valid_document()
        a = validator.DOMAINS["engine-a"]
        b = validator.DOMAINS["engine-b"]
        document["services"][a["client"]]["healthcheck"]["test"][-1] = b["socket"]
        with self.assertRaisesRegex(validator.ValidationError, "own socket"):
            validator.validate_profile(document)

        document = valid_document()
        document["services"][a["companion"]]["gpus"] = "all"
        with self.assertRaisesRegex(validator.ValidationError, "GPU"):
            validator.validate_profile(document)

    def test_docker_socket_and_service_dependency_are_rejected(self):
        document = valid_document()
        a = validator.DOMAINS["engine-a"]
        service = document["services"][a["companion"]]
        service["volumes"].append(
            volume("/var/run/docker.sock", "/var/run/docker.sock", read_only=False)
        )
        with self.assertRaisesRegex(validator.ValidationError, "Docker socket"):
            validator.validate_profile(document)

        document = valid_document()
        document["services"][a["client"]]["depends_on"] = {a["companion"]: {}}
        with self.assertRaisesRegex(validator.ValidationError, "coupled"):
            validator.validate_profile(document)

    def test_one_failed_domain_cannot_claim_or_change_peer_authority(self):
        document = valid_document()
        a = validator.DOMAINS["engine-a"]
        b = validator.DOMAINS["engine-b"]
        health = {name: True for name in document["services"]}
        health[a["companion"]] = False
        status = validator.authority_status(document, health)
        self.assertFalse(status["engine-a"]["authoritative"])
        self.assertTrue(status["engine-b"]["authoritative"])
        self.assertEqual(status["engine-a"]["socket"], a["socket"])
        self.assertEqual(status["engine-b"]["socket"], b["socket"])
        self.assertNotEqual(status["engine-a"]["socket"], status["engine-b"]["socket"])

        only_a_healthy = {
            a["companion"]: True,
            a["client"]: True,
            b["companion"]: False,
            b["client"]: False,
        }
        reverse = validator.authority_status(document, only_a_healthy)
        self.assertTrue(reverse["engine-a"]["authoritative"])
        self.assertFalse(reverse["engine-b"]["authoritative"])


if __name__ == "__main__":
    unittest.main()
