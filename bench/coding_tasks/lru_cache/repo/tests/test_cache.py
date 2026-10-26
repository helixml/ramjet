import unittest

from lrucache import LRUCache


class LRUCacheTest(unittest.TestCase):
    def test_holds_capacity_entries(self):
        cache = LRUCache(2)
        cache.put("a", 1)
        cache.put("b", 2)
        self.assertEqual(cache.get("a"), 1)
        self.assertEqual(cache.get("b"), 2)

    def test_rejects_non_positive_capacity(self):
        with self.assertRaises(ValueError):
            LRUCache(0)


if __name__ == "__main__":
    unittest.main()
