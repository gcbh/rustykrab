# Extension Seams and Reusability

Every trait in the workspace, who implements it, and whether it is carrying its
weight. Counts are `impl X for` occurrences including test doubles.

## Inventory

| Trait | Defined in | Impls | Verdict |
|---|---|---|---|
| `Tool` | `core/tool.rs` | 65 | **Load-bearing.** The system's spine |
| `ModelProvider` | `core/model.rs` | 4 real + 12 test | **Load-bearing.** Genuinely swappable backends |
| `MemoryStorage` | `memory/storage.rs` | 1 | Fine — used for test injection and a real seam |
| `Embedder` | `memory/embedding.rs` | 4 | **Earns its keep.** Hash/FastEmbed/lazy variants |
| `Sandbox` | `agent/sandbox.rs` | 2 | Earns its keep |
| `CredentialBackend` | `store/credential_backend.rs` | 4 | **Earns its keep.** Keychain / encrypted-file / env |
| `OutcomeSink` / `OutcomeSource` | `core`, `dream` | 2 / 4 | Correct inversion between writer and analyser |
| `RecallPersistence` | `core/recall.rs` | 2 | Fine |
| `TraceSink` | `core/prompt_trace.rs` | 2 | Fine |
| `RequestNotifier` | `store/credential_request.rs` | 1 | Fine — breaks store→push dependency |
| `MemoryBackend` | **`core`** | 2 | **Moved.** The one that was blocked |
| `CronBackend` | **`tools`** | 2 | Correctly implemented above the consumer |
| `MessageBackend` | **`tools`** | 1 | Correctly implemented above the consumer |
| `ComputerBackend` | **`tools`** | 2 | Correctly implemented above the consumer |
| `VideoBackend` | **`tools`** | 1 | Fine — implementation is in the same crate |
| `SessionManager` | **`tools`** | 1 | Correctly implemented above the consumer |
| `Skill` | `skills/skill.rs` | 1 | Thin — `SkillMd` is the only shape |
| `Channel` | `channels/channel.rs` | **1** | **Not earning its keep** |
| `GatewayBackend` | `tools/gateway_backend.rs` | **0** | **Dead** |

## Backend trait placement is resolved

> **Corrected after implementation.** This section originally said six traits
> were misplaced. Checking every implementor, only one is: `MemoryBackend`.
> The other five are implemented in `rustykrab-cli`, `rustykrab-agent` or
> `rustykrab-tools` itself — all at or above the tool crate — so none of them
> is blocked and moving them would be churn. `MemoryBackend` was also the only
> one that had produced a pass-through adapter, which is the tell. See the
> table below.

`MemoryBackend` has now moved to `rustykrab-core`; `HybridMemoryBackend`
implements it directly and the pass-through CLI adapter is gone. The other
five backend traits remain in `rustykrab-tools` because their real implementors
already sit in that crate or above it in the dependency graph. Moving them
would remove no cycle and delete no adapter.

Which traits are actually blocked, by where their real implementor lives:

| Trait | Real implementor | Crate | Blocked? |
|---|---|---|---|
| `MemoryBackend` | `HybridMemoryBackend` | `rustykrab-memory` | **resolved — trait is now in `core`** |
| `SessionManager` | `SubagentRunner` | `rustykrab-agent` | no, above |
| `ComputerBackend` | `EnigoXcapBackend` | `rustykrab-cli` | no, above |
| `VideoBackend` | `VideoChannelAdapter` | `rustykrab-tools` | no, same crate |
| `CronBackend` | `CronAdapter` | `rustykrab-cli` | no, above |
| `MessageBackend` | `MessageAdapter` | `rustykrab-cli` | no, above |

`CronAdapter` and `MessageAdapter` are also not pass-throughs — `CronAdapter`
merges the calling conversation's channel context into cron arguments, which
is real logic in the right place.

## Abstractions that are not earning their keep

**`Channel` (1 implementor).** The trait declares `name`/`receive`/`send`.
Only `WebChatChannel` implements it. `TelegramChannel`, `SlackChannel`,
`SignalChannel` and `VideoChannel` are concrete types with per-channel
long-poll loops in `cli/src/main.rs`, held in `AppState` as four separate
`Option<Arc<ConcreteType>>` fields, and dispatched by `ChannelHub` matching on a
channel-name string. Adding a fifth channel today means: a new `AppState` field,
a new `with_*` builder, a new arm in `ChannelHub`, a new agent loop, a new
`*_chat_map` table and store module, and new arms in the cron delivery resolver.

Either make `Channel` real — `receive_batch` / `send_to(thread)` / typing
indicator / addressing key — and store `Vec<Arc<dyn Channel>>`, or delete the
trait and stop implying an abstraction that isn't there. The current state is
the worst of both: a trait that suggests channels are pluggable and a codebase
where they are not.

See [`03-dead-code-audit.md`](03-dead-code-audit.md) for the full treatment of
each. In brief:

**`GatewayBackend` (0 implementors).** The trait, `GatewayTool`, and the
`automation_tools()` factory that constructs it are all unreachable — the CLI
builds `CronTool` directly. Dead code.

**`HarnessProfile` / `HarnessRouter`.** Four profiles exist. `research()` is
byte-identical to `default()` except its name. `coding()` differs in two fields;
`creative()` in three. `HarnessRouter` holds
`_classifier: Arc<dyn ModelProvider>` with a comment saying it is "kept for
potential future use" and is never read — the routing is keyword matching. So
the machinery is: a model provider that is never called, selecting between
profiles that differ by at most three integers, layered over a pinned-field
override system that then puts most of them back. Either the profiles should
diverge enough to justify the routing, or routing should be replaced by three
config knobs.

