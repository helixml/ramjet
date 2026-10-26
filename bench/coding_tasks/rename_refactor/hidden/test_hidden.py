import os
import re
import unittest
import warnings

import shop
from shop import pricing

PACKAGE_DIR = os.path.dirname(os.path.abspath(shop.__file__))


class HiddenRenameTest(unittest.TestCase):
    def test_new_name_exists_and_matches(self):
        items = [(10.0, 2), (5.0, 1)]
        self.assertEqual(pricing.compute_order_total(items, 0.1), 27.5)
        self.assertEqual(pricing.compute_order_total(items, tax_rate=0.1, discount=0.5), 13.75)
        self.assertIs(shop.compute_order_total, pricing.compute_order_total)
        self.assertIn("compute_order_total", shop.__all__)

    def test_alias_warns_and_forwards(self):
        items = [(10.0, 2), (5.0, 1)]
        with warnings.catch_warnings(record=True) as caught:
            warnings.simplefilter("always")
            self.assertEqual(pricing.calc_total(items, 0.1), 27.5)
            self.assertEqual(shop.calc_total(items, tax_rate=0.1, discount=0.5), 13.75)
        deprecations = [w for w in caught if issubclass(w.category, DeprecationWarning)]
        self.assertEqual(len(deprecations), 2)
        for warning in deprecations:
            self.assertIn("compute_order_total", str(warning.message))
            self.assertEqual(os.path.abspath(warning.filename), os.path.abspath(__file__))

    def test_package_use_emits_no_deprecation(self):
        with warnings.catch_warnings():
            warnings.simplefilter("error", DeprecationWarning)
            cart = shop.Cart(tax_rate=0.1)
            cart.add("ball", 10.0, 2)
            cart.add("ball", 10.0, 1)
            self.assertEqual(cart.total(), 33.0)
            self.assertEqual(cart.total(discount=0.5), 16.5)
            self.assertEqual(cart.total_with_shipping(), 37.99)
            self.assertTrue(shop.render_invoice("Ada", [(2.5, 4)], 0.2).endswith("Total: 12.00"))
            self.assertEqual(shop.daily_revenue([[(1.0, 1)], [(2.0, 2)]], tax_rate=0.5), 7.5)

    def test_no_internal_callers_of_old_name(self):
        offenders = []
        for name in sorted(os.listdir(PACKAGE_DIR)):
            if not name.endswith(".py") or name in ("pricing.py", "__init__.py"):
                continue
            with open(os.path.join(PACKAGE_DIR, name), encoding="utf-8") as handle:
                if re.search(r"\bcalc_total\b", handle.read()):
                    offenders.append(name)
        self.assertEqual(offenders, [])

    def test_shipping_unchanged(self):
        self.assertEqual(pricing.shipping_cost(50), 0.0)
        self.assertEqual(pricing.shipping_cost(49.99), 4.99)


if __name__ == "__main__":
    unittest.main()
