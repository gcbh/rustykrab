# System Overview

*Second pass, against `main` at `fd1f1e2`. The first pass reviewed `d945495`;
22 commits have landed since, including nine that came out of that review.
Findings it resolved are recorded in [`05-first-pass-outcome.md`](05-first-pass-outcome.md).*

## What this system is

A single-tenant, self-hosted AI agent daemon. One process holds a model
provider, a tool registry, a hybrid memory system, an HTTP gateway, and a set
of long-polling channel loops. A message arriving on any surface becomes a
*turn*: the agent loop calls the model, executes the tools it asks for, and
repeats until the model signals completion.

**Second-pass snapshot: 15 crates, ~90,200 lines, 949 tests.** Current mechanical
counts live in the generated crate summaries; the continuity follow-up below
is derived against base `0b565fd` plus its recorded working-tree changes.

## Crate graph

```
rustykrab-core        (no internal deps — the contract layer)
   ^  ^  ^  ^  ^
   |  |  |  |  +-- rustykrab-providers   anthropic, openai, ollama, scripted, claude-cli
   |  |  |  +----- rustykrab-memory      hybrid retrieval, own SQLite db
   |  |  +-------- rustykrab-skills      SKILL.md + agents/*.md loader, ed25519 verify
   |  +----------- rustykrab-channels    telegram, slack, signal, video, mcp
   +-------------- rustykrab-store       SQLite: conversations, secrets, jobs
                        ^      ^
   rustykrab-tools  ----+      +---- rustykrab-dream
        ^
        |
   rustykrab-control (core, store, tools, projects)   work-item graph, ladder, Worker,
                                            worker registry, routing, worktrees,
                                            review surface (issue projection)
                                            question router, standing judgment
        ^
        |
   rustykrab-agent   (core, tools, control, skills, providers)
        ^
        |
   rustykrab-runtime (core, store, agent, memory, skills)   <-- NEW
        ^      ^
        |      |
        |      +-- rustykrab-gateway  (+ channels, control)
        |               ^
        +---------------+-- rustykrab-cli  (+ control)

rustykrab-projects    (no internal deps — immutable planning domain)
   ^                  ^
   +-- rustykrab-store    +-- rustykrab-gateway
       revision storage      planning HTTP surface
       (also uses core)      (also uses runtime/store/channels)

rustykrab-e2e         (core, store, tools, agent, providers)
                     daemon boundary tests + direct production-compactor ablation
```

`rustykrab-control` is the control layer of
`docs/plans/control-layer-and-worker-fleet.md`: the work-item graph, the
resolution ladder, the error taxonomy and the controller loop, over the
store, with the `Worker` trait `rustykrab-agent` implements (`LocalWorker`,
`ExternalWorker`, and `PeerWorker` for a paired node, which reaches the
node's delegated-task API over HTTP; on the node, `DelegatedRuns` runs a
peer's brief).
The gateway depends on it directly for `/api/work` (its `ControlHandle`, the
reply types and the re-exported `Provenance`, so the gateway needs no direct
dependency on `rustykrab-tools`), and the CLI for the `work` subcommand and
the composition root, which builds the controller, its local worker and its
host wiring (tick loop, notice delivery, scheduled firings).

`rustykrab-runtime` is new since the first pass and is the significant change
to the shape of the system. The turn-running layer used to live inside the
Axum crate, so the Telegram and Slack loops depended on a web server to do
non-HTTP work. They now call `rustykrab-runtime` directly, and the crate has
no axum in its dependency tree.

## Layering

| Layer | Crate(s) | Role |
|---|---|---|
| Contracts | `core` | `Tool`, `ModelProvider`, `MemoryBackend`, `Capability`, `Session`, token estimation |
| Planning domain | `projects` | Immutable revisions, provenance rules, validated planning graph, deterministic projections |
| Capability providers | `providers`, `store`, `memory`, `skills`, `channels` | Each owns one external dependency |
| Behaviour | `tools`, `control`, `agent` | Tool implementations; the control layer (work items, ladder, controller loop); the model-call/tool-exec loop and the local worker |
| Application service | **`runtime`** | Assemble a turn: prompt, session, capabilities, memory hooks |
| Transport | `gateway`, channel loops in `cli` | HTTP/SSE, Telegram polling, Slack events |
| Composition | `cli` | Read env, build everything, spawn background tasks |
| Verification | `e2e`, `dream` | Black-box scenarios, direct compactor ablation; offline outcome analysis, and the control layer's evaluation pass (expectation metrics, proposals filed through the controller's validator) |

