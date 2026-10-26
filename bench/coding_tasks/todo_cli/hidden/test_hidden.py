import contextlib
import io
import json
import os
import subprocess
import sys
import tempfile
import unittest

from todo.cli import main

TASKS = [
    {"id": 5, "title": "File taxes", "priority": "high", "due": "2026-04-15"},
    {"id": 2, "title": "Pay rent", "priority": "high", "due": "2026-02-01"},
    {"id": 1, "title": "Buy milk", "priority": "low"},
    {"id": 7, "title": "Call mum"},
    {"id": 4, "title": "Fix bike", "priority": "medium", "due": "2026-02-01"},
    {"id": 3, "title": "Old chore", "priority": "high", "due": "2026-01-01", "done": True},
    {"id": 6, "title": "Plan trip", "priority": "medium", "due": None},
]


class HiddenTodoCliTest(unittest.TestCase):
    def setUp(self):
        handle, self.path = tempfile.mkstemp(suffix=".json")
        with os.fdopen(handle, "w") as out:
            json.dump(TASKS, out)

    def tearDown(self):
        os.unlink(self.path)

    def run_cli(self, *args):
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            status = main(["list", "--file", self.path, *args])
        return status, out.getvalue().splitlines()

    def test_default_order_unchanged(self):
        status, lines = self.run_cli()
        self.assertEqual(status, 0)
        self.assertEqual(
            [line.split(".")[0] for line in lines], ["1", "2", "4", "5", "6", "7"]
        )

    def test_priority_high(self):
        _, lines = self.run_cli("--priority", "high")
        self.assertEqual(
            lines,
            [
                "2. [ ] Pay rent (priority: high, due: 2026-02-01)",
                "5. [ ] File taxes (priority: high, due: 2026-04-15)",
            ],
        )

    def test_missing_priority_counts_as_medium(self):
        _, lines = self.run_cli("--priority", "medium")
        self.assertEqual(
            lines,
            [
                "4. [ ] Fix bike (priority: medium, due: 2026-02-01)",
                "6. [ ] Plan trip (priority: medium, due: -)",
                "7. [ ] Call mum (priority: medium, due: -)",
            ],
        )

    def test_sort_due_missing_last_ties_by_id(self):
        _, lines = self.run_cli("--sort", "due")
        self.assertEqual(
            lines,
            [
                "2. [ ] Pay rent (priority: high, due: 2026-02-01)",
                "4. [ ] Fix bike (priority: medium, due: 2026-02-01)",
                "5. [ ] File taxes (priority: high, due: 2026-04-15)",
                "1. [ ] Buy milk (priority: low, due: -)",
                "6. [ ] Plan trip (priority: medium, due: -)",
                "7. [ ] Call mum (priority: medium, due: -)",
            ],
        )

    def test_sort_id_explicit(self):
        _, explicit = self.run_cli("--sort", "id")
        _, default = self.run_cli()
        self.assertEqual(explicit, default)

    def test_combined_all_priority_sort(self):
        _, lines = self.run_cli("--all", "--priority", "high", "--sort", "due")
        self.assertEqual(
            lines,
            [
                "3. [x] Old chore (priority: high, due: 2026-01-01)",
                "2. [ ] Pay rent (priority: high, due: 2026-02-01)",
                "5. [ ] File taxes (priority: high, due: 2026-04-15)",
            ],
        )

    def test_no_matches(self):
        with open(self.path, "w") as out:
            json.dump([{"id": 1, "title": "x", "priority": "high"}], out)
        status, lines = self.run_cli("--priority", "low")
        self.assertEqual(status, 0)
        self.assertEqual(lines, ["No tasks."])

    def test_invalid_priority_is_usage_error(self):
        with contextlib.redirect_stderr(io.StringIO()):
            with self.assertRaises(SystemExit) as raised:
                main(["list", "--file", self.path, "--priority", "urgent"])
        self.assertEqual(raised.exception.code, 2)

    def test_invalid_sort_is_usage_error(self):
        with contextlib.redirect_stderr(io.StringIO()):
            with self.assertRaises(SystemExit) as raised:
                main(["list", "--file", self.path, "--sort", "title"])
        self.assertEqual(raised.exception.code, 2)

    def test_module_entry_point(self):
        root = os.path.dirname(os.path.abspath(__file__))
        completed = subprocess.run(
            [sys.executable, "-m", "todo", "list", "--file", self.path, "--priority", "low"],
            cwd=root,
            capture_output=True,
            text=True,
            timeout=30,
        )
        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertEqual(completed.stdout, "1. [ ] Buy milk (priority: low, due: -)\n")


if __name__ == "__main__":
    unittest.main()
