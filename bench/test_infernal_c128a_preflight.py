import json
import tempfile
import unittest
from pathlib import Path
from unittest import mock

import infernal_c128a_preflight as preflight


def sparse_source(layout):
    return f"""
class Builder:
    def __init__(self):
        c128a_max_compressed = get_c128a_topk_width(
            self.model_config.max_model_len, self.compress_ratio
        )
        self.c128a_max_compressed = c128a_max_compressed

    def _build_c128a_metadata(self):
        active_topk_width = {layout}
        return build_c128a_topk_metadata(
            max_compressed_tokens=active_topk_width
        )
"""


RUNNER_SOURCE = """
class Runner:
    def _build_attention_metadata(self, for_cudagraph_capture):
        if for_cudagraph_capture:
            max_seq_len = self.max_model_len
        else:
            max_seq_len = self.runtime_max
        return max_seq_len
"""


class InfernalC128APreflightTest(unittest.TestCase):
    def source_tree(self, directory, sparse):
        root = Path(directory)
        sparse_path = root / preflight.SPARSE_MLA_PATH
        runner_path = root / preflight.GPU_RUNNER_PATH
        sparse_path.parent.mkdir(parents=True)
        runner_path.parent.mkdir(parents=True)
        sparse_path.write_text(sparse)
        runner_path.write_text(RUNNER_SOURCE)
        return root, sparse_path, runner_path

    def test_candidate_accepts_only_capture_stable_capacity(self):
        with tempfile.TemporaryDirectory() as directory:
            root, _, _ = self.source_tree(
                directory, sparse_source("self.c128a_max_compressed")
            )
            report, passed = preflight.validate(root, "candidate")
        self.assertTrue(passed)
        self.assertEqual(report["status"], "pass")
        self.assertEqual(report["reason_codes"], [])
        self.assertEqual(report["observed"]["layout"], "capture_stable_capacity")

    def test_candidate_rejects_r4_batch_dependent_helper(self):
        with tempfile.TemporaryDirectory() as directory:
            root, _, _ = self.source_tree(
                directory,
                sparse_source(
                    "get_c128a_active_topk_width("
                    "self.cm.max_seq_len, self.compress_ratio, "
                    "self.c128a_max_compressed)"
                ),
            )
            report, passed = preflight.validate(root, "candidate")
        self.assertFalse(passed)
        self.assertEqual(report["reason_codes"], ["layout_batch_dependent"])

    def test_baseline_requires_exact_r4_relevant_blobs_and_layout(self):
        with tempfile.TemporaryDirectory() as directory:
            root, sparse_path, runner_path = self.source_tree(
                directory,
                sparse_source(
                    "get_c128a_active_topk_width("
                    "self.cm.max_seq_len, self.compress_ratio, "
                    "self.c128a_max_compressed)"
                ),
            )
            identities = dict(preflight.EXPECTED_IDENTITIES)
            identities["r4_sparse_mla_blob_sha1"] = preflight.git_blob_sha1(
                sparse_path.read_bytes()
            )
            identities["r4_gpu_model_runner_blob_sha1"] = preflight.git_blob_sha1(
                runner_path.read_bytes()
            )
            with mock.patch.object(preflight, "EXPECTED_IDENTITIES", identities):
                report, passed = preflight.validate(root, "baseline")
        self.assertTrue(passed)
        self.assertEqual(report["observed"]["layout"], "batch_dependent")

    def test_baseline_rejects_a_nearby_but_nonidentical_tree(self):
        with tempfile.TemporaryDirectory() as directory:
            root, _, _ = self.source_tree(
                directory,
                sparse_source(
                    "get_c128a_active_topk_width("
                    "self.cm.max_seq_len, self.compress_ratio, "
                    "self.c128a_max_compressed)"
                ),
            )
            report, passed = preflight.validate(root, "baseline")
        self.assertFalse(passed)
        self.assertEqual(report["reason_codes"], ["r4_blob_mismatch"])

    def test_unknown_layout_and_stride_drift_fail_closed(self):
        source = sparse_source("compute_width()")
        source = source.replace(
            "max_compressed_tokens=active_topk_width",
            "max_compressed_tokens=other_width",
        )
        with tempfile.TemporaryDirectory() as directory:
            root, _, _ = self.source_tree(directory, source)
            report, passed = preflight.validate(root, "candidate")
        self.assertFalse(passed)
        self.assertEqual(
            report["reason_codes"],
            ["layout_unknown", "stride_keyword_mismatch"],
        )

    def test_capacity_origin_drift_fails_closed(self):
        source = sparse_source("self.c128a_max_compressed").replace(
            "self.model_config.max_model_len", "self.runtime_max_seq_len"
        )
        with tempfile.TemporaryDirectory() as directory:
            root, _, _ = self.source_tree(directory, source)
            report, passed = preflight.validate(root, "candidate")
        self.assertFalse(passed)
        self.assertEqual(report["reason_codes"], ["capacity_origin_mismatch"])

    def test_capture_origin_drift_fails_closed(self):
        with tempfile.TemporaryDirectory() as directory:
            root, _, runner_path = self.source_tree(
                directory, sparse_source("self.c128a_max_compressed")
            )
            runner_path.write_text(
                RUNNER_SOURCE.replace("self.max_model_len", "self.runtime_max")
            )
            report, passed = preflight.validate(root, "candidate")
        self.assertFalse(passed)
        self.assertEqual(report["reason_codes"], ["capture_origin_mismatch"])

    def test_report_never_contains_source_text_or_path(self):
        secret = "hl-secret-like-source-content"
        with tempfile.TemporaryDirectory(prefix=secret) as directory:
            source = sparse_source("self.c128a_max_compressed") + repr(secret)
            root, _, _ = self.source_tree(directory, source)
            report, passed = preflight.validate(root, "candidate")
        encoded = json.dumps(report)
        self.assertTrue(passed)
        self.assertNotIn(secret, encoded)
        self.assertNotIn(directory, encoded)
        self.assertLessEqual(set(report["reason_codes"]), preflight.REASON_CODES)

    def test_missing_source_uses_only_bounded_reason_code(self):
        with tempfile.TemporaryDirectory() as directory:
            report, passed = preflight.validate(Path(directory), "candidate")
        self.assertFalse(passed)
        self.assertEqual(report["observed"], {})
        self.assertEqual(report["reason_codes"], ["source_missing"])


if __name__ == "__main__":
    unittest.main()
