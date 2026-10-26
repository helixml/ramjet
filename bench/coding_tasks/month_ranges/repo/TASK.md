# Monthly billing periods are wrong around month ends and leap years

Our billing job splits a date range into calendar-month periods with the
helpers in `dateutil2/months.py`. Customers billed from 31 January got a
crash, February periods are a day short in leap years, and long ranges skip
or repeat months.

The intended behaviour of each function (all dates are `datetime.date`):

- `month_end(d)` — the last day of `d`'s month. Leap years follow the
  Gregorian rules (2024 and 2000 are leap years, 2023 and 2100 are not).
- `add_months(d, n)` — the same day-of-month `n` months later (`n` may be zero
  or negative). If that day does not exist in the target month it is clamped
  to the target month's last day: `add_months(date(2024, 1, 31), 1)` is
  `date(2024, 2, 29)`, and `add_months(date(2024, 3, 31), -1)` is
  `date(2024, 2, 29)`. Year boundaries work in both directions.
- `month_starts(start, end)` — a list of the first day of every calendar
  month that intersects the inclusive range `[start, end]`, in order.
- `iter_month_ranges(start, end)` — a list of `(period_start, period_end)`
  tuples, one per calendar month intersecting `[start, end]`, clipped to that
  range. Consecutive periods are contiguous (no gaps, no overlap) and together
  cover exactly `[start, end]`.

`month_starts` and `iter_month_ranges` raise `ValueError` if `end < start`; a
single-day range (`start == end`) is valid.
