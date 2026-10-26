import unittest
from datetime import date

from dateutil2 import add_months, iter_month_ranges, month_end


class MonthsTest(unittest.TestCase):
    def test_month_end(self):
        self.assertEqual(month_end(date(2023, 2, 10)), date(2023, 2, 28))
        self.assertEqual(month_end(date(2023, 4, 1)), date(2023, 4, 30))

    def test_add_months_simple(self):
        self.assertEqual(add_months(date(2024, 1, 15), 1), date(2024, 2, 15))

    def test_add_months_clamps(self):
        self.assertEqual(add_months(date(2024, 1, 31), 1), date(2024, 2, 29))

    def test_ranges_within_one_month(self):
        self.assertEqual(
            iter_month_ranges(date(2023, 5, 3), date(2023, 5, 20)),
            [(date(2023, 5, 3), date(2023, 5, 20))],
        )


if __name__ == "__main__":
    unittest.main()
