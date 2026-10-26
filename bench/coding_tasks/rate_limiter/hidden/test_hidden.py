import math
import unittest

from ratelimit import TokenBucket


class FakeClock:
    def __init__(self, now=100.0):
        self.now = now

    def __call__(self):
        return self.now


class HiddenTokenBucketTest(unittest.TestCase):
    def setUp(self):
        self.clock = FakeClock()

    def bucket(self, capacity=4, rate=2.0):
        return TokenBucket(capacity, rate, clock=self.clock)

    def test_starts_full(self):
        bucket = self.bucket()
        self.assertAlmostEqual(bucket.tokens, 4.0)
        self.assertTrue(bucket.allow(4))
        self.assertFalse(bucket.allow())

    def test_fractional_refill(self):
        bucket = self.bucket()
        bucket.allow(4)
        self.clock.now += 0.25
        self.assertAlmostEqual(bucket.tokens, 0.5)
        self.assertFalse(bucket.allow())
        self.clock.now += 0.25
        self.assertTrue(bucket.allow())
        self.assertAlmostEqual(bucket.tokens, 0.0)

    def test_refill_capped_at_capacity(self):
        bucket = self.bucket()
        bucket.allow(1)
        self.clock.now += 1000
        self.assertAlmostEqual(bucket.tokens, 4.0)
        self.assertTrue(bucket.allow(4))
        self.assertFalse(bucket.allow(1))

    def test_all_or_nothing(self):
        bucket = self.bucket()
        bucket.allow(2)
        self.assertFalse(bucket.allow(3))
        self.assertAlmostEqual(bucket.tokens, 2.0)
        self.assertTrue(bucket.allow(2))

    def test_more_than_capacity_never_allowed(self):
        bucket = self.bucket()
        self.assertFalse(bucket.allow(5))
        self.clock.now += 1000
        self.assertFalse(bucket.allow(5))
        self.assertTrue(bucket.allow(4))

    def test_fractional_requests(self):
        bucket = TokenBucket(1.5, 0.5, clock=self.clock)
        self.assertTrue(bucket.allow(1.5))
        self.clock.now += 1.0
        self.assertTrue(bucket.allow(0.5))
        self.assertFalse(bucket.allow(0.5))

    def test_invalid_n(self):
        bucket = self.bucket()
        for n in (0, -1):
            with self.assertRaises(ValueError):
                bucket.allow(n)
            with self.assertRaises(ValueError):
                bucket.retry_after(n)

    def test_invalid_constructor(self):
        for capacity, rate in ((0, 1), (-1, 1), (1, -0.5)):
            with self.assertRaises(ValueError):
                TokenBucket(capacity, rate, clock=self.clock)
        TokenBucket(1, 0, clock=self.clock)

    def test_clock_going_backwards(self):
        bucket = self.bucket()
        bucket.allow(3)
        self.clock.now -= 50
        self.assertAlmostEqual(bucket.tokens, 1.0)
        self.assertTrue(bucket.allow(1))
        self.assertFalse(bucket.allow(1))
        self.clock.now += 0.5
        self.assertAlmostEqual(bucket.tokens, 1.0)

    def test_retry_after(self):
        bucket = self.bucket()
        self.assertEqual(bucket.retry_after(), 0.0)
        bucket.allow(4)
        self.assertAlmostEqual(bucket.retry_after(1), 0.5)
        self.assertAlmostEqual(bucket.retry_after(3), 1.5)
        self.assertEqual(bucket.retry_after(5), math.inf)
        self.assertAlmostEqual(bucket.tokens, 0.0)
        self.clock.now += 0.5
        self.assertEqual(bucket.retry_after(1), 0.0)
        self.assertTrue(bucket.allow(1))

    def test_zero_refill_rate(self):
        bucket = TokenBucket(2, 0, clock=self.clock)
        self.assertTrue(bucket.allow(2))
        self.clock.now += 1e6
        self.assertFalse(bucket.allow())
        self.assertEqual(bucket.retry_after(), math.inf)

    def test_default_clock(self):
        bucket = TokenBucket(1, 1000.0)
        self.assertTrue(bucket.allow())
        self.assertIsInstance(bucket.tokens, float)


if __name__ == "__main__":
    unittest.main()
