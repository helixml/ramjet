import functools

from . import pricing


def daily_revenue(orders, tax_rate=0.0):
    """Sum of order totals; ``orders`` is an iterable of item lists."""
    total_for = functools.partial(pricing.calc_total, tax_rate=tax_rate)
    return round(sum(total_for(items) for items in orders), 2)
