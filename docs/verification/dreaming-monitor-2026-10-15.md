# Dreaming availability: implementation and UI audit

Base: draft #703, `1f1669d57671b9b0fc6bc98c5f3e50e095abc52c`.

The Systems view previously showed that project reviews were enabled and
reported cumulative review metrics. It ignored the admission and latest-review
fields introduced by #701. A waiting backlog, active foreground work, a stale
controller observation and an exhausted daily allowance therefore looked the
same in this part of the dashboard. Configuration did not explain execution.

## Design available for audit

Keep the existing Systems → Dreaming and meta-evaluation section. Add a compact
"Review availability" group before its cumulative metrics:

- **Last decision:** the server's typed reason, including an idle or bounded
  incident opportunity, foreground deferral, unavailable/stale observation,
  daily limit, no due project, or review already in progress.
- **Decision check and work-demand observation:** separate ages so a recent
  status request cannot disguise old scheduler evidence.
- **Active, runnable and waiting work:** measured counts with their reasons;
  missing measurements stay "not observed". An observed empty waiting map is
  labeled "none in this observation".
- **Last completed and failed project review:** separate receipt timestamps,
  alongside the existing expectation-pass and outcome-analysis timestamps.

An opportunity is described as "may start" or "permits a bounded incident
review", not as a running cycle. "Last decision" remains accurate when the
view is paused; its age is visible. The existing Review projects now button
requests a review through the existing API. Capacity and daily limits still
apply, and the browser does not enable, schedule or reserve work itself.

Older replies without admission and future unknown decision variants show
unreported availability. They never become idle capacity. Model-provided and
future waiting labels enter textContent, including angle brackets. Refreshing
or losing the dreaming snapshot clears prior evidence.

## Implementation to inspect

- `crates/rustykrab-gateway/static/monitor.js`: renderDreamingAdmission consumes
  the existing DreamingView inside renderDreaming. No additional request.
- `crates/rustykrab-core/src/dream_review.rs`: existing ReviewAdmissionDecision,
  ReviewAdmission and DreamingView define the server observations; unchanged.
- `crates/rustykrab-cli/src/dreaming.rs`: existing view/admission construction
  and limits remain authoritative; unchanged.
- `scripts/test_dreaming_monitor.mjs`: executes the shipped rendering functions
  with real formatting helpers and a DOM fixture that rejects innerHTML and
  any attempted fetch. CI runs it beside the existing browser regressions.

This is a small observation fix before a larger UI audit. It does not regroup
incidents, redesign navigation, implement native-path capability proof, or
measure live diagnosis quality and original-goal recovery. No new API, storage,
dependency, authority, background task or deployment is introduced.

## Verification

- Eight focused renderer regressions pass. Removing only the actual admission
  renderer call makes all eight fail; exact source restoration passes again.
- All seven browser suites pass: **41 tests, zero failures**.
- Required full workspace: **2,333 passed, zero failed, 32 ignored**, one test
  thread; workspace/all-target warnings-denied lint, format and architecture/
  environment documentation checks pass. Four env-checker fixtures pass.
- The shipped renderer/helpers and stylesheet were inspected in a browser
  fixture at desktop and 390×844 phone dimensions. The phone document and
  scroll widths are both390px. Incident, stale and older-server states were
  inspected; screenshots and observation text are retained in the Codex task.
  The temporary server/tab were closed and viewport override reset.

This verifies display behavior with fixtures; it is not a full authenticated
monitor, live Tailscale session or proof of backend scheduling/model quality.
No new local full-daemon suite was needed for this browser-only observation
change. Hosted checks will build the published source independently.
