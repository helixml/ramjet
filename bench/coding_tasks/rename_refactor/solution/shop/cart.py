from .pricing import compute_order_total, shipping_cost


class Cart:
    def __init__(self, tax_rate=0.0):
        self.tax_rate = tax_rate
        self.lines = {}

    def add(self, sku, unit_price, quantity=1):
        price, count = self.lines.get(sku, (unit_price, 0))
        self.lines[sku] = (price, count + quantity)

    def total(self, discount=0.0):
        return compute_order_total(self.lines.values(), tax_rate=self.tax_rate, discount=discount)

    def total_with_shipping(self):
        total = self.total()
        return round(total + shipping_cost(total), 2)
