from .rates import BASE_RATES, FREE_ALLOWANCE_KG, PER_KG

MEMBER_DISCOUNT = 0.15
REMOTE_SURCHARGE = 3.40


def shipping_cost(weight_kg, method, member=False, remote=False):
    """Return the shipping cost of one parcel in dollars, rounded to cents."""
    cost = BASE_RATES[method]
    extra = max(0.0, weight_kg - FREE_ALLOWANCE_KG)
    cost += extra * PER_KG[method]
    if member:
        cost *= 1 - MEMBER_DISCOUNT
    if remote:
        cost += REMOTE_SURCHARGE
    return round(cost, 2)
