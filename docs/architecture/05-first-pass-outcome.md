# What the First Pass Changed

The first review ran against `d945495`. This records which of its findings
were acted on, which survived contact with the code, and which were wrong —
because a review that only reports its hits is not measurable.

## Acted on and closed

| Finding | Landed as | Effect |
|---|---|---|
| Two chat-map tables expressing one relation; stale binding bricked the chat | #588 | One `channel_bindings` table, FK cascade, legacy fold. Closed a live user-facing bug |
| In-memory cache kept the dead id after the durable fix | #589 | `load_or_rebind` heals; only `NotFound`, never a storage error |
| `PRAGMA foreign_keys = ON` with one declared FK | #590 | Eight declared; ownership cascades, provenance documented as deliberate |
| `memory_save` scoped to the process, not the conversation | #591 | Writes take the conversation from the ambient tool context |
| `MemoryBackend` in the consumer crate, forcing a pass-through adapter | #592 | Trait moved to `core`; 44-line adapter deleted |
| Five copies of `len / 3.5` | #593 | One `core::token_estimate`, with the inverse tested as a property |
| Agent loop existed twice, 82% identical | #593 | One `run_inner` parameterised by an event sink; −375 lines |
| Thinking off switch never sent `think: false` | #595 | Explicit polarity; `OLLAMA_THINK=false` now does something |
| Application layer inside the HTTP crate | #598 | `rustykrab-runtime`; no axum in its dependency tree |
| Poisoned locks turned recoverable faults into permanent outages | #603 | Every site recovers; closed #289 and #327 |
| `MemoryConfig::validate()` existed and nothing called it | #604 | Validated at construction; closed #313 and #326 |

## Corrected during implementation

Two findings did not survive checking, and both are recorded in place rather
than quietly dropped.

**"Six backend traits are in the wrong crate" → one.** Checking every
implementor: five are implemented in `cli`, `agent` or `tools` itself, all at
or above the tool crate, so none was blocked. Only `MemoryBackend` sat below
`tools` and could not implement its own contract. It was also the only one
that had produced a pass-through adapter — which is the actual tell, and a
better rule than the one first written.

**"Explicitly saved facts are invisible to scoped search" → reachable, but
never scoped.** `search` falls back to a global sweep when the scoped one
returns nothing, so the symptom was every scoped search silently widening.
Lower severity than claimed; same fix.

**"The `login_suite.rs` clippy warning will trip CI" → it will not.** `main`
carries `#[allow(clippy::too_many_arguments)]` with a justifying comment. The
warning came from uncommitted work in a shared checkout.

## Confirmed by events, not argument

The duplicated agent loop was the finding most open to "so what". While the
unification was in review, the usage-anchoring and tool-block-on-compaction
work landed — into **both** loop bodies, ~60 lines each. That is the
duplication cost being paid in real time, by someone who had to notice the
second copy existed. The merge then halved exactly the symbols that had been
written twice (`max_tokens_retries` 8→4, `compact_history(conv, tools)` 2→1)
while leaving the shared ones untouched, which is the check that the merge
preserved the work rather than resolving over it.

## Interactive continuity follow-up

Against base `0b565fd`, shared interactive setup and owned completion repair
failed-run history loss and queued-but-undispatched follow-ups. Telegram/Slack
journal admission UUIDs, serialize fallback turns and fence resets; HTTP/SSE
persist initial and partial-error histories. These are targeted repairs to the
six-copy turn-sequence finding, not a unified transaction or exactly-once queue.
Transport acknowledgement gaps, asynchronous memory durability, pending-input
recovery UX and concurrent HTTP turns remain unresolved. Compaction/provider
context changes are a separate dependent PR. See
[`turn-durability.md`](../evals/turn-durability.md).

The old runtime "no tests" finding was already stale at base `0b565fd`,
which had 14 distillation tests. This slice adds three lifecycle helper tests.
Direct turn-assembly coverage remains a separate gap; zero-test wording is retired.

## Context and compaction follow-up

The second split pins real user anchors, corrects generated-message roles,
rejects incomplete summaries and gives the runner sole authority to compact.
Ollama's schema-aware budget guard refuses overflow instead of silently
discarding messages. Explicit harness policies and the production-compactor
ablation evaluate alternatives without silently changing the Legacy default.
Captured context tests also found absent default-seeded tools in
`tools_load.active`; filtering the report and conditional memory guidance
repair that static-catalog contradiction. Dynamic schema invalidation,
task-switch readout failures and recall-after-restart coverage remain open.
See [study summary](../evals/compaction-study-validation.md).

## Method note

Three findings were wrong, and all three were caught by *implementing* them
rather than by re-reading. Static review is good at locating things and bad
at judging severity; the severity claims are the ones that needed the code
run at them. Worth remembering when reading
[`OPINION.md`](OPINION.md) — its confidence labels are the least reliable
part of it.

## Context and monitoring integration (2026-10-02)

The old WorkBackend stub description is retired: the controller implements
filing, planning and question answers, with recording/deferred adapters in
agent/CLI. QuestionReader now has a durable StoreQuestions implementation,
including answer-window selection. The runtime zero-test wording in the
reusability table was stale and is corrected; direct assembly coverage is
still a narrower open issue. Five interactive/task entry points still own
turn persistence; consolidation is not claimed by this integration.


Project-runtime continuity found against `24dc038`: the controller always pinned
repository HEAD, leaving prior verified native changes on unreachable-to-the-next
run work branches. Project-scoped rehydration now chooses their verified ancestry
chain, freezes context with the lease and refuses missing/divergent bases. Real
git/SQLite tests confirm code and corrected decisions survive a runtime switch,
fresh controller and archived task rows. Repository HEAD remains untouched.


## Work-manager review (2026-10-08)

Against v6 base `3e85f48`, the five-entry-point claim was an undercount:
`rustykrab-cli/src/task_queue.rs::process_task` still calls `save_turn`. The
current count is six, and centralizing new managed work does not remove the
legacy turn paths. A direct runtime `prepare_agent` test now covers the manager's
capability ceiling; this narrows rather than closes the prompt-assembly coverage
finding. Service observations use cached host probes and controller-owned work
receipts; no second scheduling or completion authority is added.

## Work execution slices (base 102cbbc, 2026-10-10)

The prior whole-project 128 KiB bound stopped small tasks once history accumulated.
Incident inspection found 58 tasks contributing 112,165 bytes of work history
against a 4,373-byte planning snapshot. Execution views now keep exact project
intent, the task's dependencies and pinned-base evidence; complete history remains
durable and is available to native workers through a private per-run file.
Workspace selection still checks every completed verified commit before projection.
The regression carries more than 128 KiB of historical reports through SQLite
while the next execution view is below 16 KiB.

Planner and worker instructions now support small sequential chunks and post-tasks.
The default anti-split warning is retired for ordinary plans; explicit legacy
warning/rejection policies remain available. Root-level report drafts previously
became separate filings, so temporary references to sibling drafts could not
resolve. They now form one atomic post-task graph. Gated-worker tests exercise
independent parallel work, ordered verified inputs, current-slice completion and
the follow-up group's final roll-up. Native process tests check selective context
rendering and full-history file inspection for both Claude and Codex.
