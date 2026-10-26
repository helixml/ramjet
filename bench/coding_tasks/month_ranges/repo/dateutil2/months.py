"""Calendar-month helpers used by the billing job."""

from datetime import date, timedelta

DAYS_IN_MONTH = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]


def month_end(d):
    return d.replace(day=DAYS_IN_MONTH[d.month - 1])


def add_months(d, n):
    month = d.month + n
    year = d.year + month // 12
    month = month % 12
    return d.replace(year=year, month=month)


def month_starts(start, end):
    current = start.replace(day=1)
    starts = []
    while current < end:
        starts.append(current)
        current = current + timedelta(days=30)
    return starts


def iter_month_ranges(start, end):
    ranges = []
    for first in month_starts(start, end):
        ranges.append((max(first, start), min(month_end(first), end)))
    return ranges
