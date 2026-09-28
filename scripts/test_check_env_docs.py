#!/usr/bin/env python3
"""Self-test for check_env_docs.py: it must catch undocumented helper reads.

Each case writes a synthetic `crates/` tree and README.md into a temp dir,
points the checker at it, and asserts on the exit code. If helper-based env
reads (`fn helper(key: &str)` / `let helper = |key: &str|` wrapping
`env::var`) stop being detected, the undocumented cases below pass silently
and this test fails.

Usage: python3 scripts/test_check_env_docs.py
"""

from __future__ import annotations

import contextlib
import importlib.util
import io
import sys
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).resolve().parent / "check_env_docs.py"

README_DOCUMENTED = """\
# Fixture

## Configuration

| Variable | Default | Description |
|---|---|---|
| `RUSTYKRAB_DOCUMENTED` | - | a documented variable |
"""

FN_HELPER = """\
fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

fn configure() {
    let _ = env_or("RUSTYKRAB_DOCUMENTED", "x");
    let _ = env_or("RUSTYKRAB_UNDOCUMENTED_HELPER", "y");
}
"""

TURBOFISH_HELPER = """\
fn env_parse<T: std::str::FromStr>(key: &str) -> Option<T> {
    std::env::var(key).ok()?.parse().ok()
}

fn configure() {
    let _ = env_parse::<u64>("RUSTYKRAB_UNDOCUMENTED_HELPER");
}
"""

CLOSURE_HELPER = """\
fn configure() {
    let flag = |key: &str| std::env::var_os(key).is_some();
    let _ = flag("RUSTYKRAB_UNDOCUMENTED_HELPER");
}
"""

ONLY_DOCUMENTED = """\
fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

fn configure() {
    let _ = env_or("RUSTYKRAB_DOCUMENTED", "x");
}
"""


def load_checker():
    spec = importlib.util.spec_from_file_location("check_env_docs", SCRIPT)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class HelperDetection(unittest.TestCase):
    def run_checker(self, rust: str) -> tuple[int, str]:
        checker = load_checker()
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            src = root / "crates" / "fixture" / "src"
            src.mkdir(parents=True)
            (src / "lib.rs").write_text(rust)
            (root / "README.md").write_text(README_DOCUMENTED)
            checker.ROOT = root
            checker.CRATES = root / "crates"
            checker.README = root / "README.md"
            out = io.StringIO()
            with contextlib.redirect_stdout(out):
                code = checker.main()
        return code, out.getvalue()

    def assert_flags_undocumented(self, rust: str) -> None:
        code, out = self.run_checker(rust)
        self.assertEqual(code, 1, out)
        self.assertIn("RUSTYKRAB_UNDOCUMENTED_HELPER", out)
        self.assertNotIn("RUSTYKRAB_DOCUMENTED ", out)

    def test_fn_helper(self) -> None:
        self.assert_flags_undocumented(FN_HELPER)

    def test_turbofish_helper(self) -> None:
        self.assert_flags_undocumented(TURBOFISH_HELPER)

    def test_closure_helper(self) -> None:
        self.assert_flags_undocumented(CLOSURE_HELPER)

    def test_documented_helper_read_passes(self) -> None:
        code, out = self.run_checker(ONLY_DOCUMENTED)
        self.assertEqual(code, 0, out)


if __name__ == "__main__":
    sys.exit(unittest.main())
