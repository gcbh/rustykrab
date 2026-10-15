"""t2: the suite passes, the tests are unchanged, and hidden median cases pass."""
import os
import sys

sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))
import checklib as c  # noqa: E402

HIDDEN = '''import unittest
from stats.summary import median


class HiddenMedian(unittest.TestCase):
    def test_pair(self):
        self.assertEqual(median([1, 2]), 1.5)

    def test_single(self):
        self.assertEqual(median([7]), 7)

    def test_input_not_mutated(self):
        data = [3, 1, 2, 4]
        median(data)
        self.assertEqual(data, [3, 1, 2, 4])
'''

a = c.args()
c.need_tree(a.tree)
if not c.unchanged(a.tree, a.fixture, "tests/test_summary.py"):
    c.verdict(False, "tests/test_summary.py was changed")
work = c.scratch_copy(a.tree)
ok, out = c.unittest_passes(work)
if not ok:
    c.verdict(False, "suite fails: " + out[-400:])
work = c.scratch_copy(a.tree, {"tests/test_hidden_median.py": HIDDEN})
ok, out = c.unittest_passes(work)
c.verdict(ok, "suite and hidden median tests pass" if ok else "hidden tests fail: " + out[-400:])
