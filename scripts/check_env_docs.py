#!/usr/bin/env python3
"""Fail when a RUSTYKRAB_ variable read at runtime has no README row.

Every `std::env::var` / `env::var_os` call under `crates/` whose argument
names a `RUSTYKRAB_` variable -- either as a string literal or through a
`const NAME: &str = "RUSTYKRAB_..."` defined anywhere under `crates/` --
must have a row in the Configuration table of README.md.

Names read with the build-time `env!` / `option_env!` macros are set by
build scripts, not by an operator, so they are allowlisted automatically;
BUILD_TIME_ALLOWLIST covers any a build script reads back itself.

Reads whose name is assembled at runtime (`format!("RUSTYKRAB_MCP_{}_URL")`)
cannot be resolved statically and are not checked.

Usage: python3 scripts/check_env_docs.py
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
CRATES = ROOT / "crates"
README = ROOT / "README.md"

# Build-time names that do not appear in an `env!` call but are still not
# operator configuration. Keep this short and say why for each entry.
BUILD_TIME_ALLOWLIST: set[str] = set()

NAME = r"RUSTYKRAB_[A-Z0-9_]+"
# `env::var(` / `env::var_os(` / a bare `var_os(`, then either a literal or a
# (possibly path-qualified, possibly borrowed) identifier as the argument.
READ = re.compile(
    r"\b(?:env::var(?:_os)?|var_os)\s*\(\s*"
    r"(?:\"(?P<lit>[^\"]*)\"|&?\s*(?P<ident>[A-Za-z_][A-Za-z0-9_:]*)\s*\))"
)
CONST = re.compile(
    r"\b(?:const|static)\s+(?P<ident>[A-Z_][A-Z0-9_]*)\s*:\s*&(?:'static\s+)?str\s*=\s*"
    rf"\"(?P<name>{NAME})\""
)
BUILD_MACRO = re.compile(rf"\b(?:option_)?env!\(\s*\"(?P<name>{NAME})\"")
README_ROW = re.compile(r"^\|\s*`(?P<name>[A-Z0-9_]+)`\s*\|")


def rust_sources() -> list[Path]:
    return sorted(p for p in CRATES.rglob("*.rs") if "target" not in p.parts)


def line_of(text: str, offset: int) -> int:
    return text.count("\n", 0, offset) + 1


def documented_names() -> set[str]:
    """Variable names with a row in README's Configuration table."""
    names: set[str] = set()
    in_section = False
    for line in README.read_text().splitlines():
        if line.startswith("## "):
            in_section = line.strip() == "## Configuration"
            continue
        if in_section:
            m = README_ROW.match(line)
            if m:
                names.add(m.group("name"))
    return names


def main() -> int:
    sources = {p: p.read_text(errors="replace") for p in rust_sources()}

    consts: dict[str, set[str]] = {}
    build_time = set(BUILD_TIME_ALLOWLIST)
    for text in sources.values():
        for m in CONST.finditer(text):
            consts.setdefault(m.group("ident"), set()).add(m.group("name"))
        for m in BUILD_MACRO.finditer(text):
            build_time.add(m.group("name"))

    reads: dict[str, list[str]] = {}
    for path, text in sources.items():
        for m in READ.finditer(text):
            if m.group("lit") is not None:
                names = {m.group("lit")}
            else:
                names = consts.get(m.group("ident").rsplit("::", 1)[-1], set())
            where = f"{path.relative_to(ROOT)}:{line_of(text, m.start())}"
            for name in names:
                if re.fullmatch(NAME, name):
                    reads.setdefault(name, []).append(where)

    if not documented_names():
        print("check_env_docs: no Configuration table rows found in README.md")
        return 1

    missing = sorted(set(reads) - documented_names() - build_time)
    if missing:
        print(
            "RUSTYKRAB_ variables read at runtime with no row in README.md's "
            "Configuration table:"
        )
        for name in missing:
            print(f"  {name}  (read at {', '.join(reads[name])})")
        print("Add a `| `NAME` | default | description |` row for each.")
        return 1

    print(f"check_env_docs: {len(reads)} RUSTYKRAB_ variables read, all documented")
    return 0


if __name__ == "__main__":
    sys.exit(main())
