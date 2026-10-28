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


## Answer-endpoint lock contention (2026-10-21)

On live v6.6.3/b5c930f, the UI's 12-second request limit was shorter than
64–69-second scheduling passes. A confirmed nonexistent question read in 5 ms;
a no-op answer returned 404 after 24.831 seconds, 34 ms after a tick completed.
Both scheduling and answer application held the same command lock. The live
record had 34 eligible handoff rechecks and 269 stored contexts (65,854,090 JSON
bytes); repeated reconstruction was a strong suspected cost, not a separately
timed production attribution.

The follow-up saves durable idempotent answer receipts before that lock,
completes their effects atomically on the controller, and renders the sent
message's receipt/application state. Handoff history is prepared outside the
lock, cached per project within one preparation and guarded against stale work/
project state before technical recovery. The original-lock negative control
fails the prompt-intake regression; restored intake passes and the next actual
scripted worker receives the decision. These are local regression observations,
not a claim of deployed improvement or general chat-turn durability.

## Full manager tool discovery (2026-10-25)

At base `74d3e9e` (v6.6.8), manager mode deliberately restricted tool grants to
coordination and memory, including during dynamic discovery. Its preparation
test proved direct execution tools were denied. This matched the earlier
coordination-only design, but prevented the manager from directly using the
email/account tools needed for routine requests. A recent Cometeer request
repeated denied credential and exec calls without attempting discovery; the
restriction was verified, while historical-context influence on the model's
choice remains an inference.

The owner directed a full manager toolset. Manager mode now adds coordination
guidance without the separate allowlist, retaining the ordinary available
catalog and progressive discovery. Explicit run denies, availability and
computer/subagent opt-ins remain enforced. The prompt allows direct routine
work and preserves durable controller assignments and tracked schedules/services.

The new positive tests failed on the old policy: discovery could not find email
and preparation omitted direct tools. With the restriction removed, actual
runner/discovery tests can call an inert email tool after stale credential-error
history, in append and rerender modes. A negative test confirms explicit denial
survives search and exact-name loading. This closes that access regression;
it does not prove a live model will select the right tool, that the change is
deployed, or that the user's real subscription has been checked. See the
[verification record](../verification/manager-tool-discovery-2026-10-25.md).


## Capability recovery follow-up

Against deployed base `de37684` (v6.6.9), item `39bcaa42` required
`capability:tool:work_resources` before inference, and the general-worker filter
excluded the actual `work_resources` implementation. A late SKILL.md bearing
the reference's name made the catalog look populated without supplying that
operation. An owner's instruction to build a repair was present in the brief
but could not reach a model past preflight. These are controller/composition
faults; changing the worker's prompt cannot repair that dispatch boundary.

The correction decodes the explicit tool-reference namespace, makes the existing
read-only inventory callable by general workers, checks acquired tools in the
actual dispatch matcher, and routes/surfaces ceiling mismatches before creating
more acquisition work. Legacy nested refs now require the existing functional
probe rather than falling through as availability only. A real-adapter scripted
fixture demonstrates proof followed by original work resumption; no live owner
hold, budget, repository scope, merge or deployment is changed by this patch.
The generic ability to repair a broken controller without an external bootstrap,
missing preflight transcripts and the automatic-start UI/API eligibility error
remain separate gaps. [Evidence and limits](../verification/capability-recovery-2026-10-27.md).


### Startup composition counterexample

The pre-deployment full-daemon check falsified the constructor-level inventory
claim above on v6.6.10 source `e2eee4b`: the filter was fixed, but main registered
ResourcesTool after worker construction. The original now received one bounded
ceiling-mismatch question instead of the old phantom-tool loop; inventory still
could not execute. v6.6.10 deployment was held. The earlier unit fixture supplied
the tool directly and therefore could not test production startup order.

The follow-up moves read-only inventory registration before every worker tool
snapshot, retaining the existing manager/services condition and shared registry;
controller-dependent service actions remain late. A mandatory full-daemon CLI
integration regression fails on the unchanged release source and checks the
actual API → planner → acquisition → probe → original inventory path after the
move, using an inert protocol server. This is composition evidence in addition
to adapter proof, not evidence of the owner's live goal being complete.


## Shared Git handoff recovery

Against `4d76cc5`, the controller prepared a future brief with an owner's answer,
then failed automatic integration before leasing its worker. On the observed
live task, “Create a new worktree and start from main” was saved at 01:11:35 UTC
on October 28 and the identical conflict question returned about 2.6 seconds
later, with no worker start. Permission/tool fixes did not repair this path.
A real-Git regression also reproduced a verified two-parent resolution being
ignored until after an earlier conflicting branch had already failed integration.

