def count_equal_pairs(values):
    """Number of index pairs i < j with values[i] == values[j]."""
    count = 0
    for i in range(len(values)):
        for j in range(i + 1, len(values)):
            if values[i] == values[j]:
                count += 1
    return count


def count_pairs_with_sum(values, target):
    """Number of index pairs i < j with values[i] + values[j] == target."""
    count = 0
    for i in range(len(values)):
        for j in range(i + 1, len(values)):
            if values[i] + values[j] == target:
                count += 1
    return count
