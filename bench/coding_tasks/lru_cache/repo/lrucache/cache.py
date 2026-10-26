from collections import OrderedDict


class LRUCache:
    """Least-recently-used cache holding at most ``capacity`` entries."""

    def __init__(self, capacity):
        if capacity < 1:
            raise ValueError("capacity must be positive")
        self.capacity = capacity
        self._data = OrderedDict()

    def get(self, key, default=None):
        if key in self._data:
            return self._data[key]
        return default

    def put(self, key, value):
        self._data[key] = value
        if len(self._data) >= self.capacity:
            self._data.popitem(last=False)

    def __contains__(self, key):
        return key in self._data

    def __len__(self):
        return len(self._data)
