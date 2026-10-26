def merge_intervals(intervals):
    """Merge overlapping closed intervals given as (start, end) pairs."""
    pairs = []
    for start, end in intervals:
        if start > end:
            raise ValueError(f"invalid interval ({start!r}, {end!r})")
        pairs.append((start, end))
    pairs.sort()
    result = []
    for start, end in pairs:
        if result and start <= result[-1][1]:
            if end > result[-1][1]:
                result[-1] = (result[-1][0], end)
        else:
            result.append((start, end))
    return result
