import random
import time
import unittest

from pairstats import count_equal_pairs, count_pairs_with_sum


class PairsTest(unittest.TestCase):
    def test_equal_pairs_small(self):
        self.assertEqual(count_equal_pairs([1, 2, 1, 1, 3]), 3)
        self.assertEqual(count_equal_pairs([]), 0)

    def test_pairs_with_sum_small(self):
        self.assertEqual(count_pairs_with_sum([1, 4, 2, 3, 0, 5], 5), 3)

    def test_ten_thousand_values_quickly(self):
        values = [random.randrange(1000) for _ in range(10_000)]
        started = time.perf_counter()
        count_equal_pairs(values)
        count_pairs_with_sum(values, 1000)
        self.assertLess(time.perf_counter() - started, 1.0)


if __name__ == "__main__":
    unittest.main()
