"""t1: the summary must state the value the call returns, 19.34."""
import os
import re
import sys

sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))
import checklib as c  # noqa: E402

a = c.args()
report = c.load_report(a.report)
if report is None:
    c.verdict(False, "no report")
text = c.summary(report)
# 19.34 as a number on its own (not 119.34 or 19.345).
ok = re.search(r"(?<![\d.])19\.34(?![\d])", text) is not None
c.verdict(ok, "summary states 19.34" if ok else "19.34 not in summary")
