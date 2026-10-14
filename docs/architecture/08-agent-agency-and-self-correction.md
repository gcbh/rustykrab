# Useful agency and self-correction

Owner direction, recorded 2026-10-14: simplify the system so an agent can take
the actions its work requires, receive the information needed to choose those
actions, and improve the system through analysis of actual execution.

This is the design target for development, harness evaluation and dreaming.
It does not itself grant new permissions or establish that the target is built.
The dated [execution review](../verification/agent-agency-review-2026-10-14.md)
separates observed behavior, the concurrent repair and remaining work.

## Success means useful work resumes

RustyKrab is the owner's work manager. Conversations and schedules create
durable goals; the manager allocates them to Qwen/local inference, native Claude
and Codex subscription runtimes, and registered infrastructure. Prefer local
inference for work it can complete correctly; use frontier runtimes when
complexity, failed attempts or needed capabilities justify them. An agent name
or historical specialty must not prevent otherwise feasible execution.

Use the existing work graph, discussion, evidence and controller. Avoid a
second queue, an independent incident ledger or model-authored completion
flags. A change earns its complexity through improved completion of the
original goal, lower effort from the owner, or better recovery from a fault.

## Make action feasible

Give each execution a coherent authorized workspace and working tools to read,
edit, test and inspect its work. Avoid repeated permission requests for routine
actions already inside that scope. Match real tools, effects, resources,
account capacity and access before leasing. Report the exact missing input,
access, dependency or capacity when no executor fits.

Prefer explicit operations for controller dispatch, integration and service
management. A prose recipe cannot implement a host operation. Skill discovery,
installation and catalog membership prove availability of instructions; they
do not prove the requested operation works on its intended executor.

Permit concurrent isolated worktrees and disjoint work. Serialize actual shared
writes and integration. Temporary worktree paths must not turn one approved
operation into a fresh consent question on every attempt. Repair contradictory
inherited instructions with provenance: a planning-only instruction for a
planning step must not silently forbid a later authorized implementation step.

Authority should follow the owner's goal and the consequences of the action.
Routine reversible work inside delegated scope should proceed. Payments,
external communications, destructive actions and changes outside delegated
scope still use the owner's applicable authorization. Do not remove a boundary
without explaining its purpose and the concrete capability that replaces it.

## Make the relevant state observable

Every successor needs the current objective and acceptance, owner decisions,
remaining budget, exact code/build base, current access, dependencies and why
they wait, last failure, previous repairs, and evidence of what actually ran.
Keep that state provider-neutral so Claude, Codex and local inference can
continue the same work without another provider's conversation history.

Lead with a compact execution view and an index into durable details. Support
selective reads of run transcripts, failing commands and exit results, relevant
source, active permission configuration and investigation bundles. Record
omissions and distinguish host observations, owner direction, model reports
and hypotheses. More context is useful only when it helps choose or verify an
action; dumping accumulated history can obscure the current requirement.

Dreaming needs operational evidence of blocked work, retry/spend history,
repair deployment and subsequent outcomes. Selected repository prose and a
model's quality grade alone cannot establish runtime health. Failure to gather
needed evidence is a visible limitation requiring repair, not a healthy result.

## Close the recovery loop

1. Detect stalled goal progress or a recurring failure, preserving its evidence.
2. Group the common technical cause and affected work under one owned repair
   in the existing graph. Ask the owner for a choice only when a real choice
   about scope, priorities or consequences is unresolved.
3. Give the repair executor a reproducer, the failed run's inputs/access and
   source/build version, previous attempts, and a checkable acceptance.
4. Repair in an isolated environment. Check the original failing operation on
   the intended executor with representative permissions, inputs and resources.
5. Integrate, test and promote through the authorized delivery path. Record
   which repository commit, runtime configuration and daemon build contain it.
6. Resume affected work once per new verified repair/configuration generation.
   Preserve independent holds. A stale completed prerequisite is not a new
   recovery signal; a passing test on an undeployed fix is not live recovery.
7. Measure affected goal progress after promotion. If the fault recurs with
   the fix present, retain the failed hypothesis, reassess or reverse it, and
   strengthen the regression scenario.

Use cumulative item budgets across attempts, finite repair budgets and a stop
for repeated failures without a relevant state change or new evidence. These
make experimentation informative and leave resources for a fresh diagnosis.
Do not spend the same full budget on every retry or allow repairs to form an
unowned chain of blocked repairs.

## Analysis must remain available during failure

Normal background optimization can yield to useful foreground work. Incident
analysis must have a bounded opportunity to run when the backlog is stalled,
even when queued rows exist. Queue presence is not evidence of executable work.
Expose analysis freshness, skipped/deferred reasons, failed reviews and missing
measurements. Give read-only diagnosis a separate opportunity without allowing
its model to widen execution authority or bypass result verification.

Dreaming and its fresh meta-reviewer should examine whether the loop detects
real stalls, receives sufficient evidence, proposes feasible repairs, verifies
the original operation, resumes affected work and learns from recurrence. Keep
citation and independent assessment gates; repair invalid output rather than
accepting unsupported recommendations. A review is analysis, a proposal is a
hypothesis, and a measured operational result is evidence of improvement.

## Evaluation and the owner's view

Track original-goal completion and time to useful output separately from repair
task completion. Measure repeated failures after a repair is actually present,
time to recovery, unnecessary owner interventions, cumulative spend per useful
result, oldest stalled work and review freshness. State the population, baseline,
sample and omissions. Zero measured outcomes means improvement is unmeasured.
These are requirements; metrics absent from the current registered schema must
be implemented before a reviewer claims to have measured them.

Replay real fault patterns: a discovered skill whose operation still fails;
stale Done prerequisites; many attempts across one budget; one merge conflict
blocking many items; missing artifact access; planning constraints inherited by
implementation; a stalled backlog suppressing review; and handoff from local
inference to a native runtime. Include failure, recovery and recurrence checks.
Every incident should leave a reusable regression, corrected instruction or
capability probe whose value is checked on subsequent work.

The main UI should separate owner decisions, technical incidents, active work
and verified results. One shared incident can explain many affected items.
Show the responsible executor, current action, precise wait, next recovery step
and result evidence. Work discussions retain owner direction across agents.
