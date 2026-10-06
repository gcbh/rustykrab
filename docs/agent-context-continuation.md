# Agent context continuation

Recovered Claude session: “Agent context management strategy”, session
0c1d972a-38fd-57b5-8fd2-64b1d782122d, in the local Claude project transcript.
Its control plan is [control-layer-and-worker-fleet.md](plans/control-layer-and-worker-fleet.md).

## Integrated work

The newer control checkout already contained Phases 1–3, 5 and 6. This
branch combines that checkout (5cae23b) with the separate Phase 4 questions
branch (030dce6), preserves the newer worker lifecycle and shutdown behavior,
and finishes the controller planner's file-based definition and host wiring.
Questions persist, policy can settle defaults, answers resume work, and
nightly evaluation reads real question and answer records.

The monitoring suite adds an authenticated dashboard, bounded JSON
observations, Prometheus metrics and the monitor CLI health check. It tracks
agent health and checks, assignments, controller execution, questions,
notifications, recovery events, finalized spend, and verified evidence.
Read-only observations do not renew a lease or a worker check. The
[monitoring guide](agent-monitoring.md) explains operation and evidence limits.

Real execution checks found and corrected general peers being mistaken for
planners, missing question notification provenance, a cron budget exceeding
the default approval threshold, and browser GETs rejected by the Origin
guard. Browser verification also corrected mobile overflow and distinguishes
unverified records from evidence the controller has verified.

## Verification and boundaries

Required formatting, Clippy, workspace tests and architecture checks run
against this isolated checkout: 2,067 workspace tests pass, with 31
intentionally ignored tests. Formatting, Clippy and architecture checks pass.
The full daemon suite passes 63 scenarios with zero failures and seven
expected failures: six separate project-planning targets and the existing
dreaming consolidation-cycle target. These are not counted as implemented. The scripted end-to-end suite boots actual
daemon processes and a SQLite database; it includes controller commit/diff
verification and a monitor scenario that checks evidence, released leases,
CLI exit status, metrics and served assets. Browser verification covers
question answering/resumption, evidence details, filtering, pause behavior,
mobile layout and connection failure.

Execution evidence is recorded in the task directory alongside this worktree.
The scripted provider establishes lifecycle and integration behavior.
Real-model reasoning and the outstanding Phase 0 measurements require their
separate model/measurement runs.

The original rustykrab checkout and the live builder daemon are untouched.
The new system runs separately as com.gcbh.rustykrab.agents, with its own
data directory and Tailscale HTTPS port 8443 (see the monitoring guide).
The main daemon remains on its existing build and port 443. The integrated
foundation, native runtime/context layer and project dreaming are merged in
GitHub PRs #664, #665 and #666, respectively. Each PR passed all seven hosted
CI jobs. RustyKrab 6.0 records these changes as one major release milestone;
see [CHANGELOG.md](../CHANGELOG.md) for release scope and verification limits.
The separate PR delivery implementation is still required for Phase 7;
delivery Phase 12 is still required for full Phase 8 self-management. Existing
update-supervisor code does not satisfy those missing delivery contracts.
The control plan keeps those dependencies explicit.
