import unittest
from datetime import date, timedelta

from dateutil2 import add_months, iter_month_ranges, month_end, month_starts


class HiddenMonthsTest(unittest.TestCase):
    def test_month_end_leap_rules(self):
        self.assertEqual(month_end(date(2024, 2, 1)), date(2024, 2, 29))
        self.assertEqual(month_end(date(2000, 2, 1)), date(2000, 2, 29))
        self.assertEqual(month_end(date(2100, 2, 1)), date(2100, 2, 28))
        self.assertEqual(month_end(date(2023, 12, 31)), date(2023, 12, 31))

    def test_add_months(self):
        cases = [
            (date(2024, 1, 31), 1, date(2024, 2, 29)),
            (date(2023, 1, 31), 1, date(2023, 2, 28)),
            (date(2024, 3, 31), -1, date(2024, 2, 29)),
            (date(2023, 12, 15), 1, date(2024, 1, 15)),
            (date(2023, 11, 30), 1, date(2023, 12, 30)),
            (date(2024, 1, 10), -1, date(2023, 12, 10)),
            (date(2024, 1, 10), -13, date(2022, 12, 10)),
            (date(2024, 2, 29), 12, date(2025, 2, 28)),
            (date(2024, 2, 29), 48, date(2028, 2, 29)),
            (date(2024, 5, 31), 0, date(2024, 5, 31)),
            (date(2024, 8, 31), 1, date(2024, 9, 30)),
            (date(2023, 12, 31), 24, date(2025, 12, 31)),
        ]
        for start, months, expected in cases:
            with self.subTest(start=start, months=months):
                self.assertEqual(add_months(start, months), expected)

    def test_month_starts_across_year(self):
        self.assertEqual(
            month_starts(date(2023, 11, 30), date(2024, 2, 1)),
            [date(2023, 11, 1), date(2023, 12, 1), date(2024, 1, 1), date(2024, 2, 1)],
        )

    def test_month_starts_long_range_has_every_month(self):
        starts = month_starts(date(2020, 1, 31), date(2024, 12, 31))
        self.assertEqual(len(starts), 60)
        self.assertEqual(len(set(starts)), 60)
        self.assertTrue(all(d.day == 1 for d in starts))

    def test_single_day(self):
        self.assertEqual(month_starts(date(2024, 3, 31), date(2024, 3, 31)), [date(2024, 3, 1)])
        self.assertEqual(
            iter_month_ranges(date(2024, 3, 31), date(2024, 3, 31)),
            [(date(2024, 3, 31), date(2024, 3, 31))],
        )

    def test_ranges_clip_and_cover(self):
        ranges = iter_month_ranges(date(2024, 1, 31), date(2024, 3, 1))
        self.assertEqual(
            ranges,
            [
                (date(2024, 1, 31), date(2024, 1, 31)),
                (date(2024, 2, 1), date(2024, 2, 29)),
                (date(2024, 3, 1), date(2024, 3, 1)),
            ],
        )

    def test_ranges_contiguous_over_years(self):
        start, end = date(2019, 12, 17), date(2025, 3, 4)
        ranges = iter_month_ranges(start, end)
        self.assertEqual(ranges[0][0], start)
        self.assertEqual(ranges[-1][1], end)
        for (_, previous_end), (next_start, _) in zip(ranges, ranges[1:]):
            self.assertEqual(previous_end + timedelta(days=1), next_start)
        self.assertEqual(len(ranges), 64)

    def test_reversed_range(self):
        with self.assertRaises(ValueError):
            month_starts(date(2024, 2, 2), date(2024, 2, 1))
        with self.assertRaises(ValueError):
            iter_month_ranges(date(2024, 2, 2), date(2024, 2, 1))


if __name__ == "__main__":
    unittest.main()
