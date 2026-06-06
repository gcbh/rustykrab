# Dreaming: Off-Cycle Self-Improvement

This document describes a proposed **dreaming** facility for RustyKrab: an
off-cycle, low-activity self-improvement loop that reviews what has already
happened and reconciles it into durable knowledge (memory and, eventually,
skills).

Status: **design / proposal.** Nothing here is implemented yet. The intent is
to agree on the architecture, the safety boundaries, and the build deltas
before writing code. One question is deliberately left open (see
[Open question: evaluation](#open-question-evaluation)).

## Motivation

RustyKrab already has a strong substrate for learning but no loop that drives
it:

- Memory **capture is manual** -- the agent must call `memory_save`.
  Conversations are persisted (`ConversationStore`) and compaction overflow
  lands in `recall_archive`, but nothing flows from history into memory
  automatically.
- The memory data model is **consolidation-ready** -- `parent_memory_ids`,
  `consolidation_generation`, soft-delete (`is_valid` / `invalidated_by` /
  `invalidated_at`), and link types like `Consolidation` / `Contradicts` all
  exist (`crates/rustykrab-memory/src/types.rs`) -- but no pipeline uses them.
- Skills are **static** -- loaded from disk at startup; the agent *can* create
  one via `SkillsTool`, but nothing triggers that from experience.

"System learning" is not one job. It is three loops with different signals:

1. **In-the-moment capture (online).** Retain salient facts during/after a
   turn. Partially present via manual `memory_save` + regex extraction.
2. **Reconciliation / consolidation (offline).** Periodically review
   accumulated memories: merge near-duplicates, resolve contradictions into
   temporal narratives, promote episodic specifics into semantic
   generalizations. This is what "dreaming" primarily means here.
3. **Skill-ification (pattern -> procedure).** Detect *recurring* procedures
   across many sessions and crystallize them into a `SKILL.md`.

Dreaming moves loops 2 and 3 (and the housekeeping parts of 1) off the
critical path: they run when nobody is waiting, so they cost no online
latency.

> **Provenance note.** "Dreaming" is used loosely. The closest real prior art
> is *sleep-time compute* (reorganize memory before queries arrive),
> *generative replay* / memory consolidation (the neuroscience analogy), and
> *skill-library* construction (e.g. Voyager). We deliberately scope to
> **consolidation of things that actually happened**, not generative rehearsal
> of hypothetical scenarios -- the latter can manufacture confident falsehoods
> that then get stored as memory and later retrieved as if real, which is an
> unacceptable contamination risk for a security-first gateway.

## Core requirements

Two hard requirements shape the entire design:

1. **Checkpoint / rollback.** A dream's changes must be reversible if the
   "improvements" turn out not to be improvements.
2. **Pause mid-dream.** A dream must yield quickly when external work arrives,
   without losing the work it has already done.

These two requirements are not independent. Because a dream must *pause* and
let real user work interleave (Requirement 2), a coarse "snapshot the whole
database and restore it on rollback" strategy is **disqualified** -- restoring
a whole-DB snapshot would also discard the user messages that arrived during
the dream. The requirements therefore *jointly force* a fine-grained,
per-change rollback model. That is a convenient convergence, not a
coincidence.

## The unifying architecture

> A dream is a sequence of **small, idempotent, individually-committed steps**,
> each stamped with a `dream_cycle_id`, executed as an `AgentRunner` run under
> a dedicated **dream harness profile** that honors `InboundEvent`.

Everything else falls out of this single shape:

| Property | How the step-machine provides it |
|----------|----------------------------------|
| **Pause** | Stop at the next step boundary when Cancel / activity arrives. |
| **Resume** | Restart from the first uncommitted step (steps are idempotent). |
| **Rollback** | Undo by `dream_cycle_id` using existing soft-delete. |
| **Termination** | The step list is finite; convergence is "no steps left". |
| **Bounded yield latency** | Worst-case yield time = one step's duration. |
| **Crash safety** | Each step commits in its own transaction (WAL). |

The design leans on the fact that the agent runner is **already an
event-driven loop** (see below), so a dream is largely a new *profile* + a new
*trigger* + a *write-back trust gate*, not a new execution engine.

## Requirement 1: checkpoint / rollback

Rollback has two halves that are easy to conflate: the **mechanism** (being
able to revert) and the **evaluation** (knowing whether to). This section
covers the mechanism; evaluation is the [open question](#open-question-evaluation).

### What already exists

**Files:** `crates/rustykrab-memory/src/storage.rs`,
`crates/rustykrab-memory/src/types.rs`

- **Soft-delete with provenance.** `invalidate(id, invalidated_by)`
  (`storage.rs`) sets `is_valid = 0`, `lifecycle_stage = 'tombstone'`, and
  records `invalidated_by` + `invalidated_at`. Tombstoned memories are excluded
  from retrieval but **not destroyed**.
- **Consolidation lineage.** `Memory` already carries `parent_memory_ids` and
  `consolidation_generation`, so a synthesized memory can point back at the
  originals it replaced.
- **Per-step transactions are a proven pattern.** `batch_update_stages`
  (`storage.rs`) uses `conn.unchecked_transaction()`. The store is a single
  `Arc<Mutex<Connection>>` in WAL mode; DB locks are held only inside
  `with_conn` (via `spawn_blocking` + `blocking_lock`), **not** across LLM
  calls -- so a dream's brief write locks do not block an incoming user for
  the duration of a synthesis call.

### What does not exist

- **No snapshot / backup / `VACUUM INTO`.** The only checkpoint is
  `PRAGMA wal_checkpoint(TRUNCATE)` on shutdown (`crates/rustykrab-store/src/lib.rs`).
- **No conversation versioning.** Conversations are atomic JSON blobs
  (`crates/rustykrab-store/src/conversation.rs`); every `save()` overwrites.
- **No skill rollback.** Skills are an in-memory `RwLock`
  (`crates/rustykrab-skills/src/skill.rs`) plus disk files that are never
  auto-saved back, with no manifest and no version control.

### Proposed mechanism: the `dream_cycle_id` manifest

A dream **never hard-deletes**. For each cycle:

1. Allocate a `dream_cycle_id` (UUID).
2. Every memory the dream writes is stamped with that id; every memory it
   retires is `invalidate()`d (tombstoned) with `invalidated_by` pointing at
   the synthesized child, and the child's `parent_memory_ids` pointing back.
3. A **manifest** records every artifact touched by the cycle.

Rollback of cycle *N* then walks the manifest: un-tombstone the parents
(`is_valid = 1`, restore prior `lifecycle_stage`), and `invalidate()` the
children the cycle created. This is **selective and auditable**, and crucially
it leaves untouched any real user work that arrived mid-dream.

Proposed manifest table (sketch):

```sql
CREATE TABLE dream_cycles (
    id            TEXT PRIMARY KEY,   -- dream_cycle_id
    agent_id      TEXT NOT NULL,
    started_at    TEXT NOT NULL,
    finished_at   TEXT,               -- NULL while running / paused
    status        TEXT NOT NULL,      -- running | paused | committed | rolled_back
    summary       TEXT                -- human-readable digest of what changed
);

CREATE TABLE dream_changes (
    cycle_id      TEXT NOT NULL REFERENCES dream_cycles(id),
    step_index    INTEGER NOT NULL,   -- for resume + ordered rollback
    op            TEXT NOT NULL,      -- created | invalidated | promoted | ...
    target_kind   TEXT NOT NULL,      -- memory | link | skill_proposal
    target_id     TEXT NOT NULL,
    prev_state    TEXT,               -- JSON of prior values for reversible ops
    PRIMARY KEY (cycle_id, step_index, target_id)
);
```

Memory gains a nullable `dream_cycle_id` column (or we tag via the existing
`metadata` JSON to avoid a migration -- decision deferred to implementation).

### Skills: proposal-only (defers the hard part)

Because skills have no rollback story and act as system prompts, the dream
**does not hot-register skills**. It writes skill *proposals* to a staging
area; promotion into `SkillRegistry` is gated behind human approval and/or the
existing-but-unused Ed25519 verification path
(`crates/rustykrab-skills/src/verify.rs`). This:

- removes skill rollback from v1 entirely (you only ever roll back memory),
- dissolves the cross-store atomicity problem (SQLite memory + disk skills have
  no spanning transaction), and
- keeps a human in the loop for the changes most able to alter agent behavior.

## Requirement 2: pause / preemption

### What already exists (more than expected)

**File:** `crates/rustykrab-agent/src/runner.rs`

The runner is **already event-driven**. `AgentRunner::start()` spawns the run
and returns an `AgentHandle` over an `mpsc` channel, with an `InboundEvent`
enum that already includes both signals a dream needs:

```rust
pub enum InboundEvent {
    UserMessage { parts: Vec<ContentPart>, channel: Option<String>, channel_msg_id: Option<String> },
    Cancel,
}
```

`AgentHandle::cancel()` sends `InboundEvent::Cancel`; an `AtomicBool alive`
flag tracks liveness. Tool execution is already wrapped in `tokio::select!`
(for heartbeats). **The plumbing for preemption exists end-to-end.**

### The gap

The last wire is not connected:

```rust
// drain_inbound_to_conv, runner.rs
InboundEvent::Cancel => {}  // <- received and ignored
```

And inbound is drained only *before* and *after* `run_streaming`, not
*during* iterations. So today a Cancel is accepted and silently dropped.

Three deltas make preemption real, in increasing difficulty:

1. **Honor Cancel + check inbound per iteration.** At the top of each loop
   iteration, check the channel; on Cancel (or an activity signal), stop after
   the current step. *Small.*
2. **Step granularity.** Tools run to completion -- the `select!` only services
   heartbeats; **no cancellation token is passed into a tool**. So a dream can
   be interrupted *between* steps but not *inside* one. This is acceptable only
   if steps are small (one cluster, one consolidation). **Worst-case yield
   latency = one LLM synthesis call.** Keep synthesis steps short, and never
   hold a DB write lock across an LLM call (the current `with_conn` design
   already avoids this).
3. **Activity bus.** Nothing today tells a dream "a user just arrived" --
   channels each own a separate `mpsc` receiver with no unified signal. Build a
   small broadcast `ActivityBus` that channel loops ping on inbound and that
   the dream's `select!` watches. This is also exactly what idle-detection
   needs, so it is shared infrastructure, not pure overhead.

### Pause vs. abort, and resumability

The requirement is **pause**, not abort, so a paused dream must record enough
to resume. Because steps are idempotent and individually committed, "resume"
is just "find the first `step_index` not present in `dream_changes` for this
cycle and continue". The `dream_cycles.status` column carries
`running | paused` so a resumed daemon knows what to pick up.

## Tiers

Ship in increasing order of risk so the safe parts land first and de-risk the
plumbing:

| Tier | What | Risk | Notes |
|------|------|------|-------|
| **1 -- Maintenance** | Lifecycle sweep, near-duplicate linking, embedding-drift check, tombstoning | None (no LLM) | Pure housekeeping of code that already exists but is never scheduled. Validates the scheduler / idle-detection / step-machine plumbing first. |
| **2 -- Consolidation** | Replay `recall_archive` + recent episodics; synthesize near-duplicate clusters; resolve contradictions; promote episodic -> semantic | Medium (LLM writes memory) | All writes carry `dream_cycle_id` provenance and are rollback-able. |
| **3 -- Skill-ification** | Detect recurring procedures across sessions; draft `SKILL.md` | Higher | **Proposal-only.** Routed through approval / verification gate; never auto-registered. |

## Trigger model

- **Idle vs. scheduled.** Start with a quiet-hours cron (you already have
  `JobStore` + a cron tool) -- ~80% of the value for far less complexity.
  True per-agent idle-detection (last-activity timestamp + cooldown so a dream
  does not fire and immediately get interrupted) can layer on top once the
  `ActivityBus` exists.
- **Per-agent, not global.** "Little activity" is scoped per `agent_id`; the
  dream operates on one agent's memory at a time.
- **Preemptible.** The same `ActivityBus` signal both *gates* a dream from
  starting and *pauses* one in flight.

## Cost governance & safety

- **Hard budget.** Token / time / iteration caps. `HarnessProfile`
  (`crates/rustykrab-agent/src/harness.rs`) already models iteration limits;
  the dream profile sets tight ones.
- **Convergence / idempotency.** A dream must detect "nothing meaningful to
  consolidate" and stop; re-dreaming the same archive must not keep generating
  near-duplicate "insights".
- **Auditability.** Add a `Dream` variant to `ImportanceSource`
  (today: `Heuristic | Llm | User`) so retrieved memories carry "I learned this
  while dreaming, not directly from you", and so a bad cycle can be found and
  rolled back.
- **Trust boundary.** Autonomous, unattended writes to memory are acceptable in
  Tier 2 *because* they are reversible and provenance-tagged. Skills
  (system-prompt-level influence) are **never** autonomous -- proposal-only.

## What is net-new to build

Honest deltas, grounded in the current code:

1. **Honor `InboundEvent::Cancel`** + per-iteration inbound check
   (`runner.rs`). *Small.*
2. **`ActivityBus`** broadcast for idle-detection + preemption. *Small/medium;
   needed anyway.*
3. **`dream_cycles` / `dream_changes` manifest** + rollback routine over the
   existing soft-delete API. *Medium.*
4. **Resumable progress** (first uncommitted `step_index`). *Small, given
   idempotent steps.*
5. **Dream `HarnessProfile`** + dream orchestrator (trigger, step loop).
   *Medium.*
6. **Skill staging area** (proposal-only). *Small; defers skill rollback.*

Notably **not** required, thanks to the soft-delete model: a DB snapshot
engine, a job state machine, or conversation versioning.

## Open question: evaluation

The rollback *mechanism* is well-defined above. What remains undecided is the
**evaluation** half of Requirement 1: how does the system decide a dream's
changes were *not* improvements, i.e. when to press the rollback button?

Options:

- **Manual review.** The dream emits a diff / digest (`dream_cycles.summary`);
  a human approves or rolls back. Simplest and safest; slowest.
- **Automatic guardrails.** Invariants auto-trigger rollback -- e.g. no net
  memory loss beyond a threshold, or retrieval quality on a held-out query set
  does not regress.
- **Both.** Guardrails catch the obvious; a human reviews the rest.

This choice determines whether the dream needs an **eval harness** (held-out
queries, quality metrics) or merely a **digest emitter**. It is the last thing
to pin down before implementation begins.

## Key file references

| Concern | File |
|---------|------|
| Agent event loop, `InboundEvent`, `AgentHandle`, Cancel no-op | `crates/rustykrab-agent/src/runner.rs` |
| Harness profiles (budget caps) | `crates/rustykrab-agent/src/harness.rs` |
| Memory soft-delete, transactions, `with_conn` | `crates/rustykrab-memory/src/storage.rs` |
| Memory model (provenance, consolidation fields) | `crates/rustykrab-memory/src/types.rs` |
| Lifecycle sweep / near-duplicate detection | `crates/rustykrab-memory/src/lifecycle.rs` |
| Compaction overflow store (`recall_archive`) | `crates/rustykrab-store/src/recall_archive.rs` |
| Store connection / WAL / shutdown checkpoint | `crates/rustykrab-store/src/lib.rs` |
| Cron / scheduled jobs | `crates/rustykrab-store/src/jobs.rs` |
| Skill registry, disk loading, verification | `crates/rustykrab-skills/src/` |
| Orchestration (where a post-cycle hook would attach) | `crates/rustykrab-gateway/src/orchestrate.rs` |

## Relationship to existing docs

See `MEMORY_ARCHITECTURE.md` for the memory subsystem this design builds on,
and `crates/rustykrab-memory/DEFERRED.md` for previously-deferred consolidation
work that dreaming would finally drive.
