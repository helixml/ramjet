import unittest


class InventoryTest(unittest.TestCase):
    def test_package_imports(self):
        import inventory

        self.assertTrue(hasattr(inventory, "Warehouse"))

    def test_warehouse_add(self):
        from inventory import Item, Warehouse

        warehouse = Warehouse()
        warehouse.add(Item("ABC-1234", "bolt", 5))
        warehouse.add(Item("ABC-1234", "bolt", 2))
        self.assertEqual(warehouse.quantity("ABC-1234"), 7)


if __name__ == "__main__":
    unittest.main()
