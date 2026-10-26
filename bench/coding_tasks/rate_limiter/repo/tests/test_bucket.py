import unittest

from ratelimit import TokenBucket


class FakeClock:
    def __init__(self):
        self.now = 0.0

    def __call__(self):
        return self.now


class TokenBucketTest(unittest.TestCase):
    def test_starts_full_then_denies(self):
        clock = FakeClock()
        bucket = TokenBucket(3, 1.0, clock=clock)
        self.assertEqual([bucket.allow() for _ in range(4)], [True, True, True, False])

    def test_refills_over_time(self):
        clock = FakeClock()
        bucket = TokenBucket(2, 1.0, clock=clock)
        bucket.allow(2)
        self.assertFalse(bucket.allow())
        clock.now = 1.0
        self.assertTrue(bucket.allow())


if __name__ == "__main__":
    unittest.main()
