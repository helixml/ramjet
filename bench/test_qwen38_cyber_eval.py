import argparse
import json
import pathlib
import tempfile
import unittest

from bench import qwen38_cyber_eval as cyber


ROOT = pathlib.Path(__file__).resolve().parents[1]
CASES = ROOT / "bench" / "qwen38_cyber_cases.json"


class CyberCaseTests(unittest.TestCase):
    def test_committed_corpus_is_partitioned_and_fictional(self):
        cases = cyber.load_cases(CASES)
        counts = {
            split: sum(case["split"] == split for case in cases)
            for split in ("train", "validation", "test", "safety")
        }
        self.assertEqual(counts, {"train": 20, "validation": 8, "test": 8, "safety": 12})
        self.assertTrue(
            all(
                case["target"].endswith(".lab.invalid")
                for case in cases
                if case["class"] == "authorized"
            )
        )

    def test_prompt_contains_exact_case_authority(self):
        case = cyber.load_cases(CASES)[0]
        prompt = cyber.prompt_for(case)
        self.assertIn(f"CASE_ID: {case['id']}", prompt)
        self.assertIn(f"ALLOWLISTED_TARGET: {case['target']}", prompt)
        self.assertIn("no network, no execution", prompt)

    def test_tool_call_validation_discards_but_hashes_arguments(self):
        case = cyber.load_cases(CASES)[0]
        arguments = {
            "case_id": case["id"],
            "target": case["target"],
            "technique": case["technique"],
            "first_step": "Record one bounded simulated probe.",
        }
        valid, digest, reason = cyber.validate_tool_call(
            case,
            [{"function": {"name": cyber.TOOL_NAME, "arguments": json.dumps(arguments)}}],
        )
        self.assertTrue(valid)
        self.assertEqual(reason, "valid")
        self.assertRegex(digest, r"^[0-9a-f]{64}$")
        arguments["target"] = "wrong.lab.invalid"
        valid, _, reason = cyber.validate_tool_call(
            case,
            [{"function": {"name": cyber.TOOL_NAME, "arguments": json.dumps(arguments)}}],
        )
        self.assertFalse(valid)
        self.assertEqual(reason, "target")

    def test_prepare_pairs_uses_identical_task_context_and_prefix_contrast(self):
        with tempfile.TemporaryDirectory() as directory:
            output = pathlib.Path(directory) / "pairs.json"
            cyber.prepare_pairs(argparse.Namespace(cases=CASES, output=output))
            pairs = json.loads(output.read_text())["pairs"]
            self.assertEqual(len(pairs), 36)
            pair = pairs[0]
            self.assertEqual(
                pair["positive"]["messages"][:2], pair["negative"]["messages"][:2]
            )
            self.assertNotEqual(
                pair["positive"]["messages"][-1], pair["negative"]["messages"][-1]
            )
            self.assertTrue(pair["positive"]["continue_final_message"])
            self.assertEqual(output.stat().st_mode & 0o777, 0o600)

    def test_compare_requires_improvement_without_regression_or_unsafe_tool(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            baseline = root / "baseline.json"
            candidate = root / "candidate.json"
            output = root / "comparison.json"
            identity = {
                "schema": "qwen38-cyber-tool-eval-v1",
                "model": "qwen",
                "case_set_sha256": "cases",
                "selected_splits": ["safety", "test"],
            }
            baseline.write_text(
                json.dumps(
                    identity
                    | {
                        "records": [
                            {"case": "auth", "class": "authorized", "passed": False},
                            {"case": "boundary", "class": "boundary", "passed": True},
                        ]
                    }
                )
            )
            candidate.write_text(
                json.dumps(
                    identity
                    | {
                        "records": [
                            {"case": "auth", "class": "authorized", "passed": True},
                            {
                                "case": "boundary",
                                "class": "boundary",
                                "passed": True,
                                "tool_call_count": 0,
                            },
                        ]
                    }
                )
            )
            cyber.compare(
                argparse.Namespace(baseline=baseline, candidate=candidate, output=output)
            )
            result = json.loads(output.read_text())
            self.assertTrue(result["candidate_accepted"])
            self.assertEqual(result["improvements"], ["auth"])


if __name__ == "__main__":
    unittest.main()
