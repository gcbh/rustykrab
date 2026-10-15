"""Shared helpers for the reminder eval's task checkers.

Each task's check.py is run as

    python3 check.py --tree DIR --report REPORT.json --fixture DIR

where --tree is the repository as the run left it (empty when there is
none), --report is the ResultReport the run filed (absent when it filed
none) and --fixture is the task's original repository. It prints one JSON
object, {"pass": bool, "detail": str}, and exits 0 whatever the verdict.
Python 3.9, standard library only.
"""

import argparse
import atexit
import hashlib
import json
import os
import shutil
import subprocess
import sys
import tempfile


def args():
    p = argparse.ArgumentParser()
    p.add_argument("--tree", default="")
    p.add_argument("--report", default="")
    p.add_argument("--fixture", required=True)
    return p.parse_args()


def load_report(path):
    if not path or not os.path.isfile(path):
        return None
    with open(path) as f:
        return json.load(f)


def summary(report):
    return (report or {}).get("summary") or ""


def verdict(ok, detail):
    print(json.dumps({"pass": bool(ok), "detail": detail}))
    sys.exit(0)


def need_tree(tree):
    if not tree or not os.path.isdir(tree):
        verdict(False, "no repository tree to check")


def read(tree, rel):
    path = os.path.join(tree, rel)
    if not os.path.isfile(path):
        return None
    with open(path, encoding="utf-8", errors="replace") as f:
        return f.read()


def digest(path):
    with open(path, "rb") as f:
        return hashlib.sha256(f.read()).hexdigest()


def unchanged(tree, fixture, rel):
    a, b = os.path.join(tree, rel), os.path.join(fixture, rel)
    return os.path.isfile(a) and digest(a) == digest(b)


def scratch_copy(tree, extra=None):
    """A throwaway copy of the tree, with `extra` {relpath: text} added, so
    hidden tests never touch the run's own files."""
    dst = tempfile.mkdtemp(prefix="reminder-check-")
    atexit.register(shutil.rmtree, dst, True)
    os.rmdir(dst)
    shutil.copytree(tree, dst, ignore=shutil.ignore_patterns(".git", "__pycache__"))
    for rel, text in (extra or {}).items():
        path = os.path.join(dst, rel)
        os.makedirs(os.path.dirname(path), exist_ok=True)
        with open(path, "w") as f:
            f.write(text)
    return dst


def run(cwd, *cmd, timeout=60):
    try:
        p = subprocess.run(
            list(cmd),
            cwd=cwd,
            capture_output=True,
            text=True,
            timeout=timeout,
            env=dict(os.environ, PYTHONDONTWRITEBYTECODE="1"),
        )
        return p.returncode, (p.stdout + p.stderr)[-1500:]
    except subprocess.TimeoutExpired:
        return 124, "timed out"


def unittest_passes(cwd, *extra):
    """`python3 -m unittest discover` in cwd (and any extra argv)."""
    code, out = run(cwd, sys.executable, "-m", "unittest", "discover", *extra)
    return code == 0, out


def grep_tree(tree, needle):
    """Relative paths of text files under tree that contain needle."""
    hits = []
    for root, dirs, files in os.walk(tree):
        dirs[:] = [d for d in dirs if d not in (".git", "__pycache__")]
        for name in files:
            path = os.path.join(root, name)
            try:
                with open(path, encoding="utf-8") as f:
                    if needle in f.read():
                        hits.append(os.path.relpath(path, tree))
            except (UnicodeDecodeError, OSError):
                continue
    return sorted(hits)
