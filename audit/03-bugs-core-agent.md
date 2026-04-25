# Bugs and dead code — `core`, `store`, `agent`, `cli`

Severity scale: `[C]` critical, `[H]` high, `[M]` medium, `[L]` low.

---

## Concurrency / async

### A-1  [H] Spawned-task panic in parallel tool execution drops the semaphore permit and orphans the agent loop
`crates/rustykrab-agent/src/runner.rs:1149-1162`.

```rust
let _permit = sem.acquire().await.expect("semaphore closed");
```

`expect("semaphore closed")` panics if the semaphore is closed during
shutdown. Replace with explicit error propagation; spawn shutdown
should not panic the worker.

### A-2  [H] `RwLock` poison in `core/active_tools.rs:47,56,62,71` silently recovers
Recovers via `.unwrap_or_else(|e| e.into_inner())`. The recover is
intentional, but no metric or warning is emitted, so a poisoned lock
hides the original panic forever.

**Fix.** Log when a poisoned lock is recovered, with the panic site
embedded.

### A-3  [M] Session expiry not re-checked inside spawned tool tasks
`crates/rustykrab-agent/src/runner.rs:410-412, 761-763, 1149-1162`.
The agent loop checks expiry at iteration start, but a tool spawned in
parallel can outlive that check. Pass a deadline `Instant` into the
spawned future and abort on overshoot.

### A-4  [M] `task_queue.rs` deduplication key leaks on panic
`crates/rustykrab-cli/src/task_queue.rs:104-113`. Key removal happens
at the end of a spawned task. If the task panics, the key never gets
removed and that key becomes permanently un-submittable. Use a RAII
guard so removal runs in `Drop`.

### A-5  [L] Worker loop exits silently when channel closes
`crates/rustykrab-cli/src/task_queue.rs:116`. Logs a warning, then the
loop returns. Anything that depended on this worker silently degrades.
Bubble the loss up to the caller.

---

## Error handling

### A-6  [H] Cron schedule parse failure falls back to "now + 1 hour"
`crates/rustykrab-store/src/jobs.rs:203-204`. A typo in a cron
expression schedules a "ghost" job that fires hourly. Reject invalid
expressions at submit time.

### A-7  [H] `parse_stored_timestamp` falls back to `Utc::now()`
`crates/rustykrab-store/src/jobs.rs:376-379`. Corrupted DB rows look
like fresh jobs. Return an error and let the caller decide whether to
quarantine the row.

### A-8  [M] `MessageContent` deserializer loses the per-format error
`crates/rustykrab-core/src/types.rs:45-98`. Tries multiple shapes and
returns a generic message. Aggregate the inner errors so debugging is
possible.

### A-9  [M] Conversation save uses `INSERT OR REPLACE` with no version check
`crates/rustykrab-store/src/conversation.rs:38-48`. Two concurrent
writers to the same conversation id race silently. Add a CAS column
(e.g. `version` or `updated_at`) and bail on mismatch.

### A-10 [M] Tool retry loses earlier failure context
`crates/rustykrab-agent/src/runner.rs:1113-1121, 1152-1160`. After
exhausting retries, only the last error survives. Aggregate (or at
least carry tail count + first/last cause).

### A-11 [L] `truncate_summary_to_tokens` can run pathologically long on bad UTF-8
`crates/rustykrab-agent/src/runner.rs:121-134`. Walks back to find a
char boundary with no upper bound. With a malformed string, this
walks the whole length each call. Bound the walk to e.g. 8 bytes.

---

## Logic / heuristics

### A-12 [M] `empty_response_retries` not reset on tool errors
`crates/rustykrab-agent/src/runner.rs:473-475`. Only reset on success,
so a previous tool error eats into the next iteration’s retry budget.

### A-13 [M] `effective_compaction_summary_cap` can become 0
`crates/rustykrab-agent/src/runner.rs:1226-1240`. Compute as
`(max_context_tokens / 4).min(...)`. With small `max_context_tokens`
(tests, misconfig) the cap collapses to 0 and the summary is just a
marker. Clamp to a minimum of e.g. 256.

### A-14 [M] `RUSTYKRAB_COMPACTION_CONTEXT_CEILING` parsed without bounds check
`crates/rustykrab-agent/src/runner.rs:64-70`. Filter to a sensible
minimum.

### A-15 [M] Self-consistency vote silently drops empty responses
`crates/rustykrab-agent/src/voting.rs:122-126`. If half the samples
return empty text, the vote runs on the surviving half with no
warning. Surface a `low_confidence` flag if too many samples were
empty.

### A-16 [M] Iteration-cap final call sends empty tool schemas
`crates/rustykrab-agent/src/runner.rs:709, 1087`. The final summary
turn passes `&[]` for schemas. Consistent with "no more tool calls"
semantics, but the model isn’t told that, so it sometimes hallucinates
tool calls anyway. Either include the schemas with a system note, or
use the provider’s `tool_choice: "none"` equivalent.

### A-17 [L] `is_planning_only` heuristic doesn’t handle "The best approach is..."
`crates/rustykrab-agent/src/runner.rs:182-206`. False negatives leak
through. Acceptable as long as the cap defends.

### A-18 [L] `ToolStats::success_rate` returns 1.0 on zero calls
`crates/rustykrab-agent/src/trace.rs:27-32`. Misleads downstream
ranking. Return `Option<f32>` or 0.5.

---

## Sandbox / isolation

### A-19 [H] `ProcessSandbox::execute` is a no-op
`crates/rustykrab-agent/src/sandbox.rs:130-162` validates policy and
applies a timeout, but the body is `Ok(args)`. This is a stub —
nothing is actually sandboxed. The real isolation for code execution
is in `tools/sandboxed_spawn.rs`, which is correct, but anyone reading
`agent::sandbox` would assume `execute()` does the work.

**Fix.** Either delete this layer or implement it. At minimum, mark
it `#[deprecated]` and route callers to `tools::sandboxed_spawn`.

### A-20 [L] `_classifier` field unused in `agent::router::HarnessRouter`
`crates/rustykrab-agent/src/router.rs:16-18`. Decide: remove or
implement.

### A-21 [L] Unused param `_completion_tokens` in `classify_response`
`crates/rustykrab-agent/src/runner.rs:166`. Refactor leftover.

---

## Tests / dead code

### A-22 [L] Real `panic!()` left in production code
`crates/rustykrab-agent/src/runner.rs:2435` —
`panic!("assistant message should be text")`. The line is inside a
test (`#[cfg(test)]` block above), so this is acceptable, but call it
out as the only `panic!` in the workspace per
`grep -rn "panic!(" crates/*/src --include="*.rs"`.

### A-23 [L] CLI `harness_profile` load behavior on missing file is unspecified
`crates/rustykrab-cli/src/main.rs:184`. Trace through and document or
default.

### A-24 [L] Many `let _ = …` swallows in CLI/main
`crates/rustykrab-cli/src/main.rs:321,719,822,911,924,1208,1210,1351`.
Most are intentional (typing indicators, telemetry writes), but
`store.secrets().set(...)` failures and credential writes should at
least `tracing::error!`.

### A-25 [L] `keychain::set_credential` ignored at `crates/rustykrab-store/src/registry.rs:114,116,124,133`
Every credential persistence call is `let _ = ...`. Surfacing the
errors would have caught at least one issue during my read (the
fallthrough order between OS keychain and SQLite store is partly
relying on these silently working).

