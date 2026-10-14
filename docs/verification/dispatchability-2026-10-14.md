# Controller dispatchability observation

Base a309d06: Claude's reviewed interface/visibility handoff on b1611ea
bounded recovery. Branch codex/analysis-dispatchability. Exact implementation
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
