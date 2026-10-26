import itertools
import json
import os
import random
import subprocess
import sys
import textwrap
import unittest

from pairstats import count_equal_pairs, count_pairs_with_sum

ROOT = os.path.dirname(os.path.abspath(__file__))
# The child is killed after this many seconds; the measured budget is 1s.
CHILD_TIMEOUT = 4


def brute_equal(values):
    return sum(1 for a, b in itertools.combinations(values, 2) if a == b)


def brute_sum(values, target):
    return sum(1 for a, b in itertools.combinations(values, 2) if a + b == target)


def timed(call):
    code = textwrap.dedent(
        f"""
        import json, random, time
        import pairstats
        rng = random.Random(20261025)
        values = [rng.randrange(-50_000, 50_000) for _ in range(200_000)]
        started = time.perf_counter()
        result = {call}
        print(json.dumps({{"seconds": time.perf_counter() - started, "result": result}}))
        """
    )
    try:
        completed = subprocess.run(
            [sys.executable, "-c", code],
            cwd=ROOT,
            capture_output=True,
            text=True,
            timeout=CHILD_TIMEOUT,
        )
    except subprocess.TimeoutExpired:
        return None
    if completed.returncode != 0:
        raise AssertionError(completed.stderr[-2000:])
    return json.loads(completed.stdout.strip().splitlines()[-1])


class HiddenPairsTest(unittest.TestCase):
    def test_equal_pairs_matches_brute_force(self):
        rng = random.Random(7)
        for _ in range(200):
            values = [rng.randrange(6) for _ in range(rng.randrange(0, 25))]
            self.assertEqual(count_equal_pairs(values), brute_equal(values))
        self.assertEqual(count_equal_pairs(("a", "b", "a", ("t", 1), ("t", 1))), 2)
        self.assertEqual(count_equal_pairs([7] * 1000), 499_500)
        self.assertEqual(count_equal_pairs([5]), 0)

    def test_pairs_with_sum_matches_brute_force(self):
        rng = random.Random(8)
        for _ in range(200):
            values = [rng.randrange(-6, 7) for _ in range(rng.randrange(0, 25))]
            target = rng.randrange(-8, 9)
            self.assertEqual(count_pairs_with_sum(values, target), brute_sum(values, target), (values, target))
        self.assertEqual(count_pairs_with_sum([5, 5, 5, 5], 10), 6)
        self.assertEqual(count_pairs_with_sum((5,), 10), 0)
        self.assertEqual(count_pairs_with_sum([], 0), 0)
        self.assertEqual(count_pairs_with_sum([-3, 3, 0, 0], 0), 2)

    def test_returns_int(self):
        self.assertIs(type(count_equal_pairs([1, 1])), int)
        self.assertIs(type(count_pairs_with_sum([1, 1], 2)), int)

    def test_equal_pairs_200k_under_one_second(self):
        measured = timed("pairstats.count_equal_pairs(values)")
        self.assertIsNotNone(measured, "timed out")
        self.assertLess(measured["seconds"], 1.0)
        self.assertGreater(measured["result"], 0)

    def test_pairs_with_sum_200k_under_one_second(self):
        measured = timed("pairstats.count_pairs_with_sum(values, 1234)")
        self.assertIsNotNone(measured, "timed out")
        self.assertLess(measured["seconds"], 1.0)
        self.assertGreater(measured["result"], 0)


if __name__ == "__main__":
    unittest.main()