The context evaluator also links `core` and `tools` to reuse production tool
schemas and argument validation for inert replacements. Turn execution still
crosses the real daemon process boundary; it does not call the runner directly.

The spine is now complete — the application-service row is a real crate
rather than a module inside the transport. That was the first pass's
highest-priority structural finding.

## Runtime topology

Unchanged in shape:

```
main()
 ├─ gateway HTTP server            (axum, :3000)
 ├─ telegram_agent_loop            long-poll  -> process_telegram_message
 ├─ slack_agent_loop               events     -> process_slack_message
 ├─ signal receive loop
 ├─ job_executor_loop              30s tick   -> due cron jobs -> TaskQueue
 │   (or, with RUSTYKRAB_CRON_WORK_ITEMS=1, scheduled_work: each firing
 │    filed as a work item the controller runs)
 ├─ TaskQueue worker               in-memory mpsc, bounded
 ├─ controller tick loop           work items: lease, run, reconcile, age
 ├─ work notice delivery           the work outbox, to each item's thread
 ├─ delegated-task worker          durable queue in `delegated_tasks`: free
 │                                 text, or a peer's typed brief run as a
 │                                 local worker inside this node's ceiling
 ├─ worker refresh                  2s: each peer's advertisement and health,
 │                                 the local worker's model check (30s)
 ├─ control loop                   tick -> lease -> worker runs: a local
 │                                 conversation, a `claude`/`codex`
 │                                 process in a worktree under the data dir,
 │                                 or a peer node's task over the tailnet
 ├─ work outbox notifier           one message per parent, from `work_outbox`
 ├─ memory idle lifecycle sweep
 ├─ memory FTS5 index rebuild      once, at boot
 └─ DreamWorker                    read-only outcome analysis, idle-gated
```

Two task queues still coexist with different durability guarantees — the
in-memory one for cron, credential and payment-approval wakes, the durable one for peer
delegation. Cron survives the gap because `scheduled_jobs.next_run_at` only
advances after execution, so a dropped task is re-picked. That reasoning is
still implicit and is the only thing making the in-memory queue safe.

## The path of a message

1. `telegram_agent_loop` long-polls, filters by allowlist, resolves
   `(chat_id, thread_id)` to a conversation via an in-memory map, then
   `channel_bindings`, then by creating one.
2. `process_telegram_message` loads the conversation — healing a binding that
   outlived it — snapshots persisted message ids, appends the user message,
   saves that inbound turn, and starts a typing task.
3. `rustykrab_runtime::run_agent_interactive` now uses shared `prepare_agent`
   for the system prompt, capabilities, session, active tools, durable recall,
   memory callback, retrieval log and optional outcome sink. A task-owned
   activity guard lasts through completion. The omitted wiring found against
   base `0b565fd` is repaired and covered by captured-wire regression trials.
4. `AgentRunner::run_inner` — **one loop now**, parameterised by an event
   sink — compacts if over budget, calls the provider, classifies the
   response, executes tools in parallel under the sandbox policy.
5. The caller drains `AgentEvent`s as a heartbeat.
6. `save_turn(&conv, &persisted_ids)` appends this turn's messages, including
   partial history returned on error/cancellation. An interrupted action's
   outcome may be unknown; persistence is not proof of task completion.
7. Optionally an `OutcomeRecord` plus attributions are written.
8. Any `PendingLinks` minted this turn are delivered as a separate message —
   on chat surfaces only.

## Cross-cutting observations

**Compaction is no longer driven by a guess.** `predicted_prompt_tokens`
anchors on the last response's actual `prompt_tokens + completion_tokens` and
applies the chars-per-token heuristic only to messages appended since. This
is a genuine improvement on what the first pass reviewed: the heuristic
undercounts JSON-heavy history by ~40%, which previously let real prompts
reach the window while the estimate sat comfortably below the threshold. The
estimator is now a *delta* estimator, and `core::token_estimate` says so.

**Configuration is still ambient.** There are 47 direct environment reads in
library crates rather than the composition root: 25 in `tools`, 8 in
`providers`, 5 in `gateway`, 4 in `agent`, 3 in `store`, and one each in
`channels` and `skills`. This remains the single largest obstacle to reusing
`tools` or `agent` elsewhere. `memory` and `runtime` both read zero, which is
why they are the two most portable crates here.

**The turn sequence is still written out at every call site.** Load
conversation, snapshot persisted ids, append, run with a heartbeat,
`save_turn`, extract the reply, map failures to a user-facing string. It
appears in `process_telegram_message`, `process_slack_message`,
`send_message`, `send_message_stream`, and twice in `task_queue.rs`. The
first pass counted three; it is now more. `rustykrab-runtime` is the obvious
home for it and does not yet contain it.

