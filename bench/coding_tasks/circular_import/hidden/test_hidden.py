import os
import subprocess
import sys
import textwrap
import unittest

ROOT = os.path.dirname(os.path.abspath(__file__))


def fresh(code):
    return subprocess.run(
        [sys.executable, "-c", textwrap.dedent(code)],
        cwd=ROOT,
        capture_output=True,
        text=True,
        timeout=60,
    )


class HiddenCircularImportTest(unittest.TestCase):
    def assertFresh(self, code):
        completed = fresh(code)
        self.assertEqual(completed.returncode, 0, completed.stderr[-2000:])

    def test_import_orders(self):
        orders = [
            "import inventory",
            "import inventory.models",
            "import inventory.validation",
            "from inventory.validation import validate_item, validate_sku",
            "from inventory.models import Item, Warehouse",
            "from inventory import *\nassert Item and Warehouse and validate_item and validate_sku",
            "import inventory.validation\nimport inventory.models\nimport inventory",
        ]
        for code in orders:
            with self.subTest(code=code):
                self.assertFresh(code)

    def test_identity_of_public_names(self):
        self.assertFresh(
            """
            import inventory.validation as validation
            import inventory.models as models
            import inventory
            assert inventory.Item is models.Item
            assert inventory.Warehouse is models.Warehouse
            assert inventory.validate_item is validation.validate_item
            assert inventory.validate_sku is validation.validate_sku
            assert sorted(inventory.__all__) == ["Item", "Warehouse", "validate_item", "validate_sku"]
            """
        )

    def test_behaviour(self):
        self.assertFresh(
            """
            from inventory.validation import validate_item, validate_sku
            from inventory import Item, Warehouse

            def raises(exc, fn, *args):
                try:
                    fn(*args)
                except exc:
                    return True
                return False

            assert validate_sku("ABC-1234") == "ABC-1234"
            for bad in ("abc-1234", "AB-1234", "ABCD-1234", "ABC-123", "ABC-12345", 1234, None):
                assert raises(ValueError, validate_sku, bad), bad
            assert raises(TypeError, validate_item, object())
            assert raises(TypeError, validate_item, {"sku": "ABC-1234"})
            assert raises(ValueError, Item, "bad", "thing")
            item = Item("XYZ-0001", "nut", 3)
            assert validate_item(item) is item
            item.quantity = -1
            assert raises(ValueError, validate_item, item)
            warehouse = Warehouse()
            assert raises(ValueError, warehouse.add, item)
            assert raises(TypeError, warehouse.add, "XYZ-0001")
            warehouse.add(Item("XYZ-0001", "nut", 3))
            warehouse.add(Item("XYZ-0001", "nut", 4))
            warehouse.add(Item("QQQ-9999", "washer"))
            assert warehouse.quantity("XYZ-0001") == 7
            assert warehouse.quantity("QQQ-9999") == 0
            assert warehouse.quantity("NOP-0000") == 0
            assert len(warehouse) == 2
            """
        )


if __name__ == "__main__":
    unittest.main()
