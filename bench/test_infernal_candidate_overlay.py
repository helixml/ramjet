import hashlib
import json
import pathlib
import subprocess
import tempfile
import unittest
from unittest import mock

import infernal_candidate_overlay as overlay
import infernal_parser_probe as parser_probe


class InfernalCandidateOverlayTest(unittest.TestCase):
    def test_committed_manifest_patch_and_docker_contract_are_locked(self):
        manifest = overlay.load_manifest()
        root = overlay.DEFAULT_ROOT
        patch = root / manifest["overlay_patch"]
        self.assertEqual(overlay.sha256(patch), manifest["overlay_patch_sha256"])
        self.assertEqual(
            manifest["base_image_digest"],
            "sha256:21f048058375ccf00ea555f37addad326a7ee33bc2b4699ae53370f25af4ecb6",
        )
        self.assertTrue(
            manifest["base_image"].endswith("@" + manifest["base_image_digest"])
        )
        self.assertEqual(
            manifest["base_vllm_patch_sha256"],
            "dec8963846acbd52dd76500900286fa596da83cafbe1abbc55a8b190e16b8279",
        )
        self.assertEqual(
            manifest["candidate_vllm_tree"],
            "0eb3d442a49b78d194903d37fbff6dd86140e420",
        )

        changed = overlay.patch_paths(patch)
        self.assertEqual(
            changed,
            [
                "vllm/models/deepseek_v4/sparse_mla.py",
                "vllm/parser/deepseek_v4.py",
                "vllm/parser/engine/parser_engine.py",
                "vllm/parser/engine/parser_engine_config.py",
                "vllm/parser/engine/streaming_parser_engine.py",
                "vllm/tool_parsers/utils.py",
            ],
        )
        self.assertFalse(any("deepseek_v32" in path for path in changed))
        self.assertEqual(changed, manifest["changed_files"])
        self.assertIn("MALFORMED_INVOKE_PREFIX_LF", patch.read_text())
        self.assertIn("active_topk_width = self.c128a_max_compressed", patch.read_text())
        self.assertEqual(
            manifest["inputs"]["vllm_49117"]["head"],
            "7ef0ae2480799e95fb7cb801a8105c1db2585164",
        )
        self.assertEqual(
            manifest["inputs"]["vllm_49117"]["upstream_patch_sha256"],
            "5a56dfd4cf8d12a237a39df2993522ad9f5f2cd65603c1a0340a0eac7d585907",
        )
        self.assertEqual(
            manifest["inputs"]["vllm_51318"]["head"],
            "b5a04d25e8e9f3b01a26b57ea6644b71ce44c414",
        )
        self.assertEqual(
            manifest["inputs"]["vllm_51914"]["prefixes"], ["inline", "lf"]
        )

        dockerfile = (root / "Dockerfile").read_text()
        self.assertIn("FROM ${BASE_IMAGE}", dockerfile)
        self.assertIn("/opt/infernal-invocation/vllm", dockerfile)
        self.assertIn('git -C "${source_root}" write-tree', dockerfile)
        self.assertIn('git -C "${source_root}" update-index --refresh --', dockerfile)
        refresh = dockerfile.split("update-index --refresh --", 1)[1].split(
            "git -C", 1
        )[0]
        self.assertEqual(
            [path for path in manifest["changed_files"] if path in refresh],
            manifest["changed_files"],
        )
        self.assertIn("find_spec", dockerfile)
        for variable in (
            "TRITON_CACHE_DIR",
            "TORCHINDUCTOR_CACHE_DIR",
            "B12X_COMPILE_CACHE_DIR",
            "CUTE_DSL_CACHE_DIR",
            "VLLM_FLASHINFER_AUTOTUNE_CACHE_DIR",
        ):
            self.assertIn(variable, dockerfile)

        builder = (root / "build.sh").read_text()
        self.assertIn("BUILD_IMAGE:-0", builder)
        self.assertIn("--network=none", builder)
        for forbidden in (
            "git clone",
            "git fetch",
            "git pull",
            "curl ",
            "wget ",
            "docker push",
        ):
            self.assertNotIn(forbidden, dockerfile + builder)

    def test_prepare_materializes_exact_candidate_tree(self):
        with tempfile.TemporaryDirectory() as directory:
            temp = pathlib.Path(directory)
            source = temp / "source"
            source.mkdir()
            subprocess.run(["git", "init", "-q", str(source)], check=True)
            sparse_path = source / "vllm/models/deepseek_v4/sparse_mla.py"
            sparse_path.parent.mkdir(parents=True)
            sparse_path.write_text("candidate_sparse = True\n")
            (source / "file.py").write_text("value = 1\n")
            subprocess.run(["git", "-C", str(source), "add", "."], check=True)
            subprocess.run(
                [
                    "git",
                    "-C",
                    str(source),
                    "-c",
                    "user.name=Test",
                    "-c",
                    "user.email=test@example.invalid",
                    "commit",
                    "-qm",
                    "base",
                ],
                check=True,
            )
            base_tree = subprocess.check_output(
                ["git", "-C", str(source), "write-tree"], text=True
            ).strip()
            (source / "file.py").write_text("value = 2\n")
            patch_bytes = subprocess.check_output(
                ["git", "-C", str(source), "diff", "--binary", "--full-index"]
            )
            subprocess.run(
                ["git", "-C", str(source), "checkout", "--", "file.py"], check=True
            )

            # Ask Git for the candidate tree with the patch in a temporary index.
            index = temp / "index"
            env = {**dict(__import__("os").environ), "GIT_INDEX_FILE": str(index)}
            subprocess.run(
                ["git", "-C", str(source), "read-tree", base_tree],
                check=True,
                env=env,
            )
            patch_path = temp / "candidate" / "r4-v4-correctness.patch"
            patch_path.parent.mkdir()
            patch_path.write_bytes(patch_bytes)
            subprocess.run(
                ["git", "-C", str(source), "apply", "--cached", str(patch_path)],
                check=True,
                env=env,
            )
            candidate_tree = subprocess.check_output(
                ["git", "-C", str(source), "write-tree"], text=True, env=env
            ).strip()

            manifest = {
                "schema_version": 1,
                "candidate": "fixture",
                "base_image": "example.invalid/r4@sha256:" + "1" * 64,
                "base_image_digest": "sha256:" + "1" * 64,
                "source_root": "/opt/infernal-invocation/vllm",
                "base_vllm_tree": base_tree,
                "candidate_vllm_tree": candidate_tree,
                "candidate_sparse_mla_blob_sha1": overlay.git_blob_sha1(
                    b"candidate_sparse = True\n"
                ),
                "changed_files": ["file.py"],
                "base_vllm_patch_sha256": "3" * 64,
                "overlay_patch": patch_path.name,
                "overlay_patch_sha256": hashlib.sha256(patch_bytes).hexdigest(),
                "base_parser_source_id": "sha256:" + "4" * 64,
                "candidate_parser_source_id": "sha256:" + "5" * 64,
                "cache_fingerprint": "fixture-cache",
            }
            (patch_path.parent / "manifest.json").write_text(json.dumps(manifest))
            output = temp / "output"
            with mock.patch.object(overlay, "run_c128a") as c128a, mock.patch.object(
                overlay, "run_parser"
            ) as parser:
                report = overlay.prepare(source, output, patch_path.parent)
            self.assertEqual((output / "file.py").read_text(), "value = 2\n")
            self.assertEqual(report["candidate_vllm_tree"], candidate_tree)
            self.assertEqual(c128a.call_count, 2)
            self.assertEqual(parser.call_count, 2)
        self.assertEqual(report["preflights"]["python_sources"], 1)
        self.assertEqual(
            report["preflights"]["parser_cases"], overlay.PARSER_CASE_COUNT
        )

    def test_wrapped_r4_behavior_is_identical_in_complete_profile(self):
        cases = parser_probe.load_cases(parser_probe.DEFAULT_CASES)
        wrapped = next(case for case in cases if case["id"] == "wrapped-parallel")
        self.assertEqual(wrapped["expected"]["r4"], wrapped["expected"]["complete"])
        self.assertEqual(
            wrapped["expected"]["r4"],
            {
                "tool_call_starts": 2,
                "tool_call_ends": 2,
                "open_at_eof": False,
                "args_json_valid": True,
                "duplicate_canonical_args": False,
                "dsml_content": False,
                "content": "",
            },
        )

    def test_patch_digest_mismatch_fails_before_preflights(self):
        with tempfile.TemporaryDirectory() as directory:
            candidate = pathlib.Path(directory)
            manifest = dict(overlay.load_manifest())
            manifest["overlay_patch_sha256"] = "0" * 64
            (candidate / "manifest.json").write_text(json.dumps(manifest))
            (candidate / manifest["overlay_patch"]).write_text("changed")
            with self.assertRaisesRegex(overlay.CandidateError, "patch_digest_mismatch"):
                overlay.prepare(pathlib.Path("missing"), candidate / "output", candidate)

    def test_reject_report_contains_only_bounded_reason(self):
        with mock.patch(
            "infernal_candidate_overlay.prepare",
            side_effect=overlay.CandidateError("base_tree_mismatch"),
        ), mock.patch(
            "sys.argv", ["preflight", "source-secret", "output-secret"]
        ), mock.patch("builtins.print") as printed:
            status = overlay.main()
        self.assertEqual(status, 1)
        report = json.loads(printed.call_args.args[0])
        self.assertEqual(report["reason_code"], "base_tree_mismatch")
        self.assertNotIn("secret", json.dumps(report))
        self.assertLessEqual({report["reason_code"]}, overlay.REASONS)

    def test_patch_scope_mismatch_fails_closed(self):
        with tempfile.TemporaryDirectory() as directory:
            candidate = pathlib.Path(directory)
            manifest = dict(overlay.load_manifest())
            patch = overlay.DEFAULT_ROOT / manifest["overlay_patch"]
            manifest["changed_files"] = manifest["changed_files"][:-1]
            (candidate / "manifest.json").write_text(json.dumps(manifest))
            (candidate / manifest["overlay_patch"]).write_bytes(patch.read_bytes())
            with self.assertRaisesRegex(overlay.CandidateError, "patch_scope_mismatch"):
                overlay.prepare(pathlib.Path("missing"), candidate / "output", candidate)


if __name__ == "__main__":
    unittest.main()
