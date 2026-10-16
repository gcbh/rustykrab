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


## Work-preference follow-up (base 1784ce3)

The standing-judgment fold previously applied every unrevoked grant globally,
although judgment_policies stored a scope. The new unsupported-scope regression
shows a project-only grant contributes no global resource authority; non-global
freeform intake is now refused. Reviewed work templates use a separately matched
scope with exact task/tool/resource/project/budget checks, human provenance and
revocation. Accepted proposal code now inherits the reviewed execution budget
and required tools/MCP, trigger and preconditions. Controller tests demonstrate
one-time approvals, changed scopes, daily limits and revocation remain held while
matching automatic proposals use an audited policy actor. These tests establish
permission behavior, not the usefulness of future self-generated work.

## Project-handoff recovery (base f1affb8)

An October 13 read-only audit found 34 open legacy whole-history size questions
and 10 open branch-divergence questions in the active manager store. The size
projection fix had shipped, but the sweep never revalidated its old holds.
One verified documentation commit on an independent branch prevented all later
project coding work from starting; answering continue repeated the question.

The workspace helper now retains a conflict-free integration of the verified
parents without touching repository HEAD or the checkout. The sweep rechecks
controller-owned technical handoffs every 30 seconds and obsoletes resolved
questions without inventing user answers. Real-Git/SQLite regression tests cover
history preservation and replay, legacy-hold recovery, retained oversized current
requirements and independent user decisions. Actual conflicts and missing commits
remain unresolved. No table, execution backend or permission grant is added.


## Failure prerequisites and inherited urgency (2026-10-13)

Against local handoff-recovery base `c4c7d27`, an order-3 internal item was
background work: it could be reused but the failing original immediately
continued the ladder. Ready selection used only each item's stored priority.
The follow-up creates or reuses the internal prerequisite and validates its
blocking edge atomically; the original waits until completion and resumes with
the prerequisite's evidence. Unknown failures first request diagnosis, while
known recurring failures request a cause repair.

Priority is derived transitively from interested open dependents and parent
gates, with distinct waiter count breaking ties. Stored priorities are unchanged;
closing or disabling a waiter removes its donation. Real-store controller
regressions cover creation, reuse, concurrent failures, restart persistence,
self-dependency rejection and scheduling ahead of unrelated work. Pure graph
regressions cover transitive urgency, distinct diamond fan-in, parent gates,
inactive donors and corrupt-cycle termination. This is local regression
evidence; it does not claim a production deployment.

The review also corrected older prose that claimed an internal repair receives
a controller-created code worktree. `plan_workspace` currently applies only to
`WorkKind::Code`; internal work inherits repository scope and constraints. The
new prerequisite behavior does not change that existing workspace boundary.

Hosted E2E validation caught a missing stand-in response for recurring timeout
repair work. Scenario 32 consequently waited on the new prerequisite instead
of reaching its required failed sibling. The fixture now completes the
simulated prerequisite while retaining the original failure trigger; the
restart, cascade, lease and single-notification assertions remain required.

A raw-timeout fixture trigger also matched notification text outside a worker
run, replacing the expected human message. The final trigger matches the
generated recurring-repair objective; the isolated restart/cascade/notification
scenario passes with its original assertions.


## General agents and blocker-first scheduling (base 65e335b)

The October 2 review recorded an explicit planning-only role as a requirement
separate from tool availability. That restriction is now retired at the owner's
request. Every agent can plan and execute within its actual tool and resource
grants, regardless of legacy role/category metadata. Native subscription
resources share the host-tool path as well as repository execution. This closes
a routing restriction; it does not establish that every model will succeed on
every objective. Distinct transitive waiter count now precedes explicit urgency
in selection. The same derived ranks reach the monitor without rewriting work
priorities or introducing another ledger.
## Work-status pagination (base 7d65c3c, 2026-10-13)

A read-only audit of one production conversation found status requests with
21, 21, 22 and 40 explicit IDs rejected by the tool's 20-ID parser limit.
Even accepted 20-ID calls returned at most 15 status lines, leaving the model
to reconstruct the unread portion from a `more` count. This was a separate
source of extra turns from the conversation's failed model summarizations.

The tool now accepts a full ID list, normalizes and deduplicates it in input
order, and sends only the requested page to the existing status backend.
`offset`, bounded `limit` and `next_offset` make continuation explicit for both
ID lists and subtree reads. Missing/inaccessible IDs consume their requested
position, so an empty page still advances and unchecked IDs are never labeled
missing. Responses retain the 15-line bound; there is no new state, table,
backend method or authority. Root pages remain live reads rather than snapshots.

