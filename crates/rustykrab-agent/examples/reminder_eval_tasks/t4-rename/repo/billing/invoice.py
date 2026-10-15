"""Invoice rendering."""

from .core import calc_total


def invoice_lines(items, tax_rate):
    """One line per item, then the taxed total."""
    lines = ["%d x %.2f" % (qty, price) for qty, price in items]
    lines.append("TOTAL %.2f" % calc_total(items, tax_rate))
    return lines
