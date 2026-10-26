# LRU cache evicts too early and forgets what was just used

`lrucache.LRUCache` is used to cache expensive lookups, but in production we see
far more misses than expected.

Reproduction:

```python
from lrucache import LRUCache

cache = LRUCache(2)
cache.put("a", 1)
cache.put("b", 2)
"a" in cache   # False -- but the cache has capacity for two entries!
```

and separately:

```python
cache = LRUCache(2)
cache.put("a", 1)
cache.put("b", 2)   # (once eviction is fixed)
cache.get("a")      # we just used "a" ...
cache.put("c", 3)   # ... so "b" should be evicted, but "a" is
```

Expected behaviour (this is the documented contract):

- The cache holds **at most `capacity`** entries; it only evicts when inserting
  a *new* key into a full cache, and then evicts exactly one entry: the least
  recently used one.
- A successful `get(key)` marks `key` as most recently used.
- `put(key, value)` on an existing key updates the value, marks the key as most
  recently used, and never evicts anything.
- A `get` miss returns `default` (default `None`) and does not insert anything.
- `key in cache` and `len(cache)` are read-only: they do not affect recency.
- `LRUCache(0)` or a negative capacity raises `ValueError`.

Please fix the implementation in `lrucache/cache.py`. Keep the public API as-is.
