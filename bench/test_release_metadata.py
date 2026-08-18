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

    def test_compose_defaults_pin_one_released_version_by_digest(self) -> None:
        """Every shipped compose default pins the same released version.

        These pins rotted silently through the rename -- a mirror served a
        pre-rename image for weeks -- because nothing ever compared them to
        anything. Tag and digest must both be present: a tag alone is mutable,
        a digest alone hides which release it is.

        The pinned version deliberately does not have to equal the current
        `Cargo.toml` version. A release digest only exists after the tag
        pipeline publishes it, so between the version bump and publication
        there is nothing to point at; requiring equality would make that
        intermediate state unrepresentable. It must be a version `CHANGELOG.md`
        documents, and every compose file must agree on it.
        """
        pattern = re.compile(
            r"image:\s*\$\{[A-Z_]+:-(?:ghcr\.io/helixml/ramjet):"
            r"(?:companion-)?v(\d+\.\d+\.\d+)@sha256:[0-9a-f]{64}\}"
        )
        loose = re.compile(r"image:\s*\$\{[A-Z_]+:-ghcr\.io/helixml/ramjet:[^}]*\}")
        changelog = (ROOT / "CHANGELOG.md").read_text()
        pinned: dict[str, str] = {}
        for compose in sorted(ROOT.glob("deploy/*/docker-compose*.yaml")):
            text = compose.read_text()
            name = str(compose.relative_to(ROOT))
            with self.subTest(compose=name):
                for reference in loose.findall(text):
                    self.assertRegex(
                        reference,
                        pattern.pattern.replace("(?:companion-)?v(", "(?:companion-)?v("),
                        f"{name} pins a ramjet image without a v<x.y.z> tag and digest",
                    )
                for version in pattern.findall(text):
                    pinned[name] = version
                    self.assertIn(
                        f"## {version} —",
                        changelog,
                        f"{name} pins v{version}, which CHANGELOG.md does not document",
                    )
        self.assertTrue(pinned, "no ramjet image pins found to check")
        self.assertEqual(
            len(set(pinned.values())),
            1,
            f"compose files disagree on the pinned release: {pinned}",
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
