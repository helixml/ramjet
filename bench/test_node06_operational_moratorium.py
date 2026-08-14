import json
import pathlib
import tempfile
import unittest

import node06_operational_moratorium as moratorium


class Node06OperationalMoratoriumTests(unittest.TestCase):
    def authorization(self, root, *, operation="gpu-workload", now=1_000):
        root = pathlib.Path(root)
        root.chmod(0o700)
        path = root / "authorization.json"
        path.write_text(
            json.dumps(
                {
                    "schema_version": 1,
                    "node": "node06",
                    "operation": operation,
                    "issued_at_unix": now,
                    "expires_at_unix": now + 300,
                    "nonce": "a" * 32,
                    "acknowledgement": moratorium.ACKNOWLEDGEMENT,
                    "ac_repair_confirmed": True,
                    "supervisor_present": True,
                }
            ),
            encoding="utf-8",
        )
        path.chmod(0o600)
        return path

    def test_missing_authorization_fails_closed(self):
        with self.assertRaisesRegex(moratorium.MoratoriumError, "moratorium"):
            moratorium.require_supervised_authorization(None, "gpu-workload")

    def test_exact_private_authorization_is_accepted(self):
        with tempfile.TemporaryDirectory() as directory:
            path = self.authorization(directory)
            document = moratorium.require_supervised_authorization(
                path, "gpu-workload", now=1_000
            )
        self.assertEqual(document["operation"], "gpu-workload")

    def test_authorization_is_bound_and_fresh(self):
        with tempfile.TemporaryDirectory() as directory:
            path = self.authorization(directory)
            for operation, now in (("p2p-gpu-scout", 1_000), ("gpu-workload", 1_301)):
                with self.subTest(operation=operation, now=now), self.assertRaises(
                    moratorium.MoratoriumError
                ):
                    moratorium.require_supervised_authorization(
                        path, operation, now=now
                    )

    def test_rejects_unsafe_parent_file_and_symlink(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            path = self.authorization(root)
            path.chmod(0o644)
            with self.assertRaises(moratorium.MoratoriumError):
                moratorium.require_supervised_authorization(
                    path, "gpu-workload", now=1_000
                )

            path.chmod(0o600)
            link = root / "link.json"
            link.symlink_to(path)
            with self.assertRaises(moratorium.MoratoriumError):
                moratorium.require_supervised_authorization(
                    link, "gpu-workload", now=1_000
                )

            root.chmod(0o755)
            with self.assertRaises(moratorium.MoratoriumError):
                moratorium.require_supervised_authorization(
                    path, "gpu-workload", now=1_000
                )
