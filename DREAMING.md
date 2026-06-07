# Dreaming: Off-Cycle Self-Improvement

This document describes a proposed **dreaming** facility for RustyKrab: an
off-cycle, low-activity self-improvement loop that reviews what has already
happened and reconciles it into durable knowledge (memory and, eventually,
skills).

Status: **design / proposal.** Nothing here is implemented. The intent is to
agree on the architecture, the safety boundaries, the *objective* we are
optimizing, and the build order before writing code. This revision supersedes
an earlier draft; the [Revision history](#revision-history) records what
changed and why, so the discarded reasoning survives.

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

"System learning" is three loops with different signals:

1. **In-the-moment capture (online).** Retain salient facts during/after a turn.
2. **Reconciliation / consolidation (offline).** Merge near-duplicates, resolve
   contradictions, promote episodic specifics into semantic generalizations.
3. **Skill-ification & skill improvement (offline).** Detect recurring
   procedures and crystallize them into skills -- and improve existing skills
   toward better outcomes.

Dreaming moves loops 2 and 3 (and the heavy parts of 1) into downtime so they
cost no online latency.

> **Provenance note.** "Dreaming" is used loosely. The closest real prior art is
> *sleep-time compute*, *generative replay* / memory consolidation, and
> *skill-library* construction (e.g. Voyager). We scope deliberately to
> **consolidation and improvement based on what actually happened**, not
> generative rehearsal of hypothetical scenarios -- the latter can manufacture
> confident falsehoods that get stored and later retrieved as if real, an
> unacceptable contamination risk for a security-first gateway.

## Design principles

Three principles, in priority order. Each was learned the hard way in design
review (see [Revision history](#revision-history)).

### P1. Off-cycle, never inline

Learning is the **lowest-priority background activity**. It runs only when the
system would otherwise be idle, and it **gets out of the way the instant real
work appears**. It must never run synchronously on the session-end path, where
it would tax latency exactly when a follow-up is likely and contend for model
quota exactly when there is live work. Session end may only *enqueue* work
(an instant INSERT); the thinking happens later, in downtime.

### P2. You cannot improve what you cannot measure

Optimization requires an **objective** (what "better" means), a **measurement**
(a signal of how we are doing), and a **search** (proposing variants). We have
the search (an LLM can propose edits); we mostly lack the first two. Therefore:

- **No artifact is auto-improved until its desired outcome is declared and its
  real outcomes are measurable.** Unmeasurable artifacts are *frozen*, not
  optimized.
- The same outcome signal that drives optimization also decides **rollback**
  (did the change improve subsequent outcomes?). Defining outcomes closes both
  problems at once. This is why outcome instrumentation is **Phase 0**.

### P3. Reversible and conservative before autonomous

The first downtime jobs are **read-only / report-only**. Mutating jobs come
only after read-only analysis has shown value, and they mutate through a
**stage-then-promote** path so that nothing is live until promoted and every
promoted change is reversible.

## Memory vs. skills: a fundamental asymmetry

The two halves of the system differ on the axis that matters most -- whether a
meaningful objective even exists:

| | Memory consolidation | Skill improvement |
|---|---|---|
| **Intrinsic objective?** | Weak but real: less redundancy, fewer contradictions, "what we kept is what's later recalled". | **None.** A skill exists only to cause an outcome; "better instructions" is undefined except relative to that outcome. |
| **Can progress without an external goal?** | Somewhat. | No -- it would be undirected mutation, i.e. drift. |
| **Optimization gate** | Can begin with intrinsic proxies. | **Blocked** until desired outcome is declared *and* measured. |

The practical consequence: **memory consolidation can start earlier; skill
improvement is gated on outcome measurement.** Treating them as one pipeline
(the original draft's mistake) produces mediocre memories and drifting skills.

## The optimization problem (desired outcomes)

This section is the crux. Without it, "self-improvement" is just *change*.

### Outcome signal sources, by reliability

1. **Verifiable post-conditions** -- code compiled, calendar event exists, file
   written, cron fired. Reliable but only available for *some* skills. **These
   skills are the first we can genuinely optimize.**
2. **Explicit user feedback** -- a correction, "no, do it this way," "thanks," a
   redo. Medium reliability; currently unstructured across channels, so it must
   be captured.
3. **Implicit behavioral signals** -- did the user re-ask, rephrase, or abandon?
   Did the agent need retries? Did `task_complete` fire cleanly? Cheap and
   abundant, but biased and noisy.
4. **LLM-as-judge against the skill's declared purpose** -- scalable, but it
   measures *plausibility*, not correctness, and can be gamed or drift. Use only
   as a filter, never as ground truth.

A practical system blends a cheap proxy (3) for volume with an occasional
ground-truth check (1/2) to keep the proxy honest.

### Skills must declare a definition of done

A skill becomes optimizable only if it says what success is. Proposed
`SKILL.md` frontmatter addition:

```toml
[outcome]
# Natural-language definition of done (required for auto-improvement eligibility)
success = "The requested calendar event exists and the user confirmed the details."
# Optional machine-checkable post-conditions, if the skill's effect is verifiable
checks = ["calendar.event_created", "user.confirmed"]
# Which signal class to trust for this skill: verifiable | explicit | implicit | judge
signal = "verifiable"
```

Skills with no `[outcome]` block are **frozen** -- they run, but the dream never
edits them.

### Skill improvement as offline learning from logged outcomes

Once outcomes are captured, skill optimization is principled rather than vibes:

1. Gather execution traces where the skill was used.
2. Partition by outcome signal (success / failure / ambiguous).
3. On the failures, propose an instruction change that would plausibly have
   produced success.
4. **Validate before promoting** -- counterfactually against held-out failed
   traces, or via forward A/B on subsequent uses. Promote only on demonstrated
   improvement; otherwise discard.

This is gated on **enough traces + a real outcome signal**, not on engine
readiness. Until then the skill is frozen.

## Architecture

### A deterministic pipeline, not an agent

Dreaming is a **deterministic batch orchestrator** that calls the model at
fixed synthesis/proposal points -- *not* an `AgentRunner` run where a model
freely decides what tools to call. Letting an autonomous agent drive memory and
skill mutation is more dangerous and less predictable than a fixed pipeline, and
it makes idempotency, resume, and testing far harder. (The original draft chose
the agent loop for plumbing reuse; that was the wrong trade.)

### The downtime worker

Off-cycle execution (P1) needs only modest pieces, most leaning on existing
plumbing:

- **Cheap idle detection** -- a per-agent `last_activity` timestamp bumped on
  inbound; the worker runs only after N minutes of quiet. *Not* a full
  preemption bus.
- **A work queue** -- session end *enqueues* a small job (an INSERT next to the
  existing `JobStore` in `crates/rustykrab-store/src/jobs.rs`); the work happens
  later.
- **An idle-gated background worker** -- drained on the existing `infra_handles`
  task set; yields the instant activity appears.

### Small jobs + abort-and-requeue (instead of pause/resume)

The unit of work is **small** -- one session, or one small batch. When live
work arrives, the worker does not suspend-and-resume a half-finished job with
persisted progress; it **aborts the current job and re-enqueues it**. This gives
immediate "get out of the way" behavior without a pause/resume state machine.
Pause/resume earns its keep only if jobs ever get long enough that discarding
in-flight work hurts -- deferred until proven necessary.

### Resource yielding is the real reason to step aside

The store is a single `Arc<Mutex<Connection>>`; even reads serialize through it,
so a dream loading many embeddings for clustering *will* block live traffic, WAL
notwithstanding. And a dream burning model tokens can rate-limit the user's
calls. So the dream **takes its own read-only connection** (WAL readers don't
block the writer), reads in small batches, and treats live activity as a signal
to **yield model budget and the connection**, not merely as a correctness
concern.

## Checkpoint / rollback (stage-then-promote)

Reversibility has two halves: the **mechanism** (below) and the **trigger**,
which is the outcome signal from [P2](#p2-you-cannot-improve-what-you-cannot-measure).

### Mechanism

A mutating dream computes its entire change-set against a **frozen
read-snapshot** and writes it to a **staging set**. Nothing touches live memory
until an atomic **promote**:

- **Checkpoint** is implicit -- the live set is untouched until promotion, so
  there is nothing to snapshot.
- **Abort / pause** is trivial -- discard or keep the unpromoted diff; live
  memory is never in a half-consolidated state.
- **Promote** applies the diff in one transaction (the `unchecked_transaction()`
  pattern already used by `batch_update_stages`), after a **staleness
  reconciliation** that re-verifies the snapshot's parents still exist and were
  not modified since.
- **Rollback (post-promote)** uses a manifest of what the cycle created/retired,
  built on existing soft-delete (`invalidate()` tombstones rather than deletes).

### Honest limits of rollback

Rollback is **clean only before anything depends on the cycle's outputs.** If
the live agent has since accessed, linked, corrected, or re-consolidated a
dream-produced memory, naive rollback resurrects stale parents and discards
accrued value. So rollback is offered within a **probation window** (before
first dependent access); beyond it, rollback is **best-effort and may surface
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
can carry "learned while dreaming, cycle N" -- enabling both audit and
selective rollback.

## Skills are proposal-only

Because skills act as system prompts and have no clean rollback story
(in-memory `RwLock` + disk files, no manifest, not version-controlled), the
dream **never hot-registers skills**. It writes proposals to a staging area;
promotion into `SkillRegistry` is gated behind review and/or the
existing-but-unused Ed25519 verification path
(`crates/rustykrab-skills/src/verify.rs`). This removes skill rollback from
scope and keeps a human in the loop for the changes most able to alter behavior.

**Open dependency:** proposals are useless without a **review surface**. If
dreams run unattended at 3am, proposals must be pushed somewhere a human sees
them (a digest to a channel, or a review prompt on next interaction). Without
that surface, the skill tier is theater and should be cut from scope honestly.

## Build order

Reordered so the prerequisite (outcomes) and the safe parts come first.

| Phase | What | Risk | Gate to proceed |
|---|---|---|---|
| **0 -- Instrument outcomes** | Extend `ExecutionTracer` to log tool/skill invocations linked to outcome signals; add `[outcome]` to `SKILL.md`. Pure data collection. | None | Outcome data is flowing for at least the verifiable-signal skills. |
| **1 -- Downtime read-only analysis** | Trigger + queue + idle-gated worker running *report-only* jobs: near-duplicate clusters, contradictions, per-skill success rates. Abort-and-requeue on activity. | None (no writes) | Reports show real, actionable patterns worth acting on. |
| **2 -- Memory mutation** | Consolidation that writes memory via stage-then-promote + manifest + probation-window rollback. | Medium | Consolidations measurably improve retrieval and are reliably reversible. |
| **3 -- Skill improvement** | Per-skill optimization from logged outcomes; proposal-only with a review surface. | Higher | Per-skill measurable outcomes + a working review/promotion surface exist. |

Notably **not** required, thanks to staging + soft-delete: a DB snapshot engine,
a job-state machine for pausing, conversation versioning, or a preemption bus.

## What downtime does and does not solve

- **Solves:** latency. The session-end cost is an INSERT; thinking happens when
  idle; live work always preempts.
- **Does not solve:** correctness. The moment a job *mutates* autonomously, you
  still need an outcome signal to know if it helped and to undo it if not. That
  is why Phase 0 (outcomes) precedes Phase 2 (mutation), and why early jobs are
  read-only.

## Interactions & risks

- **Dreaming vs. the decay/lifecycle manager.** Both decide "what matters" --
  decay demotes, the dream promotes episodic->semantic. Define precedence (e.g.
  dream promotions set a decay floor; recent explicit user signals win) or they
  oscillate.
- **Clustering quality.** Cosine >= 0.85 transitive closure can form giant
  clusters or merge textually-similar-but-distinct facts into a confidently
  wrong memory. Needs cluster-size caps and a low-confidence "do not merge"
  guard.
- **Proxy bias.** Implicit outcome signals are noisy; never let a single proxy
  drive irreversible change without a ground-truth cross-check.

## Open questions

- **Per-skill outcome bootstrapping.** Requiring a `[outcome]` block is clean for
  *new* skills; how do we backfill desired outcomes for skills that already
  exist, or for agent-authored ones?
- **Ground-truth coverage.** Verifiable post-conditions cover only some skills.
  For subjective skills (e.g. "summarize well"), is LLM-as-judge acceptable as a
  gated, audited signal, or do those skills stay frozen?
- **Review surface for proposals.** Which channel / UX surfaces skill (and
  risky memory) proposals for human approval?

## Key file references

| Concern | File |
|---|---|
| Agent event loop, `InboundEvent`, Cancel no-op (borrow conceptually) | `crates/rustykrab-agent/src/runner.rs` |
| Execution tracing (extend for outcome capture) | `crates/rustykrab-agent/src/runner.rs` |
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

- **v2 (this revision).** (1) Off-cycle from day one via trigger + queue +
  idle-gated worker -- never inline at session end. (2) Outcome measurement
  promoted to a first-class principle and **Phase 0**; skills gated on declared,
  measurable outcomes; the outcome signal doubles as the rollback trigger.
  (3) Deterministic pipeline instead of an agent loop. (4) Stage-then-promote
  instead of mutate-then-undo, with an explicit probation window and honest
  limits on rollback cleanliness. (5) Small jobs + abort-and-requeue instead of
  pause/resume. (6) Dream takes its own read-only connection; yielding is about
  freeing model/connection budget, not lock safety.
- **v1.** Original draft: dreaming as an `AgentRunner` run with a dream profile;
  step-machine mutating live memory with a manifest for undo; pause/resume via
  persisted progress; evaluation left as a single open question.

## Relationship to existing docs

See `MEMORY_ARCHITECTURE.md` for the memory subsystem this builds on, and
`crates/rustykrab-memory/DEFERRED.md` for previously-deferred consolidation work
that dreaming would finally drive.