Regressions replay the observed batch sizes and the accepted-20/returned-15
mismatch, empty/missing pages, normalization, terminal offsets and malformed
IDs outside the first page. The registered tool is also exercised over the
real controller/SQLite store for a 40-item list plus a missing boundary ID,
and for a multi-page subtree. This establishes interface continuity; it does
not repair the independent empty/truncated-summary model failures or claim
that a model will always choose to read the next page.

## Scheduling barriers in the live queue (base 5bc3309)

A read-only October 14 audit found one repeatedly dispatched planner item,
17 dependency-waiting leaves, one queued parent and a ready audit-file item
matching no worker's declared scope, while Codex and Fieldkit Claude were
healthy. Every repository writer also contended on one repository-wide lock,
even though the execution adapters supplied isolated worktrees.

The repair permits opted-in execution in independent first-repository worktrees,
preserves shared external locks and explicit ordering, and adds exact operator
file grants through the existing native host-tool path. It also rechecks durable
controller improve-rung waits on stopped or budget-exhausted prerequisites; the
same audit identified four such legacy waits. It does not remove ordinary
blocked dependencies or widen approval authority.

Regression evidence includes three overlapping Local/Codex/Claude stand-in runs
creating real distinct worktrees and verified independent commits, subsequent
clean integration without moving repository HEAD, shared-resource exclusion,
non-isolating executor serialization, exact-file creation/read/edit with sibling
and symlink refusal, and restart/idempotent recovery preserving an ordinary
ordering edge on the same prerequisite. These tests establish scheduler and
scope behavior, not model reasoning quality or live deployment.

A completed capability can wake a parked item only when at least one prerequisite
landed after its latest hold. If execution subsequently reports the same missing
capability, the old completion is not new evidence and cannot restart it forever.
A live audit observed the same planner item repeatedly surfacing a missing
controller-dispatch tool and being awakened by an already completed capability.
The regression checks old, newly landed and subsequently stale completions.

## One window per local model (2026-10-15)

A local model's context window was set in up to six places that had to
agree: `RUSTYKRAB_NUM_CTX` or `OLLAMA_NUM_CTX` (the Ollama `num_ctx`),
`RUSTYKRAB_MAX_CONTEXT_TOKENS` or `harness.toml` (the harness budget),
`RUSTYKRAB_COMPACTION_CONTEXT_CEILING` (the compaction ceiling), and
`context_tokens` in the local-resources JSON for the resource slots. The live
daemon ran a 32K `num_ctx` under a 96,000-token budget, and the e2e harness
carried its own copy of the arithmetic. Workers built their harness profile
from the definition's preset and ignored the operator's compaction strategy.

`<data>/models.toml` (`rustykrab-agent/src/local_model.rs`) is now the one
place: a `[default]` table and one entry per model tag, each naming only what
differs. `context_window` sizes `num_ctx`, the budget and the ceiling
together; the entry also carries output tokens, keep-alive, sampling, thinking,
compaction strategy, threshold and summary window, and late tool binding. The
daemon validates it at startup and refuses to start on a bad entry, the chat
runner, `SubagentRunner` and `LocalWorker` all apply it, `rustykrab models`
prints what each model resolves to, and the retired settings produce a startup
warning instead of silently competing. This narrows OPINION §2 for providers
(8 literal env reads to 3) without closing it. Hosted models keep
`RUSTYKRAB_MAX_CONTEXT_TOKENS`.

## Compaction state handoff (2026-10-15)

The default compaction summarizer was handed the agent's own system prompt
inside the history it summarized ("Complete the user's task… Keep going…").
After #709 turned thinking off for summary calls, qwen3.8 at a 128K window
answered the task instead of summarizing it in 8 of 16 summary calls, and the
handoff then recorded the answer's guesses as fact ("Agent lacks browser
automation capabilities"). The next action stayed on task in 2 of 6 trials.

The summarizer now reads the history without the agent's system messages,
under its own system message, with the instructions after a fenced history,
and the default prompt asks for named state sections. A hill-climb over five
candidates chose the sections; the evidence and the audited scoring are in
`docs/verification/compaction-state-handoff-2026-10-15.md`. The retention is
unchanged: first and latest user input verbatim.

