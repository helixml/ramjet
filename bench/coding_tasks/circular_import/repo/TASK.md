# `import inventory` fails with ImportError

After the last refactor the package no longer imports:

```
$ python3 -c "import inventory"
ImportError: cannot import name 'Item' from partially initialized module
'inventory.models' (most likely due to a circular import)
```

Please fix the import problem **without changing the public API**. All of
these must keep working, in any import order and in a fresh interpreter:

- `from inventory import Item, Warehouse, validate_item, validate_sku`
- `import inventory.models` / `from inventory.models import Item, Warehouse`
- `import inventory.validation` /
  `from inventory.validation import validate_item, validate_sku`

`inventory.Item` must be the same class object as `inventory.models.Item`, and
the same holds for the other names. Behaviour must not change:
`validate_item` raises `TypeError` for anything that is not an `Item` and
`ValueError` for a bad SKU or a negative quantity; `validate_sku` raises
`ValueError` unless the SKU is three upper-case letters, a dash, and four
digits (e.g. `ABC-1234`); `Warehouse.add` validates the item before storing
it.
