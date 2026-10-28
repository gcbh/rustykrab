# Shared Git handoff recovery

Base: `4d76cc5308c3f3a11e58df7aa46aecab2dea2886` (v6.6.11).

The observed dreaming task saved the owner's “Create a new worktree and start
from main” answer at 2026-10-28 01:11:35 UTC, then raised the same conflict about
2.6 seconds later before leasing a worker. The last actual execution of that task
was October 13. This was a controller preparation loop, not a new agent attempting
and failing to resolve Git. Permission and inventory-tool repairs did not close it.

## Implemented behavior

- Git preparation returns a typed repository/parent/path observation for conflicts.
- The controller files/reuses one bounded Internal repair per project/repository/
  unordered parent pair through the existing validator and approval policy.
- The repair receives an isolated checkout at an observed parent, current project
  intent, actual waiters’ saved owner direction and a concrete merge objective, without first requiring the failed merge.
- Its result must pass normal branch/base/exact-path checks and contain both
  recorded parents. No-op reports and one-parent edits do not qualify.
- Continuation first selects independent Git tips, consuming verified descendant
  resolutions before the older conflicting branches they already contain.
- Originals wait through WaitsFor edges, so cancelling the repair preserves their
  goals. Actual original preparation is rechecked before any original execution.
- Legacy synthetic Git questions can be retired without an invented owner answer
  when technical recovery is allocated. Independent decisions/consent/approval/
  budget holds and pending-answer/event/project revision guards remain intact.
- Failed, cancelled, archived or exhausted repair identity survives restarts and
  new waiters. No new repair budget or recursive improvement chain is created for
  the same conflict. Worker-authored metadata cannot select this dispatch path.

## Executed regressions

`cargo test -p rustykrab-control controller::tests::merge_recovery`

The real-Git baseline failed when a verified two-parent resolution followed its
historical ancestor in work-row order. The corrected test checks both orderings.
The controller suite uses real SQLite and Git plus a deterministic Worker that
actually creates the supplied worktree, resolves and commits the conflict, then
performs the original useful work from the verified resolution. It covers:

1. Descendant-first continuation independent of historical work ordering.
2. One shared repair for two originals, actual isolated execution and resumption
   across a controller restart, with the main checkout unchanged.
3. Rejected one-parent/no-op results and finite reuse after failure.
4. Legacy technical-question recovery without an invented owner answer.
5. Preserved independent consent and original spent-budget holds.
6. Stale owner events and pending answers refusing prepared allocation.
7. Archived verified repair history across SQLite/controller restart.
8. Forged model-authored recovery markers and report artifacts refused.
9. Repair cancellation preserving original goals and the existing repair identity.
10. Repair budget exhaustion remaining spent across restart and new waiters.
11. Existing discovered-work approval policy gating repair execution.
12. A changed project revision invalidating prepared conflict allocation.
13. Both shared waiters’ goals and saved owner answers reaching the actual repair
    execution view. This test first failed when only the repair itself was selected.

Full required-check results are recorded with the PR. An initial sandboxed control
suite passed 511 tests but two existing loopback model-health fixtures could not
bind a socket (`Operation not permitted`). Those fixtures require execution with
normal local loopback access; their failure is retained separately from code
regressions. Intermediate fixture failures included a missing historical workspace
receipt and directly seeded spend not yet loaded into the controller cache; the
final fixtures reconstruct the real historical workspace and reopen after spend
setup rather than weakening production checks.

## Architecture and limits

The change adds a controller module and typed workspace result, using existing
work/evidence/edge/question rows and the existing Worker seam. There is no new
scheduler, model executor, persistence table, authority grant or status setter.
A controller-only evidence record on the original item binds the repair's identity,
workspace and additional ancestry check; reports cannot forge it.

This verifies deterministic orchestration and actual Git execution by the fixture
worker. It does not measure a model's conflict-resolution quality, compile/test
success of an arbitrary merge, release deployment or live original-goal recovery.
The supplied parent is observed and immutable; choosing a different project base
or discarding history is not inferred from free-text answers. Main checkout/refs,
owner decisions and existing live work are not rewritten by preparing this PR.
