"""t8: both versions read 1.4.0, the changelog sections are in place with
the former Unreleased entries under 1.4.0, and the suite passes."""
import os
import re
import sys

sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))
import checklib as c  # noqa: E402

a = c.args()
c.need_tree(a.tree)
problems = []
pyproject = c.read(a.tree, "pyproject.toml") or ""
if not re.search(r'^version = "1\.4\.0"$', pyproject, re.M):
    problems.append("pyproject.toml version is not 1.4.0")
init = c.read(a.tree, "tinyqueue/__init__.py") or ""
if not re.search(r'^__version__ = "1\.4\.0"$', init, re.M):
    problems.append("__version__ is not 1.4.0")
log = c.read(a.tree, "CHANGELOG.md") or ""
heads = [(m.start(), m.group(1).strip()) for m in re.finditer(r"^## (.+)$", log, re.M)]
titles = [t for _, t in heads]
want = ["Unreleased", "1.4.0 - 2026-10-14", "1.3.2 - 2026-08-30", "1.3.1 - 2026-07-12"]
if titles != want:
    problems.append("changelog sections are %s, want %s" % (titles, want))
else:
    unreleased = log[heads[0][0]:heads[1][0]]
    released = log[heads[1][0]:heads[2][0]]
    if re.search(r"^\s*[-*] ", unreleased, re.M):
        problems.append("Unreleased section is not empty")
    for entry in ("BoundedQueue.drain()", "returns `False` instead of raising"):
        if entry not in released:
            problems.append("1.4.0 section lacks %r" % entry)
if problems:
    c.verdict(False, "; ".join(problems))
work = c.scratch_copy(a.tree)
ok, out = c.unittest_passes(work)
c.verdict(ok, "release prepared; suite passes" if ok else "suite fails: " + out[-400:])
