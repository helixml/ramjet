import unittest

from layered import deep_merge


class DeepMergeTest(unittest.TestCase):
    def test_nested_dicts_merge(self):
        base = {"db": {"host": "localhost", "port": 5432}, "debug": False}
        override = {"db": {"host": "db.internal"}, "debug": True}
        self.assertEqual(
            deep_merge(base, override),
            {"db": {"host": "db.internal", "port": 5432}, "debug": True},
        )

    def test_lists_are_replaced(self):
        self.assertEqual(deep_merge({"hosts": ["a", "b"]}, {"hosts": ["c"]}), {"hosts": ["c"]})


if __name__ == "__main__":
    unittest.main()
