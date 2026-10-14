# Agency and self-correction execution review — 2026-10-14

Reviewed running v6.5.6, commit `5bc3309ed4cda88695c65be7ce09db6cabfe723f`.
The baseline was rechecked at 20:00 UTC. The controller held its lock, was not
draining, had zero failed ticks and no runs in flight at that instant.
This review combines read-only live observations with source inspection;
it does not establish model quality or claim that proposed recovery is deployed.

## What works

The system has one durable work graph/controller, native subscription workers,
local inference, provider-neutral frozen project/discussion handoffs, isolated
repository worktrees and host-recorded command evidence. General execution and
capability-based matching already replaced historical agent/category gates
in the reviewed source. Investigation kits include prior attempts, source/build
identity and redacted daemon log slices. Fix claims must land a verified commit
or replaying classifier rule. These are useful foundations to simplify around.

## Where agency and recovery break

| Observed cost or source behavior | Mechanism and verified source location | Design implication |
|---|---|---|
| At 19:09 UTC, one diagnostic item had at least 491 runs and about 8.7 hours of recorded run time against a 3,600-second item budget. | `controller/filing.rs::parked_on_landed_capability` accepts any already-Done capability prerequisites; `controller/tick.rs` sweeps these back to queued. `controller/spend.rs::remaining_budgets` deducts spend for parent allocations, while `tick.rs` constructs a new leaf brief from the full item budget. | Fresh recovery signals and cumulative leaf limits must survive every retry and restart. |
| The repeated failure asks for controller dispatch to a specific Claude executor; acquire/build items are Done after installing skill instructions. | `crates/rustykrab-tools/src/skills.rs::SkillTool::execute` returns the skill body. Capability verification in `controller/tick.rs::judge` checks catalog state. An internal fix's landing rule in `errors/mod.rs::LANDING_RULE` checks a commit or classifier replay, a different workflow. | Neither a skill's existence nor a landed source change establishes recovery of the original operation. Preserve these distinct proofs. |
| Thirty-five blocked handoffs name the same commit pair and conflict in `crates/rustykrab-agent/src/external_worker.rs`. | `crates/rustykrab-control/src/workspace.rs::continuation_base` / `integrate_base` refuse conflicting history; `controller/project_context.rs::hold_context` surfaces a technical question on each affected item. | Keep conflict refusal, but give one integration repair an owner and all affected work. |
| Seventeen of eighteen queued rows wait on unfinished ordering prerequisites; the eighteenth is a parent. The only ready task lacks a matching artifact grant. | Readiness and `controller/tick.rs::pick` correctly enforce dependencies and resource coverage. Worker health alone cannot make these rows executable. | The manager must validate repair feasibility and provide precise reasons for waits; more workers alone cannot drain this queue. |
| Two old items account for 79 of 144 open questions. Rephrasings name different attempt/worktree paths; inherited planning-only text appears on implementation work. | `questions/router.rs::same_question` normalizes text, not operational identity. `controller/filing.rs` inherits constraints. Native default tools in `external_worker.rs` have since broadened. | Reconcile scope and runtime permissions using fresh evidence; canonicalize the operation and relevant authority, and preserve real owner decisions. |
| Dreaming is enabled but no project has a completed review within the current interval. | `crates/rustykrab-cli/src/dreaming.rs::advance` yields to any `foreground_needs_agent` row, including Queued and Ready, without checking whether an executor can run it. Preparing reviews use the same gate. | A stalled backlog can starve its own analysis. Reserve a bounded opportunity for incident diagnosis based on useful execution progress. |
| Five reviews completed, six failed; four proposals, two accepted, two pending; zero measured post-change outcomes. Latest review failed on a non-verbatim/unresolvable `repository-1` citation. | The live dreaming view records these values. `project_review.rs::validate_generated` enforces exact frozen citations. | Do not weaken evidence validation to make the dashboard look healthy. Investigate failed review execution and measure later original-work outcomes. |
| Project review inputs have seven fixed repository excerpts and at most thirty directly project-tagged work rows, carrying status, objective and acceptance. | `dreaming.rs::freeze` omits per-run failures, cumulative spend, executor coverage and original-operation replay receipts; its operations observation covers dreaming's own metrics. | The analytical loop does not receive enough detail to diagnose all the runtime failures above. Bounded, selective operational evidence needs an explicit implementation. |
| Completion metrics can rise while the backlog remains blocked. | `crates/rustykrab-dream/src/evaluate/metrics.rs::compute` measures closed non-proposal leaves, including capability/internal work; it excludes still-open goals from that completion-rate denominator. | Add original-goal throughput and stalled-work measures alongside existing, honestly labeled metrics. |

