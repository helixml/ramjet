"""Token-bucket rate limiting. See TASK.md for the specification."""

import math
import time


class TokenBucket:
    def __init__(self, capacity, refill_rate, clock=time.monotonic):
        if not capacity > 0:
            raise ValueError("capacity must be positive")
        if not refill_rate >= 0:
            raise ValueError("refill_rate must be non-negative")
        self.capacity = capacity
        self.refill_rate = refill_rate
        self._clock = clock
        self._tokens = float(capacity)
        self._last = clock()

    def _refill(self):
        now = self._clock()
        elapsed = now - self._last
        if elapsed > 0:
            self._tokens = min(float(self.capacity), self._tokens + elapsed * self.refill_rate)
            self._last = now
        elif elapsed < 0:
            self._last = now

    @property
    def tokens(self):
        self._refill()
        return self._tokens

    @staticmethod
    def _check(n):
        if not n > 0:
            raise ValueError("n must be positive")

    def allow(self, n=1):
        self._check(n)
        self._refill()
        if n > self.capacity or self._tokens < n:
            return False
        self._tokens -= n
        return True

    def retry_after(self, n=1):
        self._check(n)
        self._refill()
        if n > self.capacity:
            return math.inf
        missing = n - self._tokens
        if missing <= 0:
            return 0.0
        if self.refill_rate == 0:
            return math.inf
        return missing / self.refill_rate
