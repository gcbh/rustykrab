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

Against base `0b565fd`, deterministic wire capture exposed Telegram's separate
runner setup, and code inspection found lossy latest-user compaction, failed-run
history loss and queued-but-undispatched follow-ups. This follow-up shares runner
setup, pins the current inbound instruction, returns partial history with errors,
and processes accepted input at iteration/completion boundaries under a shared cap.
Telegram persists before setup and after failure, and serializes admission fallback.
These are targeted repairs to finding 1, not removal of all six turn sequences.
Hard process death, reset races and asynchronous memory-write completion remain
outside the proven contract. See `docs/evals/payment-and-runtime-contract.md`.

The subsequent ablation follow-up adds request-schema-aware input budgets and
refuses oversized Ollama requests without silently dropping history. Compaction
now checks mandatory anchors, preserves tool pairing, bounds oversized summary
fragments and supports controlled prompt/tail alternatives. Production behavior
and proposed policies are distinguished in the study evidence.

Telegram and Slack now journal user UUIDs before admission and use reset
generations to cancel obsolete work and suppress later obsolete delivery.
HTTP persists inbound and partial-error history; SSE owns cancellation through
completion rather than dropping the running future. These remain targeted
repairs: upstream acknowledgement gaps, exactly-once external effects,
asynchronous memory durability, concurrent HTTP turns and full recovery UX are
not resolved. Earlier evidence does not retroactively prove these new paths.

The live synthetic daemon follow-up exposed a separate context-contract defect:
`tools_load.active` echoed default-seeded names missing from the real catalog,
while the soul unconditionally instructed memory lookup. The corrected report
filters registered, available and permitted tools; runtime guidance makes
generic tool references conditional on offered schemas. HTTP/Telegram wire
fixtures join the real report to the next schema set. This closes the observed
static-catalog mismatch, not dynamic availability cache invalidation or all
model task-selection failures. Existing custom soul files are preserved.

## Method note

Three findings were wrong, and all three were caught by *implementing* them
rather than by re-reading. Static review is good at locating things and bad
at judging severity; the severity claims are the ones that needed the code
run at them. Worth remembering when reading
[`OPINION.md`](OPINION.md) — its confidence labels are the least reliable
part of it.
