#!/usr/bin/env python3
"""Restack the open PR stack after something merges.

Run it after a merge, or any time the stack looks stale:

    scripts/restack.py            # show what it would do
    scripts/restack.py --apply    # do it

The work it saves is not the rebase. It is these two things, both of
which are easy to get wrong by hand and silently produce a mess:

**Replaying the right commits.** Once a parent merges, a plain
`git rebase parent child` tries to reapply the commits that just landed,
conflicts against itself, and leaves a rebase half-finished. The child's
own commits are the range `old_parent..child`, so every branch is moved
with `--onto new_parent old_parent`. The old parent is whatever the
remote said *before* this run pushed anything, which is why every remote
sha is captured up front.

**Retargeting the PRs.** A stacked PR whose base has merged still points
at the merged branch on GitHub and shows a diff full of someone else's
commits until its base is changed. `git stack` does not do this part.

Order matters: parents are processed before children, so a child rebases
onto a parent that has already moved.
"""
from __future__ import annotations

import argparse
import json
import subprocess
import sys


def sh(*args: str, check: bool = True) -> str:
    r = subprocess.run(args, capture_output=True, text=True)
    if check and r.returncode != 0:
        raise SystemExit(f"failed: {' '.join(args)}\n{r.stderr.strip()}")
    return r.stdout.strip()


def gh_json(*args: str):
    return json.loads(sh("gh", *args))


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--apply", action="store_true", help="actually rebase and push")
    ap.add_argument("--trunk", default="main")
    ap.add_argument(
        "--stack",
        help="a branch in the stack to restack (default: the current branch). "
             "Only branches connected to it by PR base links are touched.",
    )
    args = ap.parse_args()

    sh("git", "fetch", "--quiet", "origin")

    open_prs = gh_json(
        "pr", "list", "--state", "open", "--limit", "100",
        "--json", "number,headRefName,baseRefName",
    )
    if not open_prs:
        print("no open PRs")
        return 0

    heads = {p["headRefName"]: p for p in open_prs}

    # Only this stack. A repo has other people's branches, dependabot's,
    # and other sessions' -- every one of them based on the trunk, and
    # none of them ours to rebase or force-push. The stack is the set
    # connected to the target branch by base links: walk up through bases
    # that are themselves open PRs, and down through PRs based on those.
    target = args.stack or sh("git", "branch", "--show-current")
    if target not in heads:
        raise SystemExit(
            f"'{target}' has no open PR, so there is no stack to walk. "
            f"Check out a branch in the stack, or pass --stack."
        )
    stack = {target}
    cur = target
    while (base := heads[cur]["baseRefName"]) in heads:   # ancestors
        stack.add(base)
        cur = base
    grew = True
    while grew:                                           # descendants
        grew = False
        for pr in open_prs:
            if pr["headRefName"] not in stack and pr["baseRefName"] in stack:
                stack.add(pr["headRefName"])
                grew = True
    open_prs = [p for p in open_prs if p["headRefName"] in stack]
    print(f"stack: {len(open_prs)} PR(s) connected to {target}\n")

    # A base that is neither the trunk nor an open PR's head has merged.
    # Its PR's own base becomes the new base -- usually the trunk.
    def resolve_base(pr) -> str:
        base = pr["baseRefName"]
        seen = set()
        while base != args.trunk and base not in heads:
            if base in seen:
                return args.trunk
            seen.add(base)
            merged = gh_json(
                "pr", "list", "--state", "merged", "--head", base, "--limit", "1",
                "--json", "baseRefName",
            )
            base = merged[0]["baseRefName"] if merged else args.trunk
        return base

    # Capture every remote head *before* anything is pushed: these are the
    # old bases the children must be replayed from.
    old_remote = {
        b: sh("git", "rev-parse", f"origin/{b}", check=False)
        for b in list(heads) + [args.trunk]
    }

    # Parents before children.
    ordered, placed = [], set()
    while len(ordered) < len(open_prs):
        progressed = False
        for pr in open_prs:
            b = pr["headRefName"]
            if b in placed:
                continue
            base = resolve_base(pr)
            if base == args.trunk or base in placed:
                ordered.append((pr, base))
                placed.add(b)
                progressed = True
        if not progressed:  # a cycle, or a base we cannot resolve
            missing = [p["headRefName"] for p in open_prs if p["headRefName"] not in placed]
            print(f"could not order: {missing}", file=sys.stderr)
            return 1

    for pr, base in ordered:
        branch, num = pr["headRefName"], pr["number"]
        onto = f"origin/{args.trunk}" if base == args.trunk else base
        old = old_remote.get(base) or old_remote[args.trunk]
        retarget = base != pr["baseRefName"]

        if not args.apply:
            note = f"  (retarget #{num} -> {base})" if retarget else ""
            print(f"would rebase {branch} --onto {onto} from {old[:9]}{note}")
            continue

        print(f"rebasing {branch} --onto {onto}")
        r = subprocess.run(
            ["git", "rebase", "--onto", onto, old, branch],
            capture_output=True, text=True,
        )
        if r.returncode != 0:
            subprocess.run(["git", "rebase", "--abort"], capture_output=True)
            print(f"  CONFLICT on {branch} — stopping, nothing pushed for it", file=sys.stderr)
            print(f"  {r.stderr.strip().splitlines()[-1] if r.stderr.strip() else ''}", file=sys.stderr)
            return 1
        sh("git", "push", "--quiet", "--force-with-lease", "origin", branch)
        if retarget:
            sh("gh", "pr", "edit", str(num), "--base", base)
            print(f"  retargeted #{num} -> {base}")

    if args.apply:
        print("\nstack restacked. Verify before merging:")
        print("  cargo fmt --all -- --check && cargo clippy --workspace --all-targets && cargo test --workspace")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
