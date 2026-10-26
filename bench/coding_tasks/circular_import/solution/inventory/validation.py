import re

SKU_PATTERN = re.compile(r"[A-Z]{3}-\d{4}")


def validate_sku(sku):
    if not isinstance(sku, str) or not SKU_PATTERN.fullmatch(sku):
        raise ValueError(f"invalid SKU: {sku!r}")
    return sku


def validate_item(item):
    from .models import Item

    if not isinstance(item, Item):
        raise TypeError("expected an Item")
    validate_sku(item.sku)
    if item.quantity < 0:
        raise ValueError("quantity must not be negative")
    return item
