# Usage without task budgets — 2026-10-31

Exact base: `7b0814fbb5c0b1d522537e055b8727f7c45f4682` (v6.6.14).
Owner decision: remove budgets and discover task effort from observations.
This document records candidate implementation and deterministic verification,
not release deployment, real-model quality or live original-work completion.

## Failure and resulting behavior

The original repair attempt recorded 1,603,490 tokens, 25 iterations and
1,802,805 ms. Its estimate was 200k tokens. Subsequent owner answers raised the
allowance to 400k and 600k, both below already observed usage. The guard immediately
blocked again without a new attempt. Repeated approval did not test the repair.

Durable tasks now have no token, iteration or task-runtime execution allowance.
The controller records usage across attempts and restarts without calculating
allotments or parent reservations. New items and briefs normalize legacy
execution estimates to zero while retaining finite failure-recovery policy.
Plans no longer require estimates, reject child sums or ask for token-threshold
approval. Exact reviewed templates ignore retired estimates while retaining
scope, tools, constraints, project references and recovery policy.

Local workers count every model response, including streaming and compaction,
without token admission or a task turn/runtime cap. Context-window limits stay.
Native and peer adapters keep operational process/transport timeouts; those now
report `tool/timeout`, and native vendor turn caps report process failures.
Physical context/output limits, cancellation, real provider/account capacity,
resource serialization, no-progress detection and finite recovery rungs remain.
Explicit schedule frequency and the review service's pending-job liveness
control are unchanged. This is not authority to expand a task's scope.

## Legacy state and owner direction

The serialized tick applies pending owner answer receipts before retiring old
`BudgetExhausted` leaf gates. Canonical open controller budget questions become
Obsolete without a fabricated answer or new grant. A pending cancellation keeps
its original semantics; rejected receipt history is retained without preserving
the removed gate. Historical usage and grant/evidence rows are never deleted.

An independent open question, consent/approval hold, dependency or cancellation
still prevents execution. An old plan approval is retired only when its exact
sole host trigger was a token threshold and a durable plan links that question
to that root. Mixed and unlinked approvals remain. No schema migration, new queue
or model-authored authority is introduced. Dashboard alerts no longer compare
elapsed runtime with a retired estimate; real lease/heartbeat faults remain.

## Regressions and execution evidence

| Check | Result |
|---|---|
| Released-source negative control | The new `usage_only_retires_legacy_budget_hold_and_completes_original_work` test fails on base production source: `Blocked(BudgetExhausted)` instead of `Done`. Candidate production files were restored byte-for-byte afterward. |
| Candidate original-case regression | One new lease completes; the old question is Obsolete; no grant is minted; observed tokens increase from 1,603,490 to 1,604,490. |
| Migration and authority regressions | Independent technical/consent questions, mixed/unlinked approvals, cancellations and receipt audit remain protected. A rejected historical receipt cannot hold a removed budget gate. |
| Worker regressions | Local work runs past old iteration, token and wall estimates; all six provider APIs count usage; unchanged tool calls still hand off with evidence. Native/peer timeouts retain typed technical recovery. |
| Workspace / strict lint | 2,495 tests pass; `cargo clippy --workspace --all-targets -- -D warnings` passes. Architecture metrics, env documentation and formatting are checked. |
| Scripted daemon E2E | 70 pass, 7 existing xfail, 0 fail, 0 xpass. The former budget-grant case is replaced by a required-pass usage-only/real-technical-decision case. No scenario was skipped or demoted. |
| Configured context suite | All 28 trials pass with `structured-message-tail`, the CI setting. This is wire-contract evidence, not model behavior. |

The checked-in [whole-daemon fixture](../../scripts/verify_usage_only_recovery.py)
uses the actual CLI daemon, SQLite, LocalWorker, sandboxed exec and real Git,
with an inert loopback Ollama response server. Its database is seeded only while
its own daemon is stopped; it cannot reuse an existing fixture directory. No
live data, credential, model account or live gateway is used.

Both modes start with 1,603,490 recorded tokens against an old 600k estimate,
a canonical budget question, an independent consent hold and complete prior
owner receipts. The candidate binary SHA-256 is
`d289a9bfee109f3b4eef6793aacd811e250975c0aa426603bc71ab5370bc50d3`.

- `self_correct`: 13 inert model responses, one shared Git repair, two
  Git-verified commits preserving both parent histories, then the original
  operation commits `original-result.txt`. The original's usage reaches
  1,603,820 tokens. The source checkout/main stays untouched.
- `stalled`: 45 inert responses over three attempts reach a finite technical
  hold via `model/loop`. Partial repair files and transcripts remain. The
  original operation never runs and no commit is claimed.

Both modes retain prior usage, retire only the budget question, preserve the
independent consent hold, create no budget grant and stop all fixture processes.
An initial harness assertion expected 120 historical questions but observed 121:
the separately seeded consent question also belonged to the project. Correcting
that count preserved every receipt; the complete shipped fixture passed.

Reproduce locally:

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
scripts/e2e.sh
RUSTYKRAB_CONTEXT_COMPACTION_STRATEGY=structured-message-tail \
  scripts/e2e.sh --mode context --trial-timeout 30
cargo build -p rustykrab-cli --no-default-features
python3 scripts/verify_usage_only_recovery.py \
  --binary target/debug/rustykrab-cli --fixture /tmp/usage-only-recovery-new \
  --expect self_correct
python3 scripts/verify_usage_only_recovery.py \
  --binary target/debug/rustykrab-cli --fixture /tmp/usage-only-stall-new \
  --expect stalled
python3 scripts/check_architecture_docs.py
python3 scripts/check_env_docs.py
cargo fmt --all -- --check
```

On macOS, tests that invoke the real process sandbox must run outside a second
restrictive outer sandbox; nested `sandbox-exec` produced permission failures in
the first broad attempt. The same suite with the proper permissions passed;
assertions were not weakened. One early exact-name test filter selected zero
tests; only the later observed one-test negative/positive runs count as evidence.
The updated owner direction initially exceeded the existing 8 KB review excerpt;
its wording was condensed and the unchanged full-direction test passed. Other
initial compile/assertion failures were retained and corrected, including
expectations for the intentionally retired gates.

The agency, system overview, data model, seams, affected crate architecture and
outcome/opinion documents describe these boundaries. Historical incident and
verification documents remain historical. Removing execution estimates does not
establish that every technical failure can be solved, and usage observations
are not yet a learned cost forecast.
