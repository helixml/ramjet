# Rename `calc_total` to `compute_order_total`

`shop.pricing.calc_total` is the function that turns line items into an order
total. The name is ambiguous next to the new tax and shipping helpers, so we
are renaming it to `compute_order_total`.

Please:

1. Rename the function in `shop/pricing.py` to `compute_order_total`, keeping
   its signature (`items, tax_rate=0.0, discount=0.0`) and behaviour identical.
2. Update **every** caller inside the `shop` package to use the new name.
   Nothing inside the package may call the old name any more.
3. Keep `calc_total` working for external users as a deprecated alias: it must
   accept the same positional and keyword arguments, return the same result,
   and emit a `DeprecationWarning` whose message mentions
   `compute_order_total`. The warning must be attributed to the caller's line
   (i.e. use an appropriate `stacklevel`), not to `pricing.py`.
4. Export `compute_order_total` from the top-level `shop` package (alongside
   the existing exports, which must keep working, including `shop.calc_total`).

Using the package normally (carts, invoices, reports) must not emit any
`DeprecationWarning`.
