from dataclasses import dataclass

from .validation import validate_item, validate_sku


@dataclass
class Item:
    sku: str
    name: str
    quantity: int = 0

    def __post_init__(self):
        validate_sku(self.sku)


class Warehouse:
    def __init__(self):
        self._items = {}

    def add(self, item):
        validate_item(item)
        existing = self._items.get(item.sku)
        if existing is None:
            self._items[item.sku] = Item(item.sku, item.name, item.quantity)
        else:
            existing.quantity += item.quantity

    def quantity(self, sku):
        item = self._items.get(sku)
        return 0 if item is None else item.quantity

    def __len__(self):
        return len(self._items)
