# Controller dispatchability observation

Base a19223c: Claude's integrated bounded recovery, proof and interface handoff.
This replaces the first a309d06 base; validation below distinguishes them. Branch codex/analysis-dispatchability. Exact implementation
changes: new controller/dispatchability.rs and tests/dispatchability.rs, their
two module declarations, control architecture prose/metrics and this note.
Other controller integration files were not edited.

The public inherent observer returns handle::DispatchDemand using the real
controller gates, validated cumulative budget and pick matcher. It performs no
lease/run/decision/grant or lifecycle mutation. The snapshot does not wait on
a busy tick; None denotes missing, failed, draining, busy or stale loop state.
Observations older than 60 seconds cannot be presented as live demand.

Active demand includes unfinished in-memory runs and active durable leaves
until reconciled. Individually dispatchable leaf counts are potential, not a
reservation of all worker slots. Waiting reasons are first-failing gates;
matcher_unavailable combines coverage, resource, provider/account and model
capacity. It cannot alone identify which of these is missing. Research jobs
with dream_review_job and read_only_review=true are excluded from foreground,
but their occupancy still applies.

Seven new real-store/scripted-worker tests cover no mutation/dispatch, missing
coverage and dependency gates, durable budget exhaustion, review exclusion and
active work, unknown/busy/draining/stale loop, owner holds and interactive model
activity, and single-writer resources with an otherwise spare worker slot.
Full workspace: 2,269 passed, zero failed, 31 ignored (one test thread).
Workspace/all-target clippy -D warnings, format and both docs checkers pass.
Bypassing pick makes the missing-coverage regression fail (exit 101); the exact
source was restored and all seven observer tests passed again. Dedicated
branch targets prevent concurrently built workspace interfaces from mixing. The separate
ControlHandle override and dreaming availability policy remain integration work.
No live deployment or diagnostic-quality claim.

## Integrated-base revalidation

The helper now passes the complete History to pick, preserving tool-routing
requirements added by controller proof integration. Probe occupancy is read
through the existing load_by_worker. Two additional regressions require those
facts: a routed tool absent from a worker ceiling cannot be counted as ready,
and an in-flight probe consumes the worker's otherwise idle capacity. The
initial 2,269-test result above covers only the earlier base. Final integrated
workspace: 2,311 passed, zero failed, 31 ignored (one test thread). All nine
observer regressions pass; warnings-denied workspace/all-target clippy, format
and both documentation checks pass. A disk-full compile and the sandbox's
blocked ps invocation in an existing process-cleanup test were environment
failures: task-owned incremental caches were removed and final host-context
checks passed with incremental builds disabled. No test was weakened.
