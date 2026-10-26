import copy
import random
import unittest

from intervals import merge_intervals


def covered(intervals, points):
    return {p for p in points if any(s <= p <= e for s, e in intervals)}


class HiddenMergeIntervalsTest(unittest.TestCase):
    def test_touching(self):
        self.assertEqual(merge_intervals([(1, 3), (3, 5)]), [(1, 5)])

    def test_nested(self):
        self.assertEqual(merge_intervals([(1, 10), (2, 3)]), [(1, 10)])
        self.assertEqual(merge_intervals([(1, 10), (2, 3), (4, 12)]), [(1, 12)])

    def test_unsorted(self):
        self.assertEqual(
            merge_intervals([(8, 9), (1, 3), (2, 4), (6, 7)]),
            [(1, 4), (6, 7), (8, 9)],
        )

    def test_disjoint_gap_kept(self):
        self.assertEqual(merge_intervals([(1, 2), (3, 4)]), [(1, 2), (3, 4)])

    def test_zero_length_and_negative(self):
        self.assertEqual(merge_intervals([(4, 4)]), [(4, 4)])
        self.assertEqual(merge_intervals([(-5, -1), (-1, -1), (0, 0)]), [(-5, -1), (0, 0)])

    def test_floats(self):
        self.assertEqual(merge_intervals([(0.5, 1.5), (1.5, 2.25)]), [(0.5, 2.25)])

    def test_returns_tuples(self):
        result = merge_intervals([[1, 2], [5, 6]])
        self.assertEqual(result, [(1, 2), (5, 6)])
        self.assertTrue(all(isinstance(item, tuple) for item in result))

    def test_input_not_mutated(self):
        data = [[5, 7], [1, 3], [2, 6]]
        snapshot = copy.deepcopy(data)
        merge_intervals(data)
        self.assertEqual(data, snapshot)

    def test_invalid_interval(self):
        with self.assertRaises(ValueError):
            merge_intervals([(1, 2), (5, 3)])

    def test_empty(self):
        self.assertEqual(merge_intervals([]), [])

    def test_random_against_point_model(self):
        rng = random.Random(1234)
        points = range(-2, 45)
        for _ in range(300):
            data = []
            for _ in range(rng.randint(1, 8)):
                start = rng.randint(0, 40)
                data.append((start, start + rng.randint(0, 6)))
            result = merge_intervals(data)
            self.assertEqual(covered(result, points), covered(data, points))
            for (s1, e1), (s2, e2) in zip(result, result[1:]):
                self.assertLess(e1, s2)


if __name__ == "__main__":
    unittest.main()
