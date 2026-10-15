import re
import unittest
from pathlib import Path

import tinyqueue
from tinyqueue import BoundedQueue


class QueueTest(unittest.TestCase):
    def test_fifo_and_capacity(self):
        q = BoundedQueue(2)
        self.assertTrue(q.push(1))
        self.assertTrue(q.push(2))
        self.assertFalse(q.push(3))
        self.assertEqual(q.pop(), 1)
        self.assertEqual(q.peek(), 2)

    def test_drain(self):
        q = BoundedQueue(3)
        q.push("a")
        q.push("b")
        self.assertEqual(q.drain(), ["a", "b"])
        self.assertEqual(len(q), 0)


class VersionTest(unittest.TestCase):
    def test_version_matches_pyproject(self):
        text = (Path(__file__).parent.parent / "pyproject.toml").read_text()
        declared = re.search(r'^version = "([^"]+)"', text, re.M).group(1)
        self.assertEqual(tinyqueue.__version__, declared)


if __name__ == "__main__":
    unittest.main()
