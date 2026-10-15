"""t7: the option works end to end through config, formatter and CLI, the
old behaviour is unchanged, the suite passes and new tests cover it."""
import os
import sys

sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))
import checklib as c  # noqa: E402

HIDDEN = '''import io
import unittest
from contextlib import redirect_stdout

from greet import config
from greet.cli import main
from greet.formatter import format_greeting


def run(argv):
    out = io.StringIO()
    with redirect_stdout(out):
        main(argv)
    return out.getvalue()


class HiddenUppercase(unittest.TestCase):
    def test_default_off(self):
        self.assertIs(config.DEFAULTS["uppercase"], False)

    def test_formatter(self):
        cfg = config.load({"uppercase": True})
        self.assertEqual(format_greeting("Ada", cfg), "HELLO, ADA!")
        self.assertEqual(format_greeting("Ada", config.load()), "Hello, Ada!")

    def test_cli(self):
        self.assertEqual(run(["--uppercase", "Ada"]), "HELLO, ADA!\\n")
        self.assertEqual(run(["Ada"]), "Hello, Ada!\\n")
        self.assertEqual(run(["--greeting", "Hi", "--uppercase", "Bo"]), "HI, BO!\\n")
'''

a = c.args()
c.need_tree(a.tree)
own = [
    rel for rel in c.grep_tree(os.path.join(a.tree, "tests"), "uppercase")
    if rel.endswith(".py")
]
if not own:
    c.verdict(False, "no test under tests/ mentions uppercase")
work = c.scratch_copy(a.tree)
ok, out = c.unittest_passes(work)
if not ok:
    c.verdict(False, "suite fails: " + out[-400:])
code, out = c.run(work, sys.executable, "-m", "greet.cli", "--uppercase", "Ada")
if code != 0 or out.strip() != "HELLO, ADA!":
    c.verdict(False, "greet.cli --uppercase Ada gave %r" % out[-200:])
work = c.scratch_copy(a.tree, {"tests/test_hidden_uppercase.py": HIDDEN})
ok, out = c.unittest_passes(work)
c.verdict(ok, "uppercase works; own tests: %s" % own if ok else "hidden tests fail: " + out[-400:])
