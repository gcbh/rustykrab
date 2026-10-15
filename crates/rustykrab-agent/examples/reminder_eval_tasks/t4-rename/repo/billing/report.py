"""Monthly reporting."""

from billing import core


def monthly_total(orders):
    """Untaxed total over a month of orders, each a list of (qty, price)."""
    return round(sum(core.calc_total(items) for items in orders), 2)