**Two databases, still no joins across them.** `outcome_attributions` rows
with `kind = 'memory'` name a row in `memory.db`. Unchanged.

## Context and monitoring continuation

This continuation starts from `feat/control-layer-phase1` at `5cae23b` and
integrates `feat/control-p4-questions` at `030dce6`. The controller keeps the
newer routing, spend, drain and lock mechanisms alongside question routing,
standing judgment, planning and progress-ledger repair. Planner work is a
separate definition in the shared local model slot.

The monitor is a transport observer, not a second scheduler:
`Store::work_monitor_snapshot` → read-only `WorkerRegistry` views and
`ControlHandle::loop_status` → `/api/monitor`, `/api/monitor/metrics`,
`rustykrab monitor` and the static browser dashboard. Durable work counts
and assignments cover all rows, while histories are bounded. Verification
and cost come from judged evidence and finalized run records. Nightly
evaluation now reads the actual question record through `StoreQuestions`.

## Native subscription runtime follow-up (2026-10-02)

The `claude-cli` model provider hands Rust model turns to a selected Max
login with native tools disabled, then returns structured calls to the
existing Rust harness. Native execution workers select a Max profile each
and use Claude's own tools in isolated worktrees. `agent -> providers` is a
new dependency for shared native login isolation/verification; `control`
still owns only the worker/spec contract. With local execution disabled and
the planner explicitly enabled, the fleet needs no Ollama model. Account
and observed quota state reach the existing monitor, not a second dashboard.

Codex subscription executors use the same external-worker path. Their
`CODEX_HOME` and ChatGPT-only requirement are stored independently per worker;
shared auth verification and quota sanitization stay in `providers`, with no
new crate edge or controller dependency. An owned native CLI runs the work
under its workspace sandbox; the Rust controller checks its result and
records native usage. Claude planning and execution can coexist with Codex
execution without an Ollama worker or an API fallback. Multiple profiles for
the same account share quota; profile count does not multiply subscriptions.


Project continuity now crosses the same `Brief` boundary. The controller loads
a durable project revision plus work/question history, freezes it with the lease
and pins coding work to its project's verified commit chain. Native adapters
render it without depending on Claude/Codex chat state. Archive compaction keeps
receipts and evidence, and the monitor shows the delivered revision and base.
This changes no crate dependency and introduces no second scheduler or ledger.

## Work-manager composition (base 3e85f48, 2026-10-08)

An opt-in v6 manager is the conversational control plane: it captures goals
and resource requirements, files durable work and lets the existing controller
lease a native Claude/Codex worker or a registered infrastructure adapter.
Each managed cron firing follows that same path, carrying persisted execution
requirements and retaining a work link and run history. No model-only manager
claim counts as completion. The runtime grants management tools without direct
shell/write/message execution.

The CLI owns service probing and launchd lifecycle execution; gateway reads
consume cached observations through ResourceObserver. Service recovery is a
tracked work item, bounded and restricted to registered services with verified
ownership. Native CLI workers and service executors are separate resources.
The existing main and builder instances are not automatically made executors
or migrated by enabling the manager. Private Tailscale access keeps the gateway's
normal bearer, Origin and revocable device-token controls.

## Browser access boundary

The gateway can optionally authenticate an explicitly allowed Tailscale Serve
owner instead of a bearer credential. The composition root pins one HTTPS
.ts.net origin and nonempty owner allowlist. Kernel-observed loopback peer,
exact authority and Serve identity must agree; forwarded IPs are not proof.
Existing Origin/CSRF guards still apply. Chat and monitoring share a static
browser access helper; network identity needs no token or session table.
There is no change to channel intake, scheduling or native agent execution.

## Single-owner consolidation

`consolidate stage` merges operator-owned legacy snapshots into the v6 data
model with receipts, conflict refusal, credential re-encryption and memory
provenance. It is offline staging, not concurrent sharing among old schedulers.
The optional Claude CLI integration adapter executes personal tasks through
the same fleet/controller as native coding work. Operators hand channel and
schedule ownership to one daemon after verifying the stage, as documented in
[`instance-consolidation.md`](../instance-consolidation.md).

## Consolidated briefing and delivery observations

The manager may persist dated reports in a bounded Markdown vault shared by
host tools and authenticated monitor reads. Telegram send acknowledgements
and scheduled delivery evidence separate executed work from delivered results.
Paused work waiting for a user no longer prevents idle project dreaming.

Integration completion passes through a CLI-owned WorkBackend wrapper before the
controller receives result_report. Host tool observations cannot be deferred until
Worker::run returns: the model-facing report call can already close the item.
