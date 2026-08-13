import pathlib
import tempfile
import unittest

from bench import release_tools_image


ROOT = pathlib.Path(__file__).resolve().parents[1]


class ReleaseToolsImageTest(unittest.TestCase):
    def test_repository_references_match_content_key(self):
        self.assertEqual(release_tools_image.validation_errors(ROOT), [])

    def test_key_changes_with_dockerfile_content(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            (root / "Dockerfile.release-tools").write_text("FROM scratch\n")
            first = release_tools_image.image_key(root)
            (root / "Dockerfile.release-tools").write_text("FROM scratch\nLABEL changed=true\n")
            self.assertNotEqual(first, release_tools_image.image_key(root))


if __name__ == "__main__":
    unittest.main()
