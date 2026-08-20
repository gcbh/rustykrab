# Dreaming: The Self-Improvement Outer Loop

This document describes a proposed **outer loop** for RustyKrab: a longer-running,
off-cycle process -- "dreaming" -- that continuously improves how the system
executes by reviewing what has already happened and reconciling it into durable
knowledge (memory and, eventually, skills).

Status: **design / proposal.** Nothing here is implemented. The intent is to
agree on the architecture, the safety boundaries, the *objective* being
optimized, and the build order before writing code. The
[Revision history](#revision-history) records how the design evolved so the
discarded reasoning survives.

## Motivation

RustyKrab has a strong substrate for learning but no loop that drives it:

- Memory **capture is manual** -- the agent must call `memory_save`.
  Conversation history and compaction overflow (`recall_archive`) are persisted
  but never flow into memory automatically.
- The memory model is **consolidation-ready** -- `parent_memory_ids`,
  `consolidation_generation`, soft-delete (`is_valid` / `invalidated_by` /
  `invalidated_at`), and link types like `Consolidation` / `Contradicts` all
  exist (`crates/rustykrab-memory/src/types.rs`) -- but nothing uses them.
- Skills are **static** -- loaded from disk at startup; the agent *can* create
  one via `SkillsTool`, but nothing triggers that from experience, and there is
  no notion of whether a skill *works*.

## The outer loop: a two-timescale control view

The organizing idea is that RustyKrab runs at **two timescales**, and today only
the fast one exists.

- **Inner loop (exists today).** `AgentRunner`, per turn: perceive (message +
  retrieved memory + active skills) -> reason -> call tools -> respond ->
  `task_complete`. Runs in seconds-to-minutes. *Within* a turn its policy is
  fixed -- it is whatever memory and skills currently exist.
- **Outer loop (this proposal).** A standing, ongoing process that observes many
  inner-loop executions and adjusts the durable state that *parameterizes* the
  inner loop -- memory contents, skill instructions -- so that future inner-loop
  execution is better. Runs in hours-to-days, continuously, off-cycle.

The two loops are **closed over each other** -- this is what makes it a loop
rather than a one-shot pipeline, and what "improve execution in an ongoing way"
concretely means:

- Outer-loop *outputs* (consolidated memory, improved skills) become inner-loop
  *inputs* (what is retrieved, which skill fires).
- Inner-loop *executions* (traces + outcomes) become outer-loop *inputs* (the
  measurement it steers on).

### MAPE-K mapping

The canonical prior art for "a control loop that continuously improves a running
system" is the autonomic-computing **MAPE-K** loop. RustyKrab's pieces map onto
it cleanly -- and the mapping independently re-derives the build order (you
cannot Analyze/Plan/Execute without Monitor):

| MAPE-K stage | RustyKrab realization | Phase |
|---|---|---|
| **Monitor** | `ExecutionTracer` + outcome capture | 0 |
| **Analyze** | downtime read-only analysis (clusters, contradictions, per-skill success rates) | 1 |
| **Plan** | propose staged consolidations / skill edits | 2-3 |
| **Execute** | stage-then-promote with rollback | 2-3 |
| **Knowledge** | shared memory + skills + outcome history | all |

### Control-loop consequences

Naming this a control loop (not a batch job) forces us to design for failure
modes a batch job does not have:

- **A control loop needs a setpoint.** The reference it steers toward *is* the
  declared desired outcome (see [P2](#p2-you-cannot-improve-what-you-cannot-measure)).
  No setpoint -> no control, only noise amplification.
- **Stability / oscillation is first-class.** An outer loop adjusting state from
  measurements can go unstable. The decay-manager-vs-dream oscillation
  (see [Interactions](#interactions--risks)) is a textbook control instability.
  Mitigations are control-shaped: **rate-limit changes per cycle, add
  hysteresis, use low gain** (few, small changes per cycle) so the loop neither
  chases noise nor fights itself.
- **Feedback lag & credit assignment.** Outcomes are the delayed result of
  changes made cycles earlier. The manifest + `origin` provenance tags exist not
  only for rollback but to *attribute* a later outcome to an earlier change.
- **Goodhart risk.** Optimizing a proxy setpoint diverges from the true goal;
  blend a cheap proxy with an occasional ground-truth check.

### Governed, not autonomous

"Continuous and ongoing" must not drift into "the system rewrites itself
freely." The outer loop is **governed**: reversible everywhere, rate-limited,
conservative by default, and human-gated at the highest-risk edge (skills). It
is a *supervised* controller, not an unbounded self-modifier.

## Design principles

Three principles, in priority order, each learned in design review.

### P1. Off-cycle, never inline

The outer loop is the **lowest-priority background activity**. It runs only when
the system would otherwise be idle and **gets out of the way the instant real
work appears**. It must never run synchronously on the session-end path, where
it would tax latency exactly when a follow-up is likely and contend for model
quota exactly when there is live work. Session end may only *enqueue* work (an
instant INSERT); the thinking happens later, in downtime.

### P2. You cannot improve what you cannot measure

Optimization requires an **objective** (what "better" means), a **measurement**
(a signal of how we are doing), and a **search** (proposing variants). We have
the search; we mostly lack the first two. Therefore:

- **No artifact is auto-improved until its desired outcome is declared and its
  real outcomes are measurable.** Unmeasurable artifacts are *frozen*, not
  optimized.
- The same outcome signal that drives optimization is the loop's **setpoint**
  and its **rollback trigger** (did the change improve subsequent outcomes?).
  Defining outcomes closes all three at once -- which is why outcome
  instrumentation is **Phase 0**.

### P3. Reversible and conservative before autonomous

The first downtime jobs are **read-only / report-only**. Mutating jobs come only
after read-only analysis has shown value, and they mutate through a
**stage-then-promote** path so nothing is live until promoted and every promoted
change is reversible.

## Memory vs. skills: a fundamental asymmetry

The two halves differ on whether a meaningful objective even exists:

| | Memory consolidation | Skill improvement |
|---|---|---|
| **Intrinsic objective?** | Weak but real: less redundancy, fewer contradictions, "what we kept is what's later recalled". | **None.** A skill exists only to cause an outcome; "better instructions" is undefined except relative to that outcome. |
| **Progress without an external goal?** | Somewhat. | No -- it would be undirected mutation, i.e. drift. |
| **Optimization gate** | Can begin with intrinsic proxies. | **Blocked** until desired outcome is declared *and* measured. |

Consequence: **memory consolidation can start earlier; skill improvement is
gated on outcome measurement.** Treating them as one pipeline produces mediocre
memories and drifting skills.

## The optimization problem (desired outcomes)

This is the crux. Without it, "self-improvement" is just *change*, and the outer
loop has no setpoint.

### Outcome signal sources, by reliability

1. **Verifiable post-conditions** -- code compiled, calendar event exists, file
   written, cron fired. Reliable but only for *some* skills. **These are the
   skills we can genuinely optimize first.**
2. **Explicit user feedback** -- a correction, "no, do it this way," "thanks," a
   redo. Medium reliability; currently unstructured across channels, so it must
   be captured.
3. **Implicit behavioral signals** -- did the user re-ask, rephrase, or abandon?
   Did the agent need retries? Did `task_complete` fire cleanly? Cheap and
   abundant, but biased and noisy.
4. **LLM-as-judge against the skill's declared purpose** -- scalable, but
   measures *plausibility*, not correctness, and can be gamed or drift. Use only
   as a filter, never as ground truth.

Blend a cheap proxy (3) for volume with an occasional ground-truth check (1/2)
to keep the proxy honest and resist Goodhart drift.

### Skills must declare a definition of done

A skill becomes optimizable only if it says what success is. Proposed `SKILL.md`
frontmatter addition:

```toml
[outcome]
# Definition of done (required for auto-improvement eligibility)
success = "The requested calendar event exists and the user confirmed the details."
# Optional machine-checkable post-conditions, if the effect is verifiable
checks = ["calendar.event_created", "user.confirmed"]
# Signal class to trust: verifiable | explicit | implicit | judge
signal = "verifiable"
```

Skills with no `[outcome]` block are **frozen** -- they run, but the loop never
edits them.

### Skill improvement as offline learning from logged outcomes

1. Gather execution traces where the skill was used.
2. Partition by outcome signal (success / failure / ambiguous).
3. On failures, propose an instruction change that would plausibly have produced
   success.
4. **Validate before promoting** -- counterfactually against held-out failed
   traces, or via forward A/B on subsequent uses. Promote only on demonstrated
   improvement; otherwise discard.

Gated on **enough traces + a real outcome signal**, not on engine readiness.

## Architecture

### A deterministic pipeline, not an agent

The outer loop is a **deterministic batch orchestrator** that calls the model at
fixed synthesis/proposal points -- *not* an `AgentRunner` run where a model
freely decides what to do. Letting an autonomous agent drive memory and skill
mutation is more dangerous and less predictable, and it makes idempotency,
resume, and testing far harder.

### The downtime worker

Off-cycle execution (P1) needs only modest pieces, most leaning on existing
plumbing:

- **Cheap idle detection** -- a per-agent `last_activity` timestamp bumped on
  inbound; the worker runs only after N minutes of quiet. *Not* a full
  preemption bus.
- **A work queue** -- session end *enqueues* a small job (an INSERT next to the
  existing `JobStore` in `crates/rustykrab-store/src/jobs.rs`).
- **An idle-gated background worker** -- drained on the existing `infra_handles`
  task set; yields the instant activity appears.

### Small jobs + abort-and-requeue (instead of pause/resume)

The unit of work is **small** -- one session, or one small batch. When live work
arrives, the worker **aborts the current job and re-enqueues it** rather than
suspending and resuming with persisted progress. This gives immediate
"get out of the way" behavior without a pause/resume state machine, which earns
its keep only if jobs ever get long enough that discarding in-flight work hurts.

### Resource yielding is the real reason to step aside

The store is a single `Arc<Mutex<Connection>>`; even reads serialize through it,
so a job loading many embeddings for clustering *will* block live traffic, WAL
notwithstanding. And a job burning model tokens can rate-limit the user's calls.
So the outer loop **takes its own read-only connection** (WAL readers don't block
the writer), reads in small batches, and treats live activity as a signal to
**yield model budget and the connection**.

## Checkpoint / rollback (stage-then-promote)

Reversibility has two halves: the **mechanism** (below) and the **trigger**,
which is the outcome signal from [P2](#p2-you-cannot-improve-what-you-cannot-measure).

### Mechanism

A mutating cycle computes its entire change-set against a **frozen
read-snapshot** and writes it to a **staging set**. Nothing touches live memory
until an atomic **promote**:

- **Checkpoint** is implicit -- the live set is untouched until promotion.
- **Abort / pause** is trivial -- discard or keep the unpromoted diff; live
  memory is never in a half-consolidated state.
- **Promote** applies the diff in one transaction (the `unchecked_transaction()`
  pattern already used by `batch_update_stages`), after a **staleness
  reconciliation** re-verifying the snapshot's parents still exist and were not
  modified since.
- **Rollback (post-promote)** uses a manifest of what the cycle created/retired,
  built on existing soft-delete (`invalidate()` tombstones rather than deletes).

### Honest limits of rollback

Rollback is **clean only before anything depends on the cycle's outputs.** If the
live agent has since accessed, linked, corrected, or re-consolidated a
loop-produced memory, naive rollback resurrects stale parents and discards
accrued value. So rollback is offered within a **probation window** (before first
dependent access); beyond it, rollback is **best-effort and may surface
conflicts** rather than silently clobbering. It also does not restore decay /
`access_count` -- it is not a time machine.

### Proposed manifest (sketch)

```sql
CREATE TABLE dream_cycles (
    id          TEXT PRIMARY KEY,
    agent_id    TEXT NOT NULL,
    kind        TEXT NOT NULL,   -- analysis | memory | skill
    started_at  TEXT NOT NULL,
    promoted_at TEXT,            -- NULL until promoted
    status      TEXT NOT NULL,   -- running | staged | promoted | rolled_back | aborted
    summary     TEXT             -- human-readable digest of what changed
);
CREATE TABLE dream_changes (
    cycle_id    TEXT NOT NULL REFERENCES dream_cycles(id),
    op          TEXT NOT NULL,   -- created | invalidated | promoted_stage | ...
    target_kind TEXT NOT NULL,   -- memory | link | skill_proposal
    target_id   TEXT NOT NULL,
    prev_state  TEXT,            -- JSON of prior values for reversible ops
    PRIMARY KEY (cycle_id, target_kind, target_id)
);
```

Provenance: rather than overloading `ImportanceSource` (which is about the
*score's* origin), add a distinct **`origin`** tag to memory so retrieved items
carry "learned by the outer loop, cycle N" -- enabling audit, credit
assignment, and selective rollback.

## Skills are proposal-only

Because skills act as system prompts and have no clean rollback story (in-memory
`RwLock` + disk files, no manifest, not version-controlled), the loop **never
hot-registers skills**. It writes proposals to a staging area; promotion into
`SkillRegistry` is gated behind review and/or the existing-but-unused Ed25519
verification path (`crates/rustykrab-skills/src/verify.rs`).

**Open dependency:** proposals are useless without a **review surface**. If the
loop runs unattended, proposals must be pushed somewhere a human sees them (a
digest to a channel, or a review prompt on next interaction). Without that
surface, the skill tier is theater and should be cut from scope honestly.

## Build order

| Phase | What | Risk | Gate to proceed |
|---|---|---|---|
| **0 -- Instrument outcomes (Monitor)** | Extend `ExecutionTracer` to log tool/skill invocations linked to outcome signals; add `[outcome]` to `SKILL.md`. Pure data collection. | None | Outcome data flowing for at least verifiable-signal skills. |
| **1 -- Downtime read-only analysis (Analyze)** | Trigger + queue + idle-gated worker running *report-only* jobs; abort-and-requeue on activity. | None (no writes) | Reports show real, actionable patterns. |
| **2 -- Memory mutation (Plan+Execute)** | Consolidation that writes memory via stage-then-promote + manifest + probation-window rollback; low gain, rate-limited. | Medium | Consolidations measurably improve retrieval and are reliably reversible. |
| **3 -- Skill improvement** | Per-skill optimization from logged outcomes; proposal-only with a review surface. | Higher | Per-skill measurable outcomes + a working review/promotion surface. |

Notably **not** required, thanks to staging + soft-delete: a DB snapshot engine,
a job-state machine for pausing, conversation versioning, or a preemption bus.

## What downtime does and does not solve

- **Solves:** latency. Session-end cost is an INSERT; thinking happens when idle;
  live work always preempts.
- **Does not solve:** correctness. The moment a job *mutates* autonomously, you
  still need an outcome signal to know if it helped and to undo it if not -- why
  Phase 0 precedes Phase 2 and early jobs are read-only.

## Interactions & risks

- **Loop stability vs. the decay/lifecycle manager.** Both decide "what matters"
  -- decay demotes, the loop promotes episodic->semantic. Without damping they
  oscillate. Define precedence (loop promotions set a decay floor; recent
  explicit user signals win) and rate-limit loop changes per cycle.
- **Clustering quality.** Cosine >= 0.85 transitive closure can form giant
  clusters or merge similar-but-distinct facts into a confidently wrong memory.
  Needs cluster-size caps and a low-confidence "do not merge" guard.
- **Proxy bias / Goodhart.** Implicit outcome signals are noisy; never let a
  single proxy drive irreversible change without a ground-truth cross-check.

## Open questions

- **Per-skill outcome bootstrapping.** Requiring `[outcome]` is clean for *new*
  skills; how do we backfill desired outcomes for existing or agent-authored
  ones?
- **Ground-truth coverage.** Verifiable post-conditions cover only some skills.
  For subjective skills, is gated/audited LLM-as-judge acceptable, or do they
  stay frozen?
- **Review surface.** Which channel / UX surfaces skill (and risky memory)
  proposals for human approval?

## Key file references

| Concern | File |
|---|---|
| Inner loop; `InboundEvent`; execution tracing (extend for outcome capture) | `crates/rustykrab-agent/src/runner.rs` |
| Harness profiles (budget caps) | `crates/rustykrab-agent/src/harness.rs` |
| Memory soft-delete, transactions, `with_conn` | `crates/rustykrab-memory/src/storage.rs` |
| Memory model (provenance, consolidation fields) | `crates/rustykrab-memory/src/types.rs` |
| Lifecycle sweep / near-duplicate detection | `crates/rustykrab-memory/src/lifecycle.rs` |
| Compaction overflow store (`recall_archive`) | `crates/rustykrab-store/src/recall_archive.rs` |
| Store connection / WAL / shutdown checkpoint | `crates/rustykrab-store/src/lib.rs` |
| Cron / scheduled jobs (queue lives near here) | `crates/rustykrab-store/src/jobs.rs` |
| Skill registry, disk loading, `SKILL.md`, verification | `crates/rustykrab-skills/src/` |
| Orchestration (where the enqueue hook attaches) | `crates/rustykrab-gateway/src/orchestrate.rs` |

## Revision history

- **v3 (this revision).** Reframed around the **outer loop**: an explicit
  two-timescale control view (inner `AgentRunner` loop vs. ongoing outer loop),
  MAPE-K mapping, and control-loop consequences (setpoint = desired outcome;
  stability/hysteresis/low gain; feedback lag & credit assignment; Goodhart).
  Added the "governed, not autonomous" clause.
- **v2.** Off-cycle from day one (trigger + queue + idle-gated worker, never
  inline); outcome measurement as a first-class principle and Phase 0; skills
  gated on measurable outcomes; deterministic pipeline instead of an agent loop;
  stage-then-promote instead of mutate-then-undo; small jobs + abort-and-requeue
  instead of pause/resume; dream takes its own read-only connection.
- **v1.** Original draft: dreaming as an `AgentRunner` run with a dream profile;
  step-machine mutating live memory with a manifest for undo; pause/resume via
  persisted progress; evaluation left as a single open question.

## Relationship to existing docs

See `MEMORY_ARCHITECTURE.md` for the memory subsystem this builds on, and
`crates/rustykrab-memory/DEFERRED.md` for previously-deferred consolidation work
that the outer loop would finally drive.
