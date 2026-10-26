from . import pricing


def render_invoice(customer, items, tax_rate=0.0):
    lines = [f"Invoice for {customer}"]
    for price, quantity in items:
        lines.append(f"  {quantity} x {price:.2f}")
    lines.append(f"Total: {pricing.calc_total(items, tax_rate):.2f}")
    return "\n".join(lines)
