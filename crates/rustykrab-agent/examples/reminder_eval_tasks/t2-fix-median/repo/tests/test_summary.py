import unittest

from stats.summary import mean, median, spread


class MeanTest(unittest.TestCase):
    def test_mean(self):
        self.assertEqual(mean([1, 2, 3, 4]), 2.5)

    def test_empty(self):
        with self.assertRaises(ValueError):
            mean([])


class MedianTest(unittest.TestCase):
    def test_odd_length(self):
        self.assertEqual(median([5, 1, 3]), 3)

    def test_even_length(self):
        self.assertEqual(median([4, 1, 3, 2]), 2.5)

    def test_even_length_floats(self):
        self.assertEqual(median([10.0, 2.0, 8.0, 4.0]), 6.0)

    def test_empty(self):
        with self.assertRaises(ValueError):
            median([])


class SpreadTest(unittest.TestCase):
    def test_spread(self):
        self.assertEqual(spread([3, 9, 1]), 8)


if __name__ == "__main__":
    unittest.main()
