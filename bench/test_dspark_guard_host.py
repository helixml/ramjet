from __future__ import annotations

import importlib.util
import os
import pathlib
import tempfile
import unittest


ROOT = pathlib.Path(__file__).resolve().parents[1]
MODULE_PATH = ROOT / "deploy" / "dspark_0731" / "setup_dspark_guard_host.py"
SPEC = importlib.util.spec_from_file_location("setup_dspark_guard_host", MODULE_PATH)
assert SPEC is not None and SPEC.loader is not None
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


class DsparkGuardHostTest(unittest.TestCase):
    def authority(self) -> tuple[tempfile.TemporaryDirectory[str], pathlib.Path, pathlib.Path]:
        temporary = tempfile.TemporaryDirectory(prefix="md-dspark-host-")
        directory = pathlib.Path(temporary.name) / "authority"
        directory.mkdir(mode=0o700)
        state = directory / "state.json"
        state.write_bytes(MODULE.EMPTY_STATE)
        state.chmod(0o600)
        return temporary, directory, state

    def validate(self, directory: pathlib.Path, state: pathlib.Path) -> None:
        MODULE.validate(
            directory,
            state,
            require_tmpfs=False,
            owner_uid=os.geteuid(),
            group_gid=os.getegid(),
        )

    def test_accepts_protected_canonical_authority(self) -> None:
        temporary, directory, state = self.authority()
        self.addCleanup(temporary.cleanup)
        self.validate(directory, state)

    def test_rejects_unsafe_modes_links_and_documents(self) -> None:
        temporary, directory, state = self.authority()
        self.addCleanup(temporary.cleanup)
        directory.chmod(0o770)
        with self.assertRaises(MODULE.SetupError):
            self.validate(directory, state)
        directory.chmod(0o700)

        state.chmod(0o640)
        with self.assertRaises(MODULE.SetupError):
            self.validate(directory, state)
        state.chmod(0o600)

        second = directory / "second"
        os.link(state, second)
        with self.assertRaises(MODULE.SetupError):
            self.validate(directory, state)
        second.unlink()

        state.write_bytes(b"not-json")
        with self.assertRaises(MODULE.SetupError):
            self.validate(directory, state)

        state.write_bytes(
            b'{"schema_version":1,"runtime_dirty":"false","quarantines":[]}'
        )
        with self.assertRaises(MODULE.SetupError):
            self.validate(directory, state)


if __name__ == "__main__":
    unittest.main()
