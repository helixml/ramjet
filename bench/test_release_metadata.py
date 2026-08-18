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
        """`Cargo.toml` is the single source of the version.

        Everything that ships inside the images has to agree with it before a
        tag is created, because `bench/drone_release_plan.sh` refuses to
        publish when the tag and the package version disagree. Asserting a
        literal version here would instead make every release edit its own
        guard, which is how the guard stops meaning anything.
        """
        cargo = tomllib.loads((ROOT / "Cargo.toml").read_text())
        version = cargo["package"]["version"]
        # Docker tags reject Cargo build metadata, so reject it at the source.
        self.assertRegex(version, r"^\d+\.\d+\.\d+$")

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

    def test_readme_pins_a_released_version_by_digest(self) -> None:
        """The quickstart must pin an immutable digest of a released version.

        It deliberately does not have to be the current `Cargo.toml` version:
        a release digest only exists after the tag pipeline publishes it, so
        the pin advances in `RELEASE.md`'s post-acceptance step rather than in
        the commit that bumps the version.
        """
        readme = (ROOT / "README.md").read_text()
        pins = re.findall(
            r"ghcr\.io/helixml/ramjet:v(\d+\.\d+\.\d+)@sha256:([0-9a-f]{64})\b",
            readme,
        )
        self.assertTrue(pins, "the quickstart must pin an immutable image digest")

        changelog = (ROOT / "CHANGELOG.md").read_text()
        for pinned_version, _digest in pins:
            with self.subTest(version=pinned_version):
                self.assertIn(f"## {pinned_version} —", changelog)

    def test_compose_defaults_pin_the_current_release_by_digest(self) -> None:
        """Every shipped compose default pins `v<Cargo version>` by digest.

        These pins rotted silently through the rename: a mirror kept serving a
        pre-rename image for weeks because nothing compared them to anything.
        Tag and digest must both be present -- a tag alone is mutable, and a
        digest alone hides which release it is.
        """
        version = tomllib.loads((ROOT / "Cargo.toml").read_text())["package"]["version"]
        pattern = re.compile(
            r"image:\s*\$\{[A-Z_]+:-(ghcr\.io/helixml/ramjet:([^@\s}]+)@sha256:[0-9a-f]{64})\}"
        )
        bare = re.compile(r"image:\s*\$\{[A-Z_]+:-ghcr\.io/helixml/ramjet:[^@\s}]+\}")
        found = 0
        for compose in sorted(ROOT.glob("deploy/*/docker-compose*.yaml")):
            text = compose.read_text()
            with self.subTest(compose=compose.relative_to(ROOT)):
                self.assertEqual(
                    bare.findall(text), [], "ramjet image pinned by tag without a digest"
                )
                for _ref, tag in pattern.findall(text):
                    found += 1
                    self.assertIn(
                        tag,
                        {f"v{version}", f"companion-v{version}"},
                        f"{compose.name} pins {tag}, not the current release",
                    )
        self.assertGreater(found, 0, "no ramjet image pins found to check")

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
