"""Calendar-month helpers used by the billing job."""

import calendar
from datetime import date


def month_end(d):
    return d.replace(day=calendar.monthrange(d.year, d.month)[1])


def add_months(d, n):
    index = d.year * 12 + (d.month - 1) + n
    year, month = divmod(index, 12)
    month += 1
    last = calendar.monthrange(year, month)[1]
    return date(year, month, min(d.day, last))


def month_starts(start, end):
    if end < start:
        raise ValueError("end must not be before start")
    current = start.replace(day=1)
    starts = []
    while current <= end:
        starts.append(current)
        current = add_months(current, 1)
    return starts


def iter_month_ranges(start, end):
    ranges = []
    for first in month_starts(start, end):
        ranges.append((max(first, start), min(month_end(first), end)))
    return ranges
