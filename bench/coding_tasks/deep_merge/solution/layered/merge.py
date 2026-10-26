import copy


def deep_merge(base, override):
    """Return a new dict: ``base`` recursively updated with ``override``."""
    result = copy.deepcopy(base)
    for key, value in override.items():
        if value is None:
            result.pop(key, None)
        elif isinstance(value, dict):
            current = result.get(key)
            result[key] = deep_merge(current if isinstance(current, dict) else {}, value)
        else:
            result[key] = copy.deepcopy(value)
    return result


def merge_all(*configs):
    result = {}
    for config in configs:
        result = deep_merge(result, config)
    return result
