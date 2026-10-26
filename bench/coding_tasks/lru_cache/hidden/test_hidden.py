import unittest

from lrucache import LRUCache


class HiddenLRUCacheTest(unittest.TestCase):
    def test_holds_exactly_capacity(self):
        cache = LRUCache(3)
        for key in "abc":
            cache.put(key, key.upper())
        self.assertEqual(len(cache), 3)
        self.assertTrue(all(key in cache for key in "abc"))
        cache.put("d", "D")
        self.assertEqual(len(cache), 3)
        self.assertNotIn("a", cache)

    def test_get_refreshes_recency(self):
        cache = LRUCache(2)
        cache.put("a", 1)
        cache.put("b", 2)
        self.assertEqual(cache.get("a"), 1)
        cache.put("c", 3)
        self.assertIn("a", cache)
        self.assertNotIn("b", cache)
        self.assertIn("c", cache)

    def test_put_existing_refreshes_and_updates(self):
        cache = LRUCache(2)
        cache.put("a", 1)
        cache.put("b", 2)
        cache.put("a", 10)
        cache.put("c", 3)
        self.assertEqual(cache.get("a"), 10)
        self.assertNotIn("b", cache)

    def test_update_in_full_cache_never_evicts(self):
        cache = LRUCache(2)
        cache.put("a", 1)
        cache.put("b", 2)
        cache.put("b", 20)
        self.assertEqual(len(cache), 2)
        self.assertEqual(cache.get("a"), 1)
        self.assertEqual(cache.get("b"), 20)

    def test_capacity_one(self):
        cache = LRUCache(1)
        cache.put("a", 1)
        self.assertEqual(cache.get("a"), 1)
        cache.put("b", 2)
        self.assertNotIn("a", cache)
        self.assertEqual(cache.get("b"), 2)

    def test_miss_returns_default_without_insert(self):
        cache = LRUCache(2)
        self.assertIsNone(cache.get("x"))
        self.assertEqual(cache.get("x", 42), 42)
        self.assertEqual(len(cache), 0)
        self.assertNotIn("x", cache)

    def test_contains_does_not_refresh(self):
        cache = LRUCache(2)
        cache.put("a", 1)
        cache.put("b", 2)
        self.assertIn("a", cache)
        cache.put("c", 3)
        self.assertNotIn("a", cache)
        self.assertIn("b", cache)

    def test_falsy_values_are_hits(self):
        cache = LRUCache(2)
        cache.put("zero", 0)
        cache.put("none", None)
        self.assertEqual(cache.get("zero", "miss"), 0)
        self.assertIsNone(cache.get("none", "miss"))
        cache.put("new", 1)
        self.assertNotIn("zero", cache)

    def test_negative_capacity(self):
        with self.assertRaises(ValueError):
            LRUCache(-1)


if __name__ == "__main__":
    unittest.main()
