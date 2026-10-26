from collections import Counter


def count_equal_pairs(values):
    """Number of index pairs i < j with values[i] == values[j]."""
    return sum(n * (n - 1) // 2 for n in Counter(values).values())


def count_pairs_with_sum(values, target):
    """Number of index pairs i < j with values[i] + values[j] == target."""
    seen = Counter()
    count = 0
    for value in values:
        count += seen[target - value]
        seen[value] += 1
    return count