## Duplication, measured (second pass)

**The agent loop is one function.** `run_inner` and `run_streaming_inner` were
82% identical; they are now a single loop parameterised by an event sink. The
merge is validated by symbol counts rather than by inspection: the symbols
that had been written twice halved (`max_tokens_retries` 8→4,
`compact_history(conv, tools)` 2→1) and the shared ones did not move.

**The turn sequence is still written out six times.** Load conversation →
snapshot persisted ids → append → run with heartbeat → `save_turn` → extract
reply → map failure to a user string, in `process_telegram_message`,
`process_slack_message`, `send_message`, `send_message_stream`, and twice in
`task_queue.rs`. The first pass counted three.

This one has a defect to its name rather than a hypothesis: the
`PendingLinks` drain existed in the Telegram copy and not the Slack one, so a
scheduled job minted a credential link and dropped it. It was fixed by adding
the drain to a second copy — the failure mode repeating rather than resolving.
`rustykrab-runtime` now exists and is the obvious home.

**The three providers** still each have `build_messages` / `build_tools` /
`parse_response` / `map_status_error`. Still justified: the wire formats
genuinely differ, and the parts that *are* shared (`line_buffer`, `backoff`)
are the parts that should be.

**Token estimation** is one function in `core`. Note the provider-side
constant is deliberately *not* folded in, and the module doc explains why —
collapsing them removes headroom between the compaction threshold and the
provider's own trim budget.

## Reusability, crate by crate

Could each crate be lifted into a different program?

| Crate | Reusable? | Blocker |
|---|---|---|
| `core` | **Yes** | None. Clean contract crate, no internal deps |
| `providers` | **Yes** | 8 direct env reads (`OLLAMA_NUM_CTX`, `ANTHROPIC_CONTEXT_LENGTH`, …) override explicit config |
| `memory` | **Yes** | Zero env reads, own storage trait, own config struct. The most portable crate here |
| `skills` | **Yes** | Near-standalone |
| `store` | **Mostly** | 3 env reads; single-connection design is an embedding constraint |
| `channels` | **Mostly** | Concrete types, no unifying trait, so the consumer inherits the coupling |
| `dream` | **Yes** | Depends only on `OutcomeSource`. Textbook |
| `tools` | **No** | 25 env reads; direct `rustykrab_store` coupling in 12 files |
| `agent` | **Partly** | 4 env reads inside compaction tuning; depends on all of `tools` to get 16 of its own tools registered |
| `runtime` | **Yes** | 0 env reads, no axum. Untested, which is its own problem |
| `gateway` | **Mostly** | Transport only now; `AppState` down to 10 fields |
| `cli` | N/A | Composition root by definition |

Trait placement is resolved. Ambient configuration is not: `agent` and
`tools` remain the two crates that most want to be reusable and are least
able to be, for that one reason.

## Ambient configuration

Direct environment reads outside the composition root and E2E harness:

| Crate | Reads | Examples |
|---|---|---|
| `tools` | 25 | `BROWSER_HEADLESS`, `CHROME_CDP_URL`, `X_API_BEARER_TOKEN`, `RUSTYKRAB_MCP_SERVERS`, `RUSTYKRAB_NODES` |
| `providers` | 8 | `OLLAMA_NUM_CTX`, `OLLAMA_KEEP_ALIVE`, `ANTHROPIC_CONTEXT_LENGTH` |
| `gateway` | 5 | `RUSTYKRAB_ALLOWED_ORIGINS`, `RUSTYKRAB_PUBLIC_URL` |
| `agent` | 4 | `RUSTYKRAB_COMPACTION_*` |
| `store` | 3 | `RUSTYKRAB_MASTER_KEY`, `RUSTYKRAB_DISABLE_KEYCHAIN` |
| `channels` | 1 | `TELEGRAM_API_BASE` |
| `skills` | 1 | dynamic skill requirements / prompt path |

Each read is individually defensible — it is the escape hatch for an operator
knob. Collectively they mean a library crate's behaviour depends on process
state its caller did not pass it. Two tests cannot configure the same component
differently, and a test that sets one of these variables races every other test
in the binary — which has already happened once (`37c5036`, "stop the nodes
tests racing over a process-global env var").

The fix is the standard one: each crate exposes a `Config` struct with
`from_env()`, the composition root calls `from_env()` once and passes the struct
down. `MemoryConfig` already does exactly this, which is why `memory` is the
most portable crate in the workspace.

## What is genuinely good here

Worth naming, because a review that only lists problems misrepresents the code:

- **`Tool` is a well-designed trait.** `available()` keeps unconfigured tools out
  of the model's schema list; `sandbox_requirements()` replaces hardcoded
  name allowlists with declared capabilities; `blocks_turn()` encodes a subtle
  and real distinction (stopped-early vs stopped-because-blocked) that most
  agent loops get wrong.
- **`ModelProvider` defaults are correctly chosen.** `context_limit`,
  `supports_vision`, `requires_paired_tool_results`, `chat_with_ctx`,
  `chat_with_choice` all have defaults that degrade safely, so a new provider
  implements one method.
- **The capability model is real.** `Capability::Subagent` and
  `Capability::ComputerUse` are required *in addition to* the per-tool grant,
  and the dangerous ones are gated at four independent layers.
- **The comments explain why, not what.** Repeatedly the code documents the
  reasoning behind a non-obvious choice — why old rows are not back-filled with
  the current version, why `""` and not `NULL` is the Slack no-thread sentinel,
  why `http2` had to be added to reqwest's features. This is unusually good and
  it is what makes the codebase reviewable at all at 80k lines.
