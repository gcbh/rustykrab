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

**Current tree: 14 crates, ~90,200 lines, 949 tests.**

## Crate graph

```
rustykrab-core        (no internal deps — the contract layer)
   ^  ^  ^  ^  ^
   |  |  |  |  +-- rustykrab-providers   anthropic, openai, ollama, scripted
   |  |  |  +----- rustykrab-memory      hybrid retrieval, own SQLite db
   |  |  +-------- rustykrab-skills      SKILL.md loader + ed25519 verify
   |  +----------- rustykrab-channels    telegram, slack, signal, video, mcp
   +-------------- rustykrab-store       SQLite: conversations, secrets, jobs
                        ^      ^
   rustykrab-tools  ----+      +---- rustykrab-dream
        ^
        |
   rustykrab-agent   (core, tools)
        ^
        |
   rustykrab-runtime (core, store, agent, memory, skills)   <-- NEW
        ^      ^
        |      |
        |      +-- rustykrab-gateway  (+ channels)
        |               ^
        +---------------+-- rustykrab-cli

rustykrab-projects    (no internal deps — immutable planning domain)
   ^                  ^
   +-- rustykrab-store    +-- rustykrab-gateway
       revision storage      planning HTTP surface
       (also uses core)      (also uses runtime/store/channels)
```

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
| Behaviour | `tools`, `agent` | Tool implementations; the model-call/tool-exec loop |
| Application service | **`runtime`** | Assemble a turn: prompt, session, capabilities, memory hooks |
| Transport | `gateway`, channel loops in `cli` | HTTP/SSE, Telegram polling, Slack events |
| Composition | `cli` | Read env, build everything, spawn background tasks |
| Verification | `e2e`, `dream` | Black-box scenarios; offline outcome analysis |

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
 ├─ TaskQueue worker               in-memory mpsc, bounded
 ├─ delegated-task worker          durable queue in `delegated_tasks`
 ├─ memory idle lifecycle sweep
 ├─ memory FTS5 index rebuild      once, at boot
 └─ DreamWorker                    read-only outcome analysis, idle-gated
```

Two task queues still coexist with different durability guarantees — the
in-memory one for cron and credential wakes, the durable one for peer
delegation. Cron survives the gap because `scheduled_jobs.next_run_at` only
advances after execution, so a dropped task is re-picked. That reasoning is
still implicit and is the only thing making the in-memory queue safe.

## The path of a message

1. `telegram_agent_loop` long-polls, filters by allowlist, resolves
   `(chat_id, thread_id)` to a conversation via an in-memory map, then
   `channel_bindings`, then by creating one.
2. `process_telegram_message` loads the conversation — healing a binding that
   outlived it — snapshots persisted message ids, appends the user message,
   starts a typing task.
3. `rustykrab_runtime::run_agent_interactive` → `prepare_agent`: system
   prompt, capability set, `Session`, memory write-back callback, `AgentRunner`.
4. `AgentRunner::run_inner` — **one loop now**, parameterised by an event
   sink — compacts if over budget, calls the provider, classifies the
   response, executes tools in parallel under the sandbox policy.
5. The caller drains `AgentEvent`s as a heartbeat.
6. `save_turn(&conv, &persisted_ids)` appends this turn's messages.
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
