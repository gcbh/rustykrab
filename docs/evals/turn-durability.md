# Turn durability split

First replacement for the combined PR #642, based on `0b565fd`.
This slice changes lifecycle/persistence, not compaction policy or credential
security. The following context/evaluation PR builds on these APIs.

## Contract and boundaries

Accepted channel input retains its UUID through admission, execution and a
successful final history save. Provider errors and inactivity cancellation
return owned partial history rather than throwing it away. A completion-edge
follow-up gets another dispatch within the existing total iteration allowance.
Reset fences obsolete work and waits for its persistence before later turns.

Tests cover injected follow-ups, provider failure, cancellation of a stuck
provider, abortion of parallel local tool futures, shared iteration limits,
channel heartbeat renewal, reset generations and real SQLite journal identity/
idempotence/cascade. Existing scripted HTTP/SSE scenarios remain applicable.
The separate context suite added by the next PR joins daemon wire requests
and journal rows; that harness is not included in this slice.

Cancellation cannot undo an already-applied external action. Unknown outcomes
are recorded, not automatically retried. The admission journal is not upstream
exactly-once delivery; unresolved acknowledgement gaps, async memory durability,
pending-record expiry/recovery UI and concurrent same-conversation HTTP turns
remain explicit limitations.

## Evidence and reproduction

[Original combined-run evidence](evidence.md) remains available with its original
build identity. It must not be mistaken for a standalone validation of this split.

Run the repository's architecture checker, formatting, workspace clippy/tests
and `scripts/e2e.sh`. In offline/non-ONNX environments, use
`--offline --no-default-features` for Cargo as documented in CLAUDE.md.

Standalone split validation, September 12, 2026: 1,026 workspace tests pass,
30 ignored; workspace/all-target clippy, formatting, architecture generation/
verification and diff whitespace checks pass. The real-daemon scripted suite
passes 23 scenarios, with six existing expected failures and no unexpected
passes. These checks use the offline/non-ONNX path, not default embeddings or
real merchant actions. The new context harness is validated in the next slice.
