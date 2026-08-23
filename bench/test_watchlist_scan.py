#!/usr/bin/env python3
"""Tests for the watchlist registry and its scanner."""

import ast
import contextlib
import io
import json
import os
import pathlib
import sys
import tempfile
import types
import unittest
import unittest.mock

import watchlist_scan


def entry(**overrides):
    base = {
        "name": "example",
        "kind": "gh-repo",
        "id": "owner/repo",
        "why": "a sufficiently long reason string",
        "watch_for": "a sufficiently long trigger string",
    }
    base.update(overrides)
    return base


def document(*entries):
    return {"sources": list(entries)}


class CommittedRegistryTests(unittest.TestCase):
    def test_the_committed_registry_is_valid(self):
        sources = watchlist_scan.load_sources()
        self.assertGreater(len(sources), 5)

    def test_every_committed_entry_says_what_would_matter(self):
        # The scan prints watch_for next to each change. Without it a future
        # reader sees "this repo moved" and cannot decide whether to care.
        for source in watchlist_scan.load_sources():
            self.assertGreater(len(source["watch_for"].strip()), 30, source["name"])


class NoThirdPartyDependencyTests(unittest.TestCase):
    def test_the_scanner_imports_nothing_outside_the_standard_library(self):
        # Every other tool in bench/ and deploy/ runs on the stdlib alone and
        # the CI image installs no packages; a third-party import here fails
        # the whole agent-protocol step at collection time.
        tree = ast.parse(pathlib.Path(watchlist_scan.__file__).read_text())
        imported = set()
        for node in ast.walk(tree):
            if isinstance(node, ast.Import):
                imported.update(alias.name.split(".")[0] for alias in node.names)
            elif isinstance(node, ast.ImportFrom) and node.level == 0 and node.module:
                imported.add(node.module.split(".")[0])
        outside = sorted(imported - sys.stdlib_module_names)
        self.assertEqual(outside, [], f"non-stdlib imports: {outside}")


class ValidationTests(unittest.TestCase):
    def test_unknown_kind_is_rejected(self):
        with self.assertRaises(watchlist_scan.SourceError):
            watchlist_scan.validate_sources(document(entry(kind="twitter")))

    def test_placeholder_watch_for_is_rejected(self):
        with self.assertRaises(watchlist_scan.SourceError):
            watchlist_scan.validate_sources(document(entry(watch_for="tbd")))

    def test_missing_why_is_rejected(self):
        broken = entry()
        del broken["why"]
        with self.assertRaises(watchlist_scan.SourceError):
            watchlist_scan.validate_sources(document(broken))

    def test_github_id_must_be_owner_slash_repo(self):
        with self.assertRaises(watchlist_scan.SourceError):
            watchlist_scan.validate_sources(document(entry(id="justarepo")))

    def test_hf_repo_id_must_be_owner_slash_model(self):
        with self.assertRaises(watchlist_scan.SourceError):
            watchlist_scan.validate_sources(document(entry(kind="hf-repo", id="org")))

    def test_hf_org_id_needs_no_slash(self):
        watchlist_scan.validate_sources(document(entry(kind="hf-org", id="some-org")))

    def test_duplicate_ids_are_rejected(self):
        with self.assertRaises(watchlist_scan.SourceError):
            watchlist_scan.validate_sources(document(entry(), entry(name="other")))

    def test_empty_registry_is_rejected(self):
        with self.assertRaises(watchlist_scan.SourceError):
            watchlist_scan.validate_sources({"sources": []})


class IgnoreFilterTests(unittest.TestCase):
    def test_invalid_regex_is_rejected_at_load(self):
        # Better a corpus error now than a crash mid-scan.
        with self.assertRaises(watchlist_scan.SourceError):
            watchlist_scan.validate_sources(document(entry(ignore="[unclosed")))

    def test_empty_ignore_is_rejected(self):
        with self.assertRaises(watchlist_scan.SourceError):
            watchlist_scan.validate_sources(document(entry(ignore="")))

    def test_absent_ignore_is_fine(self):
        watchlist_scan.validate_sources(document(entry()))

    def test_matching_refs_are_dropped_before_the_state_file(self):
        real = watchlist_scan.fetch
        watchlist_scan.fetch = lambda url, timeout=15: {
            "results": [
                {"name": "nightly-dev-20260823", "last_updated": "2026-08-23T00:00:00Z"},
                {"name": "v0.5.18", "last_updated": "2026-08-21T00:00:00Z"},
            ]
        }
        try:
            source = entry(kind="docker-hub", id="ns/repo", ignore="nightly-")
            result = watchlist_scan.probe(source, 5)
        finally:
            watchlist_scan.fetch = real
        self.assertEqual([item["ref"] for item in result["items"]], ["ns/repo:v0.5.18"])


