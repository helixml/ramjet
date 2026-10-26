def deep_merge(base, override):
    """Return ``base`` recursively updated with ``override``."""
    for key, value in override.items():
        if key in base and isinstance(base[key], dict) and isinstance(value, dict):
            deep_merge(base[key], value)
        elif key in base and isinstance(base[key], list) and isinstance(value, list):
            base[key] = base[key] + value
        else:
            base[key] = value
    return base


def merge_all(*configs):
    if not configs:
        return {}
    result = configs[0]
    for config in configs[1:]:
        result = deep_merge(result, config)
    return result