The follow-up makes that technical failure executable work. Typed conflict data
files/reuses one bounded Internal repair in the normal graph; its workspace starts
at an observed parent without first integrating the failing history. Its result
must pass ordinary Git branch/base/path checks and contain both recorded parents.
Independent Git tips are selected before integration, so later verified resolutions
suppress obsolete ancestor conflicts regardless of work-row ordering. Existing
synthetic technical questions can be replaced by that owned repair without
inventing owner answers; stale event/project or pending-answer state is refused.

WaitsFor edges preserve original goals if a repair fails or is cancelled. The
same repair identity, approval and cumulative budget survive restarts and new
waiters. Successful repair is followed by actual original-handoff preparation,
not a forced Done or permission override. Real Git/SQLite regressions exercise
both repair execution and useful original execution, plus negative claims,
independent holds, archival and bounded failure. This closes the deterministic
handoff-recovery mechanism; model quality, promotion and live effectiveness
remain separate. See [verification](../verification/shared-git-recovery-2026-10-28.md).

## Obsolete question history in shared recovery

The v6.6.12 rollout at `b5c0a82` passed the official signed binary's isolated
daemon/LocalWorker/Git repair and original-resumption canary. Production then
created one shared repair for 36 waiters, but its execution view exceeded
128 KiB before lease. Read-only observation found 101 obsolete unanswered
question rows (110,237 JSON bytes), 18 answered rows (18,363 bytes), and
42,614 bytes of current work definitions. The smaller deterministic cases had
not covered that accumulated-question shape. Allocation was progress, not
effective live recovery or evidence of model performance.

`ProjectContext::execution_view` now omits an obsolete question only when it
has no answer, answer submission or delegated decision. The complete context
and SQLite history keep every row. All waiters' goals, constraints, owner
answers, active questions and project revision remain; the existing size check
still rejects genuinely oversized active input. No scheduler, budget, question
settlement, persistence format or permission changes are involved.

A real-Git/SQLite regression with 36 waiters, 101 retired questions and 18
owner answers failed on the exact deployed base with the repair blocked. With
the projection change, one worker repairs both histories, every original runs,
all 18 answers reach the repair, the full receipt exceeds 128 KiB and its prompt
view fits. A separate case retains active/obsolete-but-answered and pending
owner input and rejects oversized active input. This corrects the earlier
blanket claim about historical-context overflow; subsequent live outcome still
requires promotion and observation. See [verification](../verification/shared-git-recovery-context-2026-10-28.md).

## Worker autonomy and bounded attempt recovery

Against exact main/deployed e27efed on 2026-10-28, live shared repair e8402ec7
started but failed: 31 successful exec reads, one diff repeated 12 times, no Git
changes or commit, timeout after 1800 seconds and 1,603,490 reported tokens
against a 200,000-token allowance. Its first waiter's plan-only constraint had
been copied into the independent implementation repair. The log does not prove
that contradiction caused the model's behavior. Existing tests explicitly
permitted unlimited repetition before the iteration cap and token overspending
without stopping; the timeout chose same-worker retry before a budget hold.
Earlier context and workspace-access fixes established startup, not recovery.

The follow-up gives the repair its own execution constraints, retains source
constraints/owner direction in project context, and explicitly puts planning,
implementation and verification in one local assignment. A repeat warning gives
the worker another threshold of steps to change course. Persistent unchanged
results stop the attempt as model/loop before another model request, allowing
the existing finite worker-switch rung before same-worker repair. Actual token
spend prevents later requests once the brief allowance is spent; one in-flight
response can overshoot and is fully charged. No permission or budget is widened.

The regressions retain both self-correction and alternate-worker recovery paths
with actual LocalWorker/tool calls, SQLite, Git merges, both-parent verification
and original-work continuation. Models and the alternate Worker are deterministic
fixtures. This does not prove native Codex execution, useful live model reasoning,
a reasoning-manager recovery turn, release deployment or the held original
goals' effectiveness. See the [verification record](../verification/worker-autonomy-recovery-2026-10-28.md).

The exact legacy generated constraint form is also recognized on fresh leases
under the trusted controller record. Only the execution brief is corrected;
extra repair restrictions, persisted history, active runs and budget holds remain.
The initial broad run caught an older standalone test expecting Repair before
SwitchWorker for a loop; it is updated to the approved new order, while the
adjacent real no-alternative repair-evidence case remains in place.

The final review closes another directly affected boundary: failed, blocked,
error-reported or question-bearing local attempts retain their workspaces instead
of force-removing partial edits. The real-Git handoff case reproduces ENOENT
before this refinement, then verifies the successor receives a valid workspace
pointer with uncommitted data intact and completes the original goal. Retained
files do not count as verified work and are not automatically replayed. Required
scripted scenario5 is aligned to early stopping while budget remains, preserving
its first-attempt evidence and the independent owner-grant scenario.
