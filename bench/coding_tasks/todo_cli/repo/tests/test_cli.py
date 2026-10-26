import contextlib
import io
import json
import os
import tempfile
import unittest

from todo.cli import main

TASKS = [
    {"id": 2, "title": "Pay rent", "priority": "high", "due": "2026-02-01"},
    {"id": 1, "title": "Buy milk", "priority": "low"},
    {"id": 3, "title": "Old chore", "priority": "high", "done": True},
]


class TodoCliTest(unittest.TestCase):
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

    def test_lists_open_tasks_by_id(self):
        status, lines = self.run_cli()
        self.assertEqual(status, 0)
        self.assertEqual(
            lines,
            [
                "1. [ ] Buy milk (priority: low, due: -)",
                "2. [ ] Pay rent (priority: high, due: 2026-02-01)",
            ],
        )

    def test_priority_filter(self):
        status, lines = self.run_cli("--priority", "high")
        self.assertEqual(status, 0)
        self.assertEqual(lines, ["2. [ ] Pay rent (priority: high, due: 2026-02-01)"])


if __name__ == "__main__":
    unittest.main()
