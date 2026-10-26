# `merge_intervals` returns wrong results for several inputs

The booking service uses `intervals.merge_intervals` to collapse busy slots, and
we have had double-bookings traced back to it.

`merge_intervals(intervals)` takes a list of `(start, end)` pairs describing
**closed** intervals (both endpoints included) and should return the minimal
list of disjoint intervals covering exactly the same points, as a list of
`(start, end)` tuples sorted by start.

Reported problems:

- `merge_intervals([(1, 3), (3, 5)])` returns two intervals. Intervals that only
  touch at an endpoint share that point and must be merged into `(1, 5)`.
- `merge_intervals([(1, 10), (2, 3)])` returns `[(1, 3)]`, losing most of the
  first interval.
- Input that is not already sorted gives garbage.

Additional requirements:

- The caller's list (and the pairs in it) must not be modified.
- A pair with `start > end` is invalid and must raise `ValueError`.
  Zero-length intervals such as `(4, 4)` are valid.
- An empty input returns `[]`.
- Numbers may be ints or floats, including negatives.

The implementation is in `intervals/merge.py`.
