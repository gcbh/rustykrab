"""t5: reports/region_totals.md is the region table, every number matching
data/sales.csv (computed here from the fixture's copy)."""
import csv
import os
import re
import sys
from decimal import Decimal

sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))
import checklib as c  # noqa: E402

a = c.args()
c.need_tree(a.tree)

units, revenue = {}, {}
with open(os.path.join(a.fixture, "data", "sales.csv")) as f:
    for row in csv.DictReader(f):
        r = row["region"]
        units[r] = units.get(r, 0) + int(row["units"])
        revenue[r] = revenue.get(r, Decimal(0)) + int(row["units"]) * Decimal(row["unit_price"])
expected = [[r, str(units[r]), "%.2f" % revenue[r]] for r in sorted(units)]
expected.append(["TOTAL", str(sum(units.values())), "%.2f" % sum(revenue.values())])

text = c.read(a.tree, "reports/region_totals.md")
if text is None:
    c.verdict(False, "reports/region_totals.md missing")
rows = []
for line in text.splitlines():
    line = line.strip()
    if not line.startswith("|"):
        continue
    cells = [cell.strip() for cell in line.strip("|").split("|")]
    if all(re.fullmatch(r":?-{3,}:?", cell) for cell in cells):
        continue
    rows.append(cells)
if not rows or [x.lower() for x in rows[0]] != ["region", "units", "revenue"]:
    c.verdict(False, "header is not | region | units | revenue |: %s" % (rows[:1],))
body = rows[1:]
if body != expected:
    c.verdict(False, "table differs: got %s, want %s" % (body, expected))
c.verdict(True, "table matches (%d regions)" % (len(expected) - 1))
