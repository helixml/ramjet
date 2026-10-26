import unittest

from intervals import merge_intervals


class MergeIntervalsTest(unittest.TestCase):
    def test_overlapping(self):
        self.assertEqual(merge_intervals([(1, 4), (2, 6), (8, 9)]), [(1, 6), (8, 9)])

    def test_touching_intervals_merge(self):
        self.assertEqual(merge_intervals([(1, 3), (3, 5)]), [(1, 5)])

    def test_empty(self):
        self.assertEqual(merge_intervals([]), [])


if __name__ == "__main__":
    unittest.main()
