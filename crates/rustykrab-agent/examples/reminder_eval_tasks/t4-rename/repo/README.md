# billing

Order and invoice arithmetic.

- `billing.core.calc_total(items, tax_rate=0.0)` sums `quantity * unit_price`
  over `(quantity, unit_price)` pairs and applies the tax rate, rounded to
  cents. It is also importable as `billing.calc_total`.
- `billing.invoice.invoice_lines(items, tax_rate)` renders an invoice.
- `billing.report.monthly_total(orders)` totals a month of orders.

Run the tests with `python3 -m unittest discover`.
