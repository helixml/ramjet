"""Token-bucket rate limiting. See TASK.md for the specification."""

import time


class TokenBucket:
    def __init__(self, capacity, refill_rate, clock=time.monotonic):
        raise NotImplementedError

    @property
    def tokens(self):
        raise NotImplementedError

    def allow(self, n=1):
        raise NotImplementedError

    def retry_after(self, n=1):
        raise NotImplementedError
