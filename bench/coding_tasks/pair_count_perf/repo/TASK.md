# Pair statistics are far too slow on real data

`pairstats.count_equal_pairs` and `pairstats.count_pairs_with_sum` are
correct on small inputs, but the nightly job now feeds them 200,000 values and
never finishes.

Please make both functions fast enough that each handles **200,000 values in
well under one second** on an ordinary machine, without changing their
results:

- `count_equal_pairs(values)` returns the number of index pairs `i < j` with
  `values[i] == values[j]`. Values may be any hashable objects that compare
  by equality as usual (e.g. ints, strings, tuples).
- `count_pairs_with_sum(values, target)` returns the number of index pairs
  `i < j` with `values[i] + values[j] == target`, for integer `values` and
  integer `target` (negative numbers included).

Both accept any sequence (lists and tuples) and return an `int`; empty and
single-element inputs give `0`. Do not add third-party dependencies.
