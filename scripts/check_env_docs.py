#!/usr/bin/env python3
"""Fail when a RUSTYKRAB_ variable read at runtime has no README row.

Every `std::env::var` / `env::var_os` call under `crates/` whose argument
names a `RUSTYKRAB_` variable -- either as a string literal or through a
`const NAME: &str = "RUSTYKRAB_..."` defined anywhere under `crates/` --
must have a row in the Configuration table of README.md.

Wrappers count too: any `fn helper(key: &str)` or `let helper = |key: &str|`
whose body passes that parameter to `env::var` / `var_os` is a known
env-reading helper, and a `helper("RUSTYKRAB_...")` call (turbofish allowed,
e.g. `env_parse::<u64>(...)`) is checked like a direct read.

Names read with the build-time `env!` / `option_env!` macros are set by
build scripts, not by an operator, so they are allowlisted automatically;
BUILD_TIME_ALLOWLIST covers any a build script reads back itself.

Reads whose name is assembled at runtime (`format!("RUSTYKRAB_MCP_{upper}_URL")`)
need a pattern row instead: a README row whose name has `<PLACEHOLDER>`
segments, e.g. `RUSTYKRAB_MCP_<NAME>_URL`. A `format!` template counts as a
read when it is passed straight to `env::var` / `var_os`, or bound with
`let key = format!(...)` and that `key` is read in the same file. Each
`{...}` in the template must fall where the row has a placeholder. Names
matched by prefix over `env::vars()` are not traced.

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
# A function or closure taking a `&str` parameter; its body is then searched
# for an env read of that parameter.
HELPER_DEF = re.compile(
    r"(?:\bfn\s+(?P<fn>[a-z_][a-z0-9_]*)\s*(?:<[^>(]*>)?\s*\(\s*"
    r"|\blet\s+(?P<cl>[a-z_][a-z0-9_]*)\s*=\s*(?:move\s*)?\|\s*)"
    r"(?P<param>[a-z_][a-z0-9_]*)\s*:\s*&(?:'static\s+)?str\b"
)
# How far past a helper's signature to look for the env read of its parameter.
HELPER_BODY_CHARS = 400
BUILD_MACRO = re.compile(rf"\b(?:option_)?env!\(\s*\"(?P<name>{NAME})\"")
README_ROW = re.compile(r"^\|\s*`(?P<name>[A-Z0-9_]+)`\s*\|")
README_PATTERN_ROW = re.compile(
    r"^\|\s*`(?P<name>RUSTYKRAB_[A-Z0-9_]*<[A-Z0-9_]+>[A-Z0-9_<>]*)`\s*\|"
)
# A `format!` template naming a RUSTYKRAB_ variable, with `{...}` holes.
TEMPLATE = r"format!\(\s*\"(?P<tpl>RUSTYKRAB_[A-Za-z0-9_{}]*\{[A-Za-z0-9_{}]*)\""
TEMPLATE_READ = re.compile(
    r"\b(?:env::var(?:_os)?|var_os)\s*\(\s*&?\s*" + TEMPLATE
)
TEMPLATE_LET = re.compile(r"\blet\s+(?P<ident>[a-z_][a-z0-9_]*)\s*=\s*" + TEMPLATE)
HOLE = re.compile(r"\{[^{}]*\}")


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


def documented_patterns() -> dict[str, re.Pattern[str]]:
    """Pattern rows (`RUSTYKRAB_MCP_<NAME>_URL`) in the Configuration table.

    Each maps to a regex over a template whose holes are written as `{}`:
    a `<PLACEHOLDER>` matches one or more holes or name characters, and
    literal segments must match exactly.
    """
    patterns: dict[str, re.Pattern[str]] = {}
    in_section = False
    for line in README.read_text().splitlines():
        if line.startswith("## "):
            in_section = line.strip() == "## Configuration"
            continue
        if in_section:
            m = README_PATTERN_ROW.match(line)
            if m:
                parts = re.split(r"<[A-Z0-9_]+>", m.group("name"))
                rx = r"(?:\{\}|[A-Z0-9_])+".join(map(re.escape, parts))
                patterns[m.group("name")] = re.compile(rx)
    return patterns


def template_reads(path: Path, text: str) -> dict[str, list[str]]:
    """`format!`-built RUSTYKRAB_ names read in `text`, holes normalised to `{}`."""
    found: list[tuple[str, int]] = []
    for m in TEMPLATE_READ.finditer(text):
        found.append((m.group("tpl"), m.start()))
    for m in TEMPLATE_LET.finditer(text):
        ident = re.escape(m.group("ident"))
        read = rf"\b(?:env::var(?:_os)?|var_os)\s*\(\s*&?\s*{ident}\s*\)"
        if re.search(read, text[m.end() :]):
            found.append((m.group("tpl"), m.start()))
    reads: dict[str, list[str]] = {}
    for tpl, offset in found:
        where = f"{path.relative_to(ROOT)}:{line_of(text, offset)}"
        reads.setdefault(HOLE.sub("{}", tpl), []).append(where)
    return reads


def env_helpers(sources: dict[Path, str]) -> set[str]:
    """Functions and closures that read the variable named by their `&str` arg."""
    helpers: set[str] = set()
    for text in sources.values():
        for m in HELPER_DEF.finditer(text):
            body = text[m.end() : m.end() + HELPER_BODY_CHARS]
            param = re.escape(m.group("param"))
            read = rf"\b(?:env::var(?:_os)?|var_os)\s*\(\s*&?\s*{param}\s*\)"
            if re.search(read, body):
                helpers.add(m.group("fn") or m.group("cl"))
    return helpers


def helper_call(helpers: set[str]) -> re.Pattern[str] | None:
    """`helper("RUSTYKRAB_...")` or `helper::<T>("RUSTYKRAB_...")`."""
    if not helpers:
        return None
    alt = "|".join(sorted(map(re.escape, helpers)))
    return re.compile(
        rf"(?<![A-Za-z0-9_:.])(?:{alt})\s*(?:::\s*<[^>]*>\s*)?"
        rf"\(\s*\"(?P<lit>{NAME})\""
    )


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
    templates: dict[str, list[str]] = {}
    call = helper_call(env_helpers(sources))
    for path, text in sources.items():
        for tpl, wheres in template_reads(path, text).items():
            templates.setdefault(tpl, []).extend(wheres)
        if call is not None:
            for m in call.finditer(text):
                where = f"{path.relative_to(ROOT)}:{line_of(text, m.start())}"
                reads.setdefault(m.group("lit"), []).append(where)
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

    failed = False
    missing = sorted(set(reads) - documented_names() - build_time)
    if missing:
        print(
            "RUSTYKRAB_ variables read at runtime with no row in README.md's "
            "Configuration table:"
        )
        for name in missing:
            print(f"  {name}  (read at {', '.join(reads[name])})")
        print("Add a `| `NAME` | default | description |` row for each.")
        failed = True

    patterns = documented_patterns().values()
    unmatched = sorted(
        tpl for tpl in templates if not any(p.fullmatch(tpl) for p in patterns)
    )
    if unmatched:
        print(
            "RUSTYKRAB_ names built at runtime with no pattern row in "
            "README.md's Configuration table:"
        )
        for tpl in unmatched:
            print(f"  {tpl}  (read at {', '.join(templates[tpl])})")
        print(
            "Add a `| `RUSTYKRAB_..._<PLACEHOLDER>_...` | default | description |` "
            "row with a placeholder for each `{}`."
        )
        failed = True

    if failed:
        return 1

    print(
        f"check_env_docs: {len(reads)} RUSTYKRAB_ variables and "
        f"{len(templates)} runtime-built name patterns read, all documented"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