class GithubTokenTests(unittest.TestCase):
    def setUp(self):
        self.addCleanup(setattr, watchlist_scan, "_GH_TOKEN", None)
        watchlist_scan._GH_TOKEN = None

    def test_environment_wins_and_gh_is_not_invoked(self):
        called = []
        real_run = watchlist_scan.subprocess.run
        watchlist_scan.subprocess.run = lambda *a, **k: called.append(a) or real_run
        self.addCleanup(setattr, watchlist_scan.subprocess, "run", real_run)
        with unittest.mock.patch.dict(os.environ, {"GITHUB_TOKEN": "from-env"}):
            self.assertEqual(watchlist_scan.github_token(), "from-env")
        self.assertEqual(called, [])

    def test_falls_back_to_gh_when_env_is_unset(self):
        real_run, real_which = watchlist_scan.subprocess.run, watchlist_scan.shutil.which
        watchlist_scan.shutil.which = lambda name: "/usr/bin/gh"
        watchlist_scan.subprocess.run = lambda *a, **k: types.SimpleNamespace(
            returncode=0, stdout="from-gh\n"
        )
        self.addCleanup(setattr, watchlist_scan.subprocess, "run", real_run)
        self.addCleanup(setattr, watchlist_scan.shutil, "which", real_which)
        with unittest.mock.patch.dict(os.environ, {}, clear=True):
            self.assertEqual(watchlist_scan.github_token(), "from-gh")

    def test_unauthenticated_is_a_supported_mode(self):
        # No gh, no env: the scan must still run, just against the 60/hour cap.
        real_which = watchlist_scan.shutil.which
        watchlist_scan.shutil.which = lambda name: None
        self.addCleanup(setattr, watchlist_scan.shutil, "which", real_which)
        with unittest.mock.patch.dict(os.environ, {}, clear=True):
            self.assertEqual(watchlist_scan.github_token(), "")

    def test_a_broken_gh_does_not_raise(self):
        real_run, real_which = watchlist_scan.subprocess.run, watchlist_scan.shutil.which
        watchlist_scan.shutil.which = lambda name: "/usr/bin/gh"

        def explode(*args, **kwargs):
            raise OSError("gh is broken")

        watchlist_scan.subprocess.run = explode
        self.addCleanup(setattr, watchlist_scan.subprocess, "run", real_run)
        self.addCleanup(setattr, watchlist_scan.shutil, "which", real_which)
        with unittest.mock.patch.dict(os.environ, {}, clear=True):
            self.assertEqual(watchlist_scan.github_token(), "")


class NormalizeTests(unittest.TestCase):
    def test_zulu_suffix_becomes_offset(self):
        self.assertEqual(
            watchlist_scan.normalize("2026-08-23T10:00:00.000Z"), "2026-08-23T10:00:00"
        )

    def test_absent_stamp_is_empty(self):
        self.assertEqual(watchlist_scan.normalize(None), "")

    def test_stamps_compare_lexicographically(self):
        # The scan decides "newer" by string comparison, so the normalized form
        # has to sort correctly or changes are silently missed.
        older = watchlist_scan.normalize("2026-08-01T00:00:00Z")
        newer = watchlist_scan.normalize("2026-08-23T00:00:00Z")
        self.assertLess(older, newer)


class ScanBehaviourTests(unittest.TestCase):
    """End-to-end behaviour with the network stubbed out."""

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        root = pathlib.Path(self.tmp.name)
        self.sources = root / "sources.yaml"
        self.state = root / "state.json"
        self.sources.write_text(
            "[[sources]]\n"
            'name = "example"\n'
            'kind = "gh-repo"\n'
            'id = "owner/repo"\n'
            'why = "a sufficiently long reason string"\n'
            'watch_for = "a sufficiently long trigger string"\n'
        )
        self.pushed = "2026-08-01T00:00:00Z"
        real_fetch = watchlist_scan.fetch

        def fake_fetch(url, timeout=15):
            if url.endswith("/releases/latest"):
                raise LookupError("no releases")
            return {"pushed_at": self.pushed, "stargazers_count": 1}

        watchlist_scan.fetch = fake_fetch
        self.addCleanup(setattr, watchlist_scan, "fetch", real_fetch)

    def run_scan(self, *extra):
        # The scanner is a reporting tool; its stdout is not the assertion.
        with contextlib.redirect_stdout(io.StringIO()):
            return watchlist_scan.main(
                ["--sources", str(self.sources), "--state", str(self.state),
                 "--json", *extra]
            )

    def test_first_run_records_a_baseline_and_reports_nothing(self):
        # Otherwise every source is "new" on day one and the signal is buried.
        self.run_scan()
        recorded = json.loads(self.state.read_text())
        self.assertIn("owner/repo", recorded["seen"])

    def test_unchanged_source_is_not_reported_twice(self):
        self.run_scan()
        before = json.loads(self.state.read_text())["seen"]["owner/repo"]
        self.run_scan()
        self.assertEqual(json.loads(self.state.read_text())["seen"]["owner/repo"], before)

    def test_a_newer_push_is_detected(self):
        self.run_scan()
        self.pushed = "2026-09-15T00:00:00Z"
        self.run_scan()
        self.assertEqual(
            json.loads(self.state.read_text())["seen"]["owner/repo"], "2026-09-15T00:00:00"
        )

    def test_since_does_not_overwrite_state(self):
        # --since is an ad-hoc query; letting it rewrite the baseline would
        # silently skip changes on the next ordinary scan.
        self.run_scan()
        stamp = self.state.read_text()
        self.run_scan("--since", "2020-01-01T00:00:00")
        self.assertEqual(self.state.read_text(), stamp)

    def test_a_failing_source_does_not_abort_the_scan(self):
        def broken(url, timeout=15):
            raise OSError("network down")

        watchlist_scan.fetch = broken
        self.assertEqual(self.run_scan(), 0)


if __name__ == "__main__":
    unittest.main()
