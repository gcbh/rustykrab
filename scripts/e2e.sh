#!/usr/bin/env bash
# End-to-end evaluation harness (docs/plans/apollo-ios-and-credential-guard.md §11).
#
# Builds the daemon without the embeddings feature (buildable in
# network-restricted sandboxes — no ONNX runtime download), then boots it
# on a throwaway data dir and runs the exit-criteria scenario suite with a
# scripted agent. Prints a JSON report; exit code 0 means green
# (implemented scenarios pass, Phase 2 target scenarios are xfail).
#
# Usage:
#   scripts/e2e.sh                      # scripted plumbing suite (fast, CI)
#   scripts/e2e.sh --mode model         # gemma4 behaviour suite (slow)
#   scripts/e2e.sh --mode browser       # opt-in live browser journeys
#   scripts/e2e.sh --mode all --release
#
# Any flag other than --release is passed through to the runner.
set -euo pipefail
cd "$(dirname "$0")/.."

PROFILE=debug
CARGO_FLAGS=()
RUNNER_ARGS=()
for arg in "$@"; do
  if [[ "$arg" == "--release" ]]; then
    PROFILE=release
    CARGO_FLAGS+=(--release)
  else
    RUNNER_ARGS+=("$arg")
  fi
done

# Bind every machine-readable evidence artifact to the exact checkout that
# built the daemon and runner. Callers may override this for packaged sources
# that do not include Git metadata.
if [[ -n "$(git status --porcelain --untracked-files=normal)" ]]; then
  DETECTED_SOURCE_REVISION="$(git rev-parse HEAD)-dirty"
else
  DETECTED_SOURCE_REVISION="$(git rev-parse HEAD)"
fi
E2E_SOURCE_REVISION="${RUSTYKRAB_E2E_SOURCE_REVISION:-$DETECTED_SOURCE_REVISION}"

echo "building daemon (--no-default-features)..." >&2
cargo build -p rustykrab-cli --no-default-features "${CARGO_FLAGS[@]+"${CARGO_FLAGS[@]}"}"
echo "building e2e runner..." >&2
cargo build -p rustykrab-e2e "${CARGO_FLAGS[@]+"${CARGO_FLAGS[@]}"}"

# Report goes to stdout and to e2e-report.json (CI uploads it as an
# artifact); the exit code is the runner's.
set +e
RUSTYKRAB_E2E_SOURCE_REVISION="$E2E_SOURCE_REVISION" \
  RUSTYKRAB_BIN="target/$PROFILE/rustykrab-cli" "target/$PROFILE/rustykrab-e2e" \
  "${RUNNER_ARGS[@]+"${RUNNER_ARGS[@]}"}" | tee e2e-report.json
status=${PIPESTATUS[0]}
exit "$status"
