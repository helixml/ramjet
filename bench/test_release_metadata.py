#!/usr/bin/env python3
"""Fail closed when source, image, and release versions drift."""

from __future__ import annotations

import pathlib
import re
import tomllib
import unittest


ROOT = pathlib.Path(__file__).resolve().parents[1]


class ReleaseMetadataTest(unittest.TestCase):
    def test_release_version_is_consistent(self) -> None:
        cargo = tomllib.loads((ROOT / "Cargo.toml").read_text())
        version = cargo["package"]["version"]
        self.assertEqual(version, "0.1.0")

        lock = tomllib.loads((ROOT / "Cargo.lock").read_text())
        package_versions = [
            package["version"]
            for package in lock["package"]
            if package["name"] == cargo["package"]["name"]
        ]
        self.assertEqual(package_versions, [version])

        label = f'LABEL org.opencontainers.image.version="{version}"'
        for dockerfile in ("Dockerfile", "Dockerfile.companion"):
            with self.subTest(dockerfile=dockerfile):
                contents = (ROOT / dockerfile).read_text()
                self.assertEqual(contents.count(label), 1)
                self.assertEqual(
                    len(re.findall(r"org\.opencontainers\.image\.version=", contents)),
                    1,
                )

        self.assertIn(f"## {version} —", (ROOT / "CHANGELOG.md").read_text())
        self.assertIn(
            f"Version {version} is the first public Rust release",
            (ROOT / "README.md").read_text(),
        )

    def test_package_has_public_release_documents(self) -> None:
        cargo = tomllib.loads((ROOT / "Cargo.toml").read_text())
        included = set(cargo["package"]["include"])
        self.assertTrue({"README.md", "CHANGELOG.md", "RELEASE.md", "LICENSE"} <= included)

        license_text = (ROOT / "LICENSE").read_text()
        self.assertIn("Apache License", license_text)
        self.assertIn("Version 2.0, January 2004", license_text)

    def test_local_secrets_are_ignored(self) -> None:
        ignored = {
            line.strip()
            for line in (ROOT / ".gitignore").read_text().splitlines()
            if line.strip() and not line.lstrip().startswith("#")
        }
        self.assertIn(".env", ignored)


if __name__ == "__main__":
    unittest.main()
