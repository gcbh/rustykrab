"""Base rates per shipping method, in dollars."""

BASE_RATES = {
    "standard": 4.50,
    "express": 9.75,
    "overnight": 21.00,
}

# Each kilogram above the free allowance costs this much, per method.
PER_KG = {
    "standard": 0.80,
    "express": 1.20,
    "overnight": 2.10,
}

FREE_ALLOWANCE_KG = 5
