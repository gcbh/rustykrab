#!/usr/bin/env bash
# End-to-end evaluation harness (docs/plans/apollo-ios-and-credential-guard.md §11).
#
# Builds the daemon without the embeddings feature (buildable in
# network-restricted sandboxes — no ONNX runtime download), then boots it
# on a throwaway data dir and runs the exit-criteria scenario suite with a
# scripted agent. Prints a JSON report; exit code 0 means green
# (implemented scenarios pass, Phase 2 target scenarios are xfail).
#
# Usage: scripts/e2e.sh [--release]
set -euo pipefail
cd "$(dirname "$0")/.."

PROFILE=debug
CARGO_FLAGS=()
if [[ "${1:-}" == "--release" ]]; then
  PROFILE=release
  CARGO_FLAGS+=(--release)
fi

echo "building daemon (--no-default-features)..." >&2
cargo build -p rustykrab-cli --no-default-features "${CARGO_FLAGS[@]+"${CARGO_FLAGS[@]}"}"
echo "building e2e runner..." >&2
cargo build -p rustykrab-e2e "${CARGO_FLAGS[@]+"${CARGO_FLAGS[@]}"}"

# Report goes to stdout and to e2e-report.json (CI uploads it as an
# artifact); the exit code is the runner's.
set +e
RUSTYKRAB_BIN="target/$PROFILE/rustykrab-cli" "target/$PROFILE/rustykrab-e2e" \
  | tee e2e-report.json
status=${PIPESTATUS[0]}
exit "$status"
