import unittest

from timeparse.clock import format_hms


class FormatHmsTest(unittest.TestCase):
    def test_format(self):
        self.assertEqual(format_hms(3725), "1:02:05")

    def test_negative(self):
        with self.assertRaises(ValueError):
            format_hms(-1)


if __name__ == "__main__":
    unittest.main()
