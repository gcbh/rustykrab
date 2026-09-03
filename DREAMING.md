# Dreaming: The Self-Improvement Outer Loop

This document describes a proposed **outer loop** for RustyKrab: a longer-running,
off-cycle process -- "dreaming" -- that continuously improves how the system
executes by reviewing what has already happened and reconciling it into durable
knowledge (memory and, eventually, skills).

Status: **Phase 0 implemented; Phases 1-3 proposed.** Outcome instrumentation
and credit assignment are in the tree, opt-in behind
`RUSTYKRAB_OUTCOME_CAPTURE` and observational only -- see
[Phase 0 as built](#phase-0-as-built). Everything downstream of that is still
design. The [Revision history](#revision-history) records how the design
evolved so the discarded reasoning survives.

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

## Prior art: ACE (Agentic Context Engineering)

The closest published system is **ACE** -- "Agentic Context Engineering:
Evolving Contexts for Self-Improving Language Models" (arXiv 2510.04618,
Stanford/SambaNova, ICLR 2026; camera-ready retitled "Learning Comprehensive
Contexts for Self-Improving Language Models"; open-source at
`github.com/ace-agent/ace`). ACE self-improves by evolving *contexts* rather
than weights: context is a **playbook** of itemized bullets
(`[id] helpful=X harmful=Y :: content` -- stable identifier + outcome
counters), adapted by three roles: a **Generator** (produces reasoning
trajectories), a **Reflector** (distills concrete insights from successes and
errors), and a **Curator** (an LLM step emitting compact, itemized **delta
entries**), with the deltas merged into the playbook by **deterministic,
non-LLM logic** -- new IDs append, existing bullets update in place (counters
increment), embedding-based dedup prunes (cosine >= 0.9 in the reference
implementation). Verified headline results: +10.6% on agent benchmarks
(AppWorld), +8.6% on finance reasoning (FiNER/XBRL), −86.9% average adaptation
latency vs. baselines like GEPA and Dynamic Cheatsheet.

### Mapping to this design

| ACE | This design |
|---|---|
| Offline context optimization (system prompts) | Skill improvement (skills are system prompts) |
| Online adaptation (agent memory) | Memory consolidation |
| Generator trajectories | Inner-loop traces (**Monitor**) |
| Reflector | Phase 1 read-only analysis (**Analyze**) |
| Curator delta entries | Staged proposals (**Plan**) |
| Deterministic non-LLM merge | Promote step (**Execute**) |
| helpful/harmful counters | `proof_count` + outcome attribution |
| Grow-and-refine dedup (>= 0.9) | `detect_near_duplicates` (>= 0.85 link / >= 0.95 invalidate) |

### Empirical validation of two choices made here on first principles

- **Context collapse proves the stability argument.** With monolithic LLM
  rewriting, ACE's AppWorld case study shows an accumulated context going from
  18,282 tokens / 66.7 accuracy at step 60 to **122 tokens / 57.1 accuracy at
  step 61** -- worse than no adaptation (63.7). "Brevity bias" (summaries
  dropping domain insights) is the companion failure. This is published
  evidence for our stage-then-promote / small-deltas / **no monolithic
  rewrites** rules, and for rejecting naive summarize-and-replace as the
  consolidation mechanism.
- **Feedback-quality dependence proves P2.** ACE adapts label-free from
  execution feedback (+14.8% over ReAct baseline without ground-truth labels),
  but the paper states that without reliable feedback both ACE and similar
  methods "may degrade" and the context "can be polluted by spurious or
  misleading signals." That is this document's "you cannot improve what you
  cannot measure," independently discovered and measured. Caution for us: ACE's
  gains come from benchmarks with *clear* execution feedback; a chat gateway's
  feedback is murkier, so the outcome-signal hierarchy and freeze rule are
  load-bearing.

### What ACE lacks that this design adds

ACE has **no rollback, no staging, no probation window, no human gate, and no
idle scheduling** -- playbooks update in place during adaptation runs, which is
fine for benchmark runs and unacceptable for a persistent, security-first,
multi-channel production gateway. It also carries the whole playbook in-context
(leaning on long-context models), where we retrieve selectively -- our memory
system already solves the growth problem that ACE's lazy dedup only patches.
The two systems compose: ACE supplies the update algebra; this design supplies
governance, persistence, scheduling, and retrieval.

### Amendments adopted from ACE

1. **Delta algebra as the promote step.** Unit of change = itemized entry with
   a stable ID + helpful/harmful counters; the Reflector-analog emits delta
   entries; **promote = deterministic merge** (append new IDs, increment
   counters in place, dedup). The staging area is *where* deltas wait; ACE's
   merge is *how* promote applies them.
2. **Skill bodies restructured for delta-updatability.** Split `SKILL.md` into
   human-authored prose (immutable by the loop) plus a loop-managed
   `## Learned strategies` itemized section. Enables a **graduated gate**:
   counter increments on existing bullets auto-promote; new or edited bullet
   *text* still requires review -- softening the human bottleneck without
   giving up the security stance.
3. **`harmful_count` alongside `proof_count`**, with Phase 0 attributing
   per-trace outcomes to the specific memories/bullets retrieved -- the
   credit-assignment mechanism the control-loop section requires.
4. **"No monolithic rewrites" is a hard rule**, citable to the collapse case
   study.
5. **Mine `github.com/ace-agent/ace`** for Reflector/Curator prompts and
   thresholds; port concepts, not code.

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
# Machine-checkable post-conditions. Each names a probe the deployment has
# registered; a check nothing can observe makes the whole block advisory.
checks = ["calendar_event_booked", "confirmation_sent"]
# Signal class to trust: verifiable | explicit | implicit | judge
signal = "verifiable"
```

Skills with no `[outcome]` block are **frozen** -- they run, but the loop never
edits them.

A check names a **post-condition probe**: something that goes and looks at
the world, independently of what the run reported doing. The probe is
sampled twice, before the run and after, and the check passes only when the
effect **appeared across that window**.

The cheap alternative -- treat a check as satisfied when the run called a
tool of that name and it returned `Ok` -- was tried and rejected, and the
reason is the crux of this whole document. That question is about the
agent's behaviour, not about the world: a run that created the event on the
wrong day, for the wrong person, in the wrong calendar satisfies it exactly
as well as a correct one. And the answer does not stay local. It becomes
`Verifiable`, which is what `is_ground_truth()` admits, which is what
`Readiness::Ready` requires, which is what permits the loop to mutate
memory. A proxy the agent controls would end up as the sole evidence
authorizing self-modification -- the loop grading its own homework, and
precisely the Goodhart failure [P2](#p2-you-cannot-improve-what-you-cannot-measure)
exists to prevent.

Sampling twice matters for the same reason. "The calendar contains the
event" is a claim about the calendar; "the calendar gained the event during
this turn" is a claim about the turn. Only the second is a post-condition,
and without it every subsequent run would be credited for work done once.

Five properties are load-bearing, and each has a test that fails without it:

- **Only `signal = "verifiable"` buys ground truth.** Checks alone do not.
  A skill asking to be judged by a model must not have its runs promoted to
  fact, or the loop can launder its own opinion into evidence.
- **A check no probe can observe yields no contract at all**, and the run
  falls back to the implicit signal. Not satisfied (invention), not unmet
  (a typo in a `SKILL.md` would score a working skill as doing nothing,
  forever). This is also what happens on a deployment that has registered
  no probe for an effect some other deployment can see.
- **An effect that predates the run is not credited to it.**
- **An outstanding check is `Ambiguous`, not `Failure`.** A skill's effects
  may legitimately span several turns, and from one run there is no way to
  tell "did not do it" from "has not done it yet". Ambiguous records are
  kept but excluded from success rates, so a working multi-turn skill is
  never scored as harmful for being mid-conversation.
- **Producing every effect and then erroring is not success.** Otherwise a
  skill that reliably crashes after its side effects accumulates a clean
  record.

A probe that *errors* -- a calendar server briefly down -- is not an unmet
check. It yields no verdict at all, because a server outage is not evidence
about the skill.

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
| **0 -- Instrument outcomes (Monitor)** | *(implemented)* Outcome records + credit assignment; `[outcome]` in `SKILL.md`. Pure data collection, opt-in via `RUSTYKRAB_OUTCOME_CAPTURE`. | None | Outcome data flowing for at least verifiable-signal skills. |
| **1 -- Downtime read-only analysis (Analyze)** | Trigger + queue + idle-gated worker running *report-only* jobs; abort-and-requeue on activity. | None (no writes) | Reports show real, actionable patterns. |
| **2 -- Memory mutation (Plan+Execute)** | Consolidation that writes memory via stage-then-promote + manifest + probation-window rollback; low gain, rate-limited. | Medium | Consolidations measurably improve retrieval and are reliably reversible. |
| **3 -- Skill improvement** | Per-skill optimization from logged outcomes; proposal-only with a review surface. | Higher | Per-skill measurable outcomes + a working review/promotion surface. |

Notably **not** required, thanks to staging + soft-delete: a DB snapshot engine,
a job-state machine for pausing, conversation versioning, or a preemption bus.

### Evals

Each phase's targets are written down as evals before its code meets them
(`rustykrab_dream::eval`): an eval expected to fail keeps the suite green and
names what is missing; the day it passes, the suite turns red until it is
promoted. `crates/rustykrab-dream/tests/evals.rs` and
`crates/rustykrab-cli/tests/outcome_evals.rs` hold them, the e2e suite carries
the daemon-level target as an `xfail` scenario, and
`.github/workflows/dream-evals.yml` runs everything nightly at a seed range a
pull request cannot afford. The report on that run is the loop's own
scorecard: what it proves today, and what it still owes.

## Phase 0 as built

Shipped and opt-in behind `RUSTYKRAB_OUTCOME_CAPTURE` (off by default).

- **`rustykrab-core::outcome`** — the shared vocabulary: `SignalClass`
  (`Verifiable` > `Explicit` > `Implicit` > `Judge`, with `is_ground_truth()`
  gating what may justify a costly change), `OutcomeVerdict`,
  `Attribution`/`AttributionKind`, `ExecutionCounters`, `OutcomeRecord`,
  `OutcomeTally`, and the `OutcomeSink` trait.
- **`rustykrab-core::retrieval_log`** — the missing join. The retrieval path
  knew *what was recalled* and the run-completion path knew *how the turn
  went*, with nothing connecting them. `RetrievalLog` connects them: the
  memory backend records the ids it actually hands to the model, and the
  runner drains them when the run ends.
- **`rustykrab-store::outcomes`** — `outcome_records` + `outcome_attributions`,
  written once and never updated, pruned at 50k rows. Tallies are derived by
  `GROUP BY` on read, with a `ground_truth_only` filter so proxy signals
  cannot launder a verifiable failure.
- **`SKILL.md` `[outcome]` block** — `success`, optional `checks`, and
  `signal`. `SkillMd::is_optimizable()` implements the freeze rule; an
  unset or unparseable `signal` degrades to `Implicit` rather than
  something stronger, so an unclear declaration buys no authority.
- **`OutcomeContract` + `post_condition` + `probes`** — a skill's
  `[outcome]` block becomes a checkable claim only when it declared
  `signal = "verifiable"`, named at least one check, *and* every one of
  those checks resolves to a probe this deployment has registered.
  Anything else yields no contract and the run falls back to the implicit
  signal. The contract is derived in `rustykrab-runtime`'s `prepare_agent`
  from whichever skill the turn already resolved, so it applies to every
  path — conversation, cron, peer-delegated — rather than only to the one
  that happened to be wired. Which effects a skill *claims* comes from its
  `SKILL.md`; which effects this machine can *observe* comes from
  `build_probe_registry` in the CLI.
- **`AgentRunner::capture_outcome`** — the per-run tracer was hoisted from
  `run_inner` to the `run`/`run_streaming` wrappers so capture sees the run's
  traces regardless of which of the inner loop's ~10 exit paths fired.
  Best-effort by construction: a failing sink is logged and swallowed.

Two deliberate deviations from the plan above:

1. **`harmful_count` on `Memory` is deferred to Phase 2.** `Memory` has no
   constructor, so adding a field touches every struct literal plus four
   hand-numbered placeholder lists in `upsert_memory`, and
   `SqliteMemoryStorage` has no additive-migration phase to extend. Since
   tallies are *derived* from records rather than incremented in place, the
   column buys nothing until something mutates memory — which is Phase 2.
2. **Explicit corrections are still uncaptured.** A skill's declared
   `checks` are now verified (see below), so runs it drives carry
   `Verifiable` evidence; nothing yet notices a user *saying* the run was
   wrong, so `Explicit` remains unused.

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

- **v4 (this revision).** Added the ACE prior-art analysis (arXiv 2510.04618,
  ICLR 2026) after verifying its mechanism and results against the paper text
  and the authors' open-source implementation. ACE independently validates two
  first-principles choices here (context collapse -> no monolithic rewrites /
  stage-then-promote; feedback-quality dependence -> P2). Adopted amendments:
  itemized delta algebra with helpful/harmful counters as the promote step,
  loop-managed `## Learned strategies` sections in `SKILL.md` with a graduated
  gate, `harmful_count` + per-trace credit assignment in Phase 0.
- **v3.** Reframed around the **outer loop**: an explicit
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
