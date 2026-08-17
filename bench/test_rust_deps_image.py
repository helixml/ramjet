import importlib.util
import pathlib
import tempfile
import unittest


ROOT = pathlib.Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "bench" / "rust_deps_image.py"
SPEC = importlib.util.spec_from_file_location("rust_deps_image", SCRIPT)
rust_deps_image = importlib.util.module_from_spec(SPEC)
assert SPEC.loader is not None
SPEC.loader.exec_module(rust_deps_image)


class RustDepsImageTest(unittest.TestCase):
    def test_repository_reference_matches_dependency_inputs(self):
        self.assertEqual(rust_deps_image.validation_errors(ROOT), [])

    def test_key_frames_names_and_payloads(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            for relative in rust_deps_image.INPUTS:
                path = root / relative
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_bytes(relative.as_posix().encode())
            first = rust_deps_image.dependency_key(root)
            (root / rust_deps_image.INPUTS[0]).write_bytes(b"different")
            second = rust_deps_image.dependency_key(root)
            self.assertEqual(len(first), 64)
            self.assertNotEqual(first, second)

    def test_update_writes_every_reference(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            for relative in rust_deps_image.INPUTS:
                path = root / relative
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_bytes(relative.as_posix().encode())
            (root / "Dockerfile").write_text(
                "ARG RUST_DEPS_IMAGE=ghcr.io/helixml/ramjet:rust-deps-sha256-deadbeef\n"
            )
            (root / "Dockerfile.companion").write_text(
                "ARG RUST_DEPS_IMAGE=ghcr.io/helixml/ramjet:rust-deps-sha256-deadbeef\n"
            )
            (root / ".drone.yml").write_text(
                "--destination ghcr.io/example:rust-deps-sha256-deadbeef\n"
            )

            expected = rust_deps_image.image_reference(
                rust_deps_image.dependency_key(root)
            )
            self.assertEqual(rust_deps_image.update_references(root), expected)
            self.assertEqual(rust_deps_image.validation_errors(root), [])


if __name__ == "__main__":
    unittest.main()
