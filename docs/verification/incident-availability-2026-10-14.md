# Bounded incident-analysis admission

Implementation base 3be65a2: verified controller bridge 9a65b86 plus #696
1a4f23d/#694. Base workspace 2,321 passed, zero failed, 31 ignored and required
gates passed. One existing real-Git scenario_17 test initially saw Running; the
unchanged isolated case and full retry passed. Failure logs are retained.
The first canonical integration base was e8145c0a8c7352dc82ff9db2001a32b3eecaa28d,
Claude's bridge/local-wrapper/#696/#694 composition. Its reported 2,322-test
and 69-pass/7-xfail daemon results are attributed to Claude; this branch's own
whole-workspace and execution evidence are recorded below.

The host consumes fresh real controller demand, with a five-second observation
read bound and 60-second freshness limit. It does not count all queued rows as
useful execution or implement a second matcher. Both automatic cycle creation
and each new generator/meta job obey the same capacity rule. Marker/archive
recovery stays idempotent; output/deadline housekeeping can advance while busy.

| Observation | Admission |
|---|---|
| Active or dispatchable foreground | Defer new inference |
| Fresh waiting-only backlog | Due bounded incident review |
| Fresh empty foreground demand | Due idle review |
| Unknown/error/timeout, stale/future demand, unhealthy/draining/unowned loop | Explicit defer |
| No enabled/due project, daily limit, existing cycle | Exposed in monitor admission state |

Force can persist Preparing and bypass the interval, but cannot bypass capacity
or the three-cycle/24-hour limit. Each inference leaf retains its existing four
iterations, 60,000 tokens, 600 seconds and zero rung allowances. Existing
cumulative guard/breaker and independent holds stay authoritative; no grant or
additional retry ledger is created. There is no immediate model-slot preemption
or assertion that an analysis worker can already be leased.

Fifteen focused dreaming tests pass, seven new: known/fresh demand and stale or
future rejection; actual unexecutable backlog admission with owner holds retained
and real controller generation execution; forced Preparing/foreground priority;
meta deferral/recovery; missing/failed demand without private error leakage and
old-view compatibility; daily cap/failure freshness after database reopening;
and a Research leaf blocked by cumulative spend on a reconstructed controller
without executing the model or adding a grant.

Typed view facts include checked/observed times, null unknown counts and latest
project-review completion/failure. Old views have no observation rather than a
fabricated idle state. Existing monitor warnings show demand observation faults.
UI rendering of the new structured fields remains separate. Frozen input and
citation/publication/owner-review rules remain unchanged. Component fixtures
demonstrate bounded scheduling opportunity, not live model diagnosis quality or
original-goal recovery. No live deployment occurred.

Final whole-workspace checks on e8145c0 plus this patch: 2,329 passed, zero
failed, 31 ignored (one test thread); warnings-denied workspace/all-target
clippy, format and both documentation checks passed. Reintroducing the old
queued-row suppression makes the actual backlog-admission regression fail
(exit 101); exact source hash restored and all fifteen focused tests pass again.
Machine-readable logs/hashes and any daemon smoke report are retained in the
calling task's antifragile-collaboration directory, tied to the implementation
commit. The stale pre-canonical full run was cancelled rather than counted.

## Fully linear review stack

Final linear base is Claude's layer-4
4c95a73c4ddf10892beebb20f275d51d1ec89ed6, descended from #695 bc32e0c
on #696/#694. Restacking changed only generated CLI architecture counts;
admission and core-view sources are byte-identical to the verified fc0972c
implementation. This base includes the reviewed local-wrapper serial deadline
and bound-directory execution fixture. Independent final workspace checks:
2,330 passed, zero failed, 31 ignored; warnings-denied workspace/all-target
clippy, formatting and architecture/environment checks pass. The retained
old-gate negative control has the same exact restored admission source hash.
The six-file admission diff excludes Claude's controller and wrapper changes.
Clean-commit daemon evidence remains separately tied to its recorded revision;
main merge, deployment, model quality and original-goal recovery are not inferred.
