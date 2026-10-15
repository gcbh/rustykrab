"""t3: hidden tests over every rule of the spec, a new test module that
exercises parse_duration, and the suite passes."""
import os
import sys

sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))
import checklib as c  # noqa: E402

HIDDEN = '''import unittest
from timeparse.durations import parse_duration


class HiddenParseDuration(unittest.TestCase):
    def test_valid(self):
        cases = {
            "1h30m": 5400, "45m": 2700, "2h5s": 7205, "90s": 90,
            "  1h  ": 3600, "1H": 3600, "1H30M": 5400, "0s": 0,
            "1h2m3s": 3723, "10m10s": 610,
        }
        for text, seconds in cases.items():
            with self.subTest(text=text):
                value = parse_duration(text)
                self.assertEqual(value, seconds)
                self.assertIsInstance(value, int)

    def test_invalid(self):
        for text in ["", "   ", "h", "1hm", "5x", "1d", "1h1h", "1m1h",
                     "15", "1h30", "-5m", "1.5h", "1h 30m"]:
            with self.subTest(text=text):
                with self.assertRaises(ValueError):
                    parse_duration(text)
'''

a = c.args()
c.need_tree(a.tree)
own = [
    rel for rel in c.grep_tree(os.path.join(a.tree, "tests"), "parse_duration")
    if rel.endswith(".py")
]
if not own:
    c.verdict(False, "no test module under tests/ exercises parse_duration")
work = c.scratch_copy(a.tree)
ok, out = c.unittest_passes(work)
if not ok:
    c.verdict(False, "suite fails: " + out[-400:])
work = c.scratch_copy(a.tree, {"tests/test_hidden_durations.py": HIDDEN})
ok, out = c.unittest_passes(work)
c.verdict(ok, "spec tests pass; own tests: %s" % own if ok else "spec tests fail: " + out[-500:])
