#!/usr/bin/env bash
# Build the daemon (no default features, as scripts/e2e.sh does) at each revision.
# The target dir is shared across worktrees, and cargo keys workspace crates by
# relative path, so a crate built in one worktree is reused, stale, in the next.
# Clean every workspace member first so each build compiles its own tree.
set -euo pipefail
V="$(cd "$(dirname "$0")" && pwd)"
export CARGO_TARGET_DIR=~/projects/rustycrab-worktrees/target-shared
for pair in base:verify-base pr661:spawn-ctx pr662:runner-endturn; do
  name=${pair%%:*}; wt=${pair#*:}
  cd ~/projects/rustycrab-worktrees/$wt
  echo "== $name $(git rev-parse HEAD) tree $(git rev-parse HEAD^{tree}) dirty=$(git status --porcelain | wc -l | tr -d ' ')"
  members=$(cargo metadata --no-deps --format-version 1 | python3 -c 'import json,sys; print(" ".join("-p "+p["name"] for p in json.load(sys.stdin)["packages"]))')
  cargo clean $members 2>&1 | tail -1
  cargo build -p rustykrab-cli --no-default-features 2>&1 | tail -1
  cp "$CARGO_TARGET_DIR/debug/rustykrab-cli" "$V/bin/rustykrab-cli-$name"
  "$V/bin/rustykrab-cli-$name" --version
  echo "  sha256 $(shasum -a 256 "$V/bin/rustykrab-cli-$name" | cut -c1-16)"
  echo "  contains #661 code (with_task_locals): $(nm "$V/bin/rustykrab-cli-$name" | grep -c with_task_locals)"
  echo "  contains #662 code (reminder log line): $(strings -n 8 "$V/bin/rustykrab-cli-$name" | grep -c 'delivering the previous answer')"
done
echo BUILD-ALL-DONE