The controller paths in this table are under `crates/rustykrab-control/src/`.
Source anchors at base `5bc3309`: `controller/filing.rs:164`,
`controller/tick.rs:514` and `:1803`, `controller/spend.rs:58`,
`errors/mod.rs:369`, `workspace.rs:432`, `controller/project_context.rs:274`,
`questions/router.rs:228`; also `crates/rustykrab-tools/src/skills.rs:342`,
`crates/rustykrab-cli/src/dreaming.rs:364`, `:457` and `:807`, and
`crates/rustykrab-dream/src/evaluate/metrics.rs:104`.
The backlog observations are dated snapshots, not permanent counts. Evidence
IDs and private local snapshots are retained in the operator's investigation;
raw transcripts, tokens, personal integration content and host paths are not
published here.

## Concurrent repair and remaining priorities

[PR #693](https://github.com/gcbh/rustykrab/pull/693), inspected at `702539b`,
adds isolated-worktree concurrency, exact readable/writable artifact grants,
recovery of identified stale dependency waits, and capability recovery only
when at least one prerequisite closed after the current failure. Its CI passed and
it merged at 20:01:40 UTC on October 14; release builds were still running at
the subsequent check. Its tests use deterministic model adapters and real Git
worktrees; that proves scheduling/verification behavior, not live model
throughput or deployment. Refresh its status before using this dated report.

That patch does not itself establish cumulative leaf budget enforcement,
resolution of the shared merge conflict, retirement of historical permission
questions, original-operation capability probes, or anti-starvation for analysis.

The next slices should be ordered by affected original work:

1. Bound repeated attempts and enforce cumulative leaf spend before leasing;
   distinguish a repair landed, present in the running system, and successful
   on the failing operation.
2. Resolve the common integration incident and reconcile stale permissions and
   planning constraints; revalidate affected work with independent holds intact.
3. Supply compact operational failure/coverage/spend evidence to analysis and
   ensure incident diagnosis can run while a backlog is unexecutable.
4. Measure original-goal recovery, recurrence and owner effort; connect those
   receipts to dreaming's meta-evaluation and the owner's separate UI views.

## This change's scope

The [machine-readable recovery corpus](../evals/agency-recovery-cases.json)
records ten reproducible failure patterns with explicit pass/fail conditions.
Every case is marked `specified_not_run`; these are implementation/evaluation
requirements, not a claim that ten new recovery tests already pass.

The accompanying [design direction](../architecture/08-agent-agency-and-self-correction.md)
is linked from both repository instruction entry points and the architecture
index. Project dreaming now captures its committed contents, with an 8,000-byte
bound, for both generation and fresh meta-evaluation. Trusted review prompts
ask about useful agency, stalled analysis and original-work recovery; repository
text remains evidence and cannot grant tool or execution authority.

This is an information and development-guidance improvement. It does not
silently change production permissions, arm memory mutation, introduce a new
scheduler, resolve the backlog, or claim that the remaining recovery slices
are implemented. Real Git/store regression checks distinguish committed
direction from dirty working-tree text, preserve frozen review input, and
make missing or oversized direction explicit.
