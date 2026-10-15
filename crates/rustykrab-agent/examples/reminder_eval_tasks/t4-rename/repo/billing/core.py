"""Core order arithmetic."""


def calc_total(items, tax_rate=0.0):
    """Sum quantity * unit price over items, then apply tax, rounded to cents."""
    subtotal = sum(qty * price for qty, price in items)
    return round(subtotal * (1 + tax_rate), 2)
