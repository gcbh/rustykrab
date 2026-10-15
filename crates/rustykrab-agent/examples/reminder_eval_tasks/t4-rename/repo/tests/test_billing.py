import unittest

from billing.core import calc_total
from billing.invoice import invoice_lines
from billing.report import monthly_total


class TotalTest(unittest.TestCase):
    def test_untaxed(self):
        self.assertEqual(calc_total([(2, 3.5), (1, 4.25)]), 11.25)

    def test_taxed(self):
        self.assertEqual(calc_total([(4, 2.5)], tax_rate=0.2), 12.0)


class InvoiceTest(unittest.TestCase):
    def test_lines(self):
        self.assertEqual(
            invoice_lines([(2, 3.5)], 0.1),
            ["2 x 3.50", "TOTAL 7.70"],
        )


class ReportTest(unittest.TestCase):
    def test_monthly(self):
        self.assertEqual(monthly_total([[(1, 1.25)], [(2, 2.0)]]), 5.25)


if __name__ == "__main__":
    unittest.main()
