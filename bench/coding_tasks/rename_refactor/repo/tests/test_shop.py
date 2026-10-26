import unittest

import shop


class ShopTest(unittest.TestCase):
    def test_cart_total(self):
        cart = shop.Cart(tax_rate=0.1)
        cart.add("ball", 10.0, 2)
        cart.add("kite", 5.0)
        self.assertEqual(cart.total(), 27.5)
        self.assertEqual(cart.total_with_shipping(), 32.49)

    def test_invoice(self):
        text = shop.render_invoice("Ada", [(2.5, 4)], tax_rate=0.0)
        self.assertTrue(text.endswith("Total: 10.00"))

    def test_daily_revenue(self):
        self.assertEqual(shop.daily_revenue([[(1.0, 1)], [(2.0, 2)]]), 5.0)


if __name__ == "__main__":
    unittest.main()
