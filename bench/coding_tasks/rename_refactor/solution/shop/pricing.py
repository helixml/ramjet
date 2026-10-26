import warnings


def compute_order_total(items, tax_rate=0.0, discount=0.0):
    """Return the order total for ``items``.

    ``items`` is an iterable of ``(unit_price, quantity)`` pairs. ``discount``
    is a fraction taken off the subtotal before tax. The result is rounded to
    cents.
    """
    subtotal = sum(price * quantity for price, quantity in items)
    discounted = subtotal * (1 - discount)
    return round(discounted * (1 + tax_rate), 2)


def calc_total(*args, **kwargs):
    """Deprecated alias of :func:`compute_order_total`."""
    warnings.warn(
        "calc_total is deprecated; use compute_order_total",
        DeprecationWarning,
        stacklevel=2,
    )
    return compute_order_total(*args, **kwargs)


def shipping_cost(total):
    """Orders of 50 or more ship free."""
    return 0.0 if total >= 50 else 4.99
