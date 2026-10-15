"""Descriptive statistics over plain lists of numbers."""


def mean(values):
    """Return the arithmetic mean of a non-empty list of numbers."""
    if not values:
        raise ValueError("mean of empty list")
    return sum(values) / len(values)


def median(values):
    """Return the median of a non-empty list of numbers."""
    if not values:
        raise ValueError("median of empty list")
    ordered = sorted(values)
    middle = len(ordered) // 2
    return ordered[middle]


def spread(values):
    """Return max - min of a non-empty list of numbers."""
    if not values:
        raise ValueError("spread of empty list")
    return max(values) - min(values)
