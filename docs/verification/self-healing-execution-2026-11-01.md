# Clean workspaces and observed recovery — 2026-11-01

Base: `edae99901dae7878868322256a110e21c7a560b7` (v6.6.17).
Branch: `codex/self-healing-execution-20261101`.
Scope: controller workspace selection, worker liveness, concrete shared-merge
progress, existing routing escalation and regression fixtures. No live mutation,
merge, release installation or owner-answer application was performed.

## Observed failure and source explanation

The read-only live snapshot at 2026-11-01T22:57:17Z reported a healthy v6.6.17
daemon with zero live runs, zero budget holds and five independent approval
holds. Shared repair `e8402ec7-7ce7-4adc-a251-038ad4ef8e04` was blocked for a
decision after finite recovery, with 6,287,349 recorded tokens over eight usage
records. The latest attempt ended after 1,804,754 ms with the 1800-second stall
reason. Removing estimated task budgets had worked; this was another boundary.

At this exact source base, `plan_workspace` integrated every completed project
commit, irrespective of the next item's dependencies. A clean directory could
therefore receive an old, conflicting integration instead of current main.
`ProgressLedger` had no caller installing an implementation, so the controller
had no actual activity signal to renew its inactivity timer. Switching after a
local loop could select another named local slot advertising the same model.

## Implementation and proof boundaries

Independent code work fetches the remote default commit (normally main) and
starts a separate clean worktree. No shared branch, FETCH_HEAD, index or worktree
is reset or cleaned. Local-only repositories use main/master. Remote failure is
visible and periodically rechecked by existing handoff recovery; it does not
silently fall back to stale HEAD. Explicit dependencies still integrate, and
trusted failure reproducers/shared repairs retain exact pinned parents.

Actual local runner messages and native stdout/stderr renew inactivity. Shared
merge repairs additionally need a new conflict-file/index/commit fingerprint
within the existing window. Repeated inspections or previously seen states do
not count. This is a narrow progress heuristic, not a proof of correctness or a
generic supervisor. Long silent checks can still expire. Loop escalation uses
configured cost tiers and skips same-kind aliases advertising the same model;
it does not manufacture a compatible worker or override access/owner holds.

Old recorded dependency edges and surfaced decisions are retained. In particular,
this change does not automatically restart the existing live repair or declare
its original waiters effective. Those need a separate review of their current
state after rollout. There is no new scheduler, table, authority or task budget.

## Execution evidence

- Full `cargo test --workspace`: 2,505 passed, 39 ignored after updating conflict fixtures to
  declare the history they intentionally depend on.
- `cargo clippy --workspace --all-targets`: passed; final check rerun after the
  fixture adjustment is retained in the local evidence directory.
- Scripted `scripts/e2e.sh`: 70 passed, seven expected failures, no unexpected failures.
- Formatting, architecture regeneration/verification and environment-doc checks:
  passed. No schema or environment setting changed.
- Real Git regressions: fresh upstream despite a dirty feature checkout; clean
  isolated execution while shared main has an unresolved merge; unavailable
  upstream refusal; unrelated conflicting history ignored; explicit dependency
  merge, verified repair reuse, restart/archive and independent holds preserved.
- Activity/progress regressions: repeated observed activity survives multiple
  inactivity windows, silence still expires, unrelated inspection output does
  not renew merge progress, real changes do, revisited states do not, and a
  higher-tier alias of the same local model cannot consume the switch.
- Isolated no-model gateway fixtures: self-correction and bounded unresolved
  recovery both pass; original/main checkout, historical usage, failed partial
  work and independent consent are checked.

The opt-in `scripts/verify_usage_only_recovery.py --expect native_recovery`
fixture used a separate daemon/data directory, a tiny Rust crate, two real
conflicting paths and the existing authenticated Codex CLI registered through
RustyKrab. It contacted no live RustyKrab gateway or work database. The local
executor was deliberately repetitive; Codex performed the actual resolution.
The result at 2026-11-01T23:16:26Z was:

1. Local repetition became model/loop; the same repair switched to Codex.
2. The first native report failed command verification: its `cargo test` claim
   did not match the recorded commands. It was not accepted as completion.
3. The existing Repair rung returned that evidence to Codex. The next attempt
   produced an accepted Git result, then the original operation completed.
4. An independent checkout of the verified merge passed `cargo test --offline`
   and `git diff --check`, preserving broad tool authority and the older feature.
5. Both recorded parents are ancestors of the verified merge; shared main and
   an unrelated consent hold remained unchanged. Fixture processes were stopped.

Native repair commit: `847af43cad0876a305bff1f3a38ade1920f53bab`.
Parents: `da4620317d38a4b4f60784a91e64e6243e93536b` and
`5d68d8adc419a4590e464c5d69ae65a286bf988d`.
Tested binary SHA-256: `f88b530902177ddb8f9ddd093c619ffebd346a0dfe7a9f5b2460ccd1bc8b7334`.
Two native attempts recorded 196,228 and 209,355 tokens, taking 32.056 and
33.580 seconds. This is one observed successful recovery, not a reliability rate.
A later wrapper-only adjustment prevents a stale host counter from masking
native activity for a continued conversation; ordinary fresh-run behavior in
this fixture is unchanged and is covered by the final workspace regressions.

Local evidence is retained outside the repository under this chat's
`antifragile-collaboration/`: `self-healing-live-before.json`,
`self-healing-workspace-verified.log`, `self-healing-clippy-verified.log`,
`native-recovery-20261101-b/receipt.json`,
`native-recovery-20261101-b/verified-merge-cargo-test.log`,
`self-correct-recovery-20261101/receipt.json` and
`stalled-recovery-20261101/receipt.json`. Private native transcripts remain local.

## Rollout status

Implementation and isolated execution are verified as described above. This
branch is not deployed, and the existing live blocked repair has not resumed.
Passing fixture work is not evidence of live original-goal effectiveness.

Upstream selection is prepared outside the controller command lock and shared
per repository for one scheduling pass. Dispatch rechecks current work and scope;
a repository first appearing during reconciliation waits until the next pass.
A stalled loopback Git remote regression verifies cancellation completes while
the fetch is still pending and cancelled work never reaches the worker.

## Final candidate recheck

After the unlocked upstream-preparation change, all required checks passed again:
2,505 workspace tests passed (39 ignored), workspace clippy passed without
warnings, and format/architecture/environment checks passed. The final binary's
SHA-256 is `49cce0ca13401efdb3278d4efa37970147598a2e2af3886dc715bbca026b3d60`.
The native recovery fixture passed again at 2026-11-01T23:29:38Z. Its verified
merge is `6927631327f4ba0256fb738eef1e3a78fb15e9a0`, containing parents
`9c4d15cdc23bbf4ce37263a0eac40a0089822350` and
`4b41257ba6b5146e7cbad72d1d6526a324a4e862`. Independent Cargo tests, shared-checkout
preservation, original-task completion and independent consent all passed.
Again the first native check claim was rejected and the next native attempt
corrected it. Native usage was 222,553 and 185,044 tokens over 30.012 and 33.636
seconds. These are observed costs, not forecasts or caps.

The final restart-mid-graph end-to-end scenario also passed. Evidence:
`native-recovery-20261101-final/receipt.json`, `self-healing-e2e-full-report.json`,
`self-healing-e2e-final-restart.log`, `self-healing-workspace-verified.log`,
`self-healing-clippy-verified.log` and `self-healing-cli-verified.log` in the local
evidence directory above. No live recovery or rollout is claimed.
