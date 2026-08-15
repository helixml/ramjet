#!/usr/bin/env python3
"""Keep developer laptop paths out of the committed tree."""

from __future__ import annotations

import pathlib
import re
import subprocess
import unittest


ROOT = pathlib.Path(__file__).resolve().parents[1]
# node06's serving account is operational, not a laptop home directory.
PERSONAL_HOME = re.compile(rb"/home/(?!luke\b)[A-Za-z_][A-Za-z0-9._-]*")


def tracked_files() -> list[pathlib.Path]:
    listed = subprocess.check_output(
        ["git", "-C", str(ROOT), "ls-files", "-z"],
        cwd=ROOT,
    )
    return [ROOT / path.decode() for path in listed.split(b"\0") if path]


class RepoHygieneTest(unittest.TestCase):
    def test_committed_files_do_not_embed_personal_home_directories(self):
        leaks: list[str] = []
        for path in tracked_files():
            if not path.is_file() or path.is_symlink():
                continue
            data = path.read_bytes()
            if b"\0" in data:
                continue
            for match in PERSONAL_HOME.finditer(data):
                leaks.append(f"{path.relative_to(ROOT)}: {match.group(0).decode()}")
        self.assertEqual(leaks, [])


if __name__ == "__main__":
    unittest.main()
