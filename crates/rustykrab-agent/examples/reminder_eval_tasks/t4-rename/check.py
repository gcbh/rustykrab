"""t4: no calc_total anywhere, the suite passes, and compute_total is
importable from both places calc_total was."""
import os
import sys

sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))
import checklib as c  # noqa: E402

HIDDEN = '''import unittest


class HiddenRename(unittest.TestCase):
    def test_names(self):
        from billing.core import compute_total
        from billing import compute_total as exported
        self.assertIs(compute_total, exported)
        self.assertEqual(compute_total([(3, 1.5)], tax_rate=0.1), 4.95)
'''

a = c.args()
c.need_tree(a.tree)
left = c.grep_tree(a.tree, "calc_total")
if left:
    c.verdict(False, "calc_total still in: %s" % left)
work = c.scratch_copy(a.tree)
ok, out = c.unittest_passes(work)
if not ok:
    c.verdict(False, "suite fails: " + out[-400:])
work = c.scratch_copy(a.tree, {"tests/test_hidden_rename.py": HIDDEN})
ok, out = c.unittest_passes(work)
c.verdict(ok, "renamed everywhere; suite passes" if ok else "hidden tests fail: " + out[-400:])
