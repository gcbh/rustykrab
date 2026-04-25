# Bugs and dead code — `gateway`, `channels`, `providers`

Severity: `[C]` critical, `[H]` high, `[M]` medium, `[L]` low.

---

## Gateway (Axum HTTP, SSE)

### G-1  [C] `WebChat::receive` is a permanent error
`crates/rustykrab-channels/src/webchat.rs:44-50`. Already covered as
critical in `01-critical.md` §C-8. Restated here because it sits at
the gateway / channels boundary.

### G-2  [H] SSE events dropped via `try_send` with only a warning
`crates/rustykrab-gateway/src/routes.rs:218`. Under load or after
client disconnect, agent events are lost without backpressure to the
agent loop. Use a bounded `mpsc` with `send().await` and a deadline,
or maintain an overflow buffer for tool-call events.

### G-3  [M] SSE panic-watcher races against stream close
`crates/rustykrab-gateway/src/routes.rs:272-279`. Panic watcher posts
to `panic_tx` which may already be closed; `let _ = ...await` hides
the failure. Log under `error!` if the post fails, so the panic isn’t
silently dropped.

### G-4  [M] Heartbeat monitor abort is a no-op after natural exit
`crates/rustykrab-gateway/src/routes.rs:260`. `monitor.abort()` after
the monitor has already exited does nothing. The agent loop has no
cancellation token, so a tool call that hangs past the 5-min heartbeat
keeps running. Wire a cancellation token through.

### G-5  [M] Origin policy parses header values with `.unwrap()`
`crates/rustykrab-gateway/src/origin.rs:97,100,104` and
`gateway/src/lib.rs:29,33,39`. Unparseable header values should fall
back to the default response, not panic.

### G-6  [L] `Conversation` save error in streaming path is logged but not signalled to the client
`crates/rustykrab-gateway/src/routes.rs:264-266`. The user sees a
successful stream even though persistence failed. Promote the log
level and emit a final `error` SSE event.

### G-7  [L] SSE keep-alive interval is hardcoded (15s)
`crates/rustykrab-gateway/src/routes.rs:331-335`. Expose as
`SSE_KEEPALIVE_SECS`.

---

## Channels — Telegram

### G-8  [H] `Retry-After` not honored on 429
`crates/rustykrab-channels/src/telegram.rs:211-214`. Comment says we
do, code uses fixed exponential backoff. Parse the header and prefer
its value.

### G-9  [M] Bare `@mention` reply uses `let _ = self.send_text(...)`
`crates/rustykrab-channels/src/telegram.rs:445-460,476`. Send failures
go unreported; user sees nothing.

### G-10 [M] Inbound queue capacity (256) silently drops messages on overflow
Same file, ~lines 495-503. Errors propagate to the agent but not to
the user. Either ack in the chat ("we’re busy, try again") or expose
a richer status.

### G-11 [L] `update_id` already in payload but never used (cross-ref C-6 / S-21)
Listed here so the channel-side fix path is clear: store `update_id`
in a per-bot LRU keyed on `(chat_id, update_id)` and dedupe before
dispatch.

---

## Channels — Signal

### G-12 [M] Signal webhook secret optionality is opaque
See `02-security.md` §S-33. Verify that `with_webhook_secret()` is
required at construction, or that `parse_webhook_payload` matches
Telegram’s "no secret ⇒ refuse" pattern.

### G-13 [L] `send_to_group` lacks tests and group routing isn’t wired
`crates/rustykrab-channels/src/signal.rs:132-154`. The reverse path
(group_id from envelope → response delivered to the same group) isn’t
threaded through.

---

## Channels — MCP

### G-14 [M] Tool definition cache never invalidated
`crates/rustykrab-channels/src/mcp.rs:302-306`. No TTL, no
`refresh_tools`. Add either.

### G-15 [L] `JsonRpcResponse::jsonrpc` and `JsonRpcError::data` carry `#[allow(dead_code)]` but are written by serde
`crates/rustykrab-channels/src/mcp.rs:83,94`. Drop the `allow` and
add a comment that serde owns the field.

### G-16 [L] Test path uses `unwrap()` on `serde_json::to_string`
`crates/rustykrab-channels/src/mcp.rs:378`. Tests should `expect("...")`.

### G-17 [L] MCP cancel/kill use `let _ = …` (intentional, but worth noting)
`crates/rustykrab-channels/src/mcp.rs:182,330,339,360`. These are
shutdown best-effort; OK as is.

---

## Providers — Anthropic

### G-18 [C] Streaming partial JSON parse silently produces `{}`
`crates/rustykrab-providers/src/anthropic.rs:568-571`. Truncated
streaming → empty tool-input object → tool runs with default args.
Log + drop the tool call.

### G-19 [H] Discarded in-flight tool calls on stream interrupt aren’t named
`crates/rustykrab-providers/src/anthropic.rs:610-614`. We lose them
silently; at least log the names so the agent can react.

### G-20 [H] `anthropic-version: 2023-06-01` is hardcoded
`crates/rustykrab-providers/src/anthropic.rs:318`. Today this version
is still accepted, but it’s aged and pins us. Read from env
(`ANTHROPIC_API_VERSION`, default to a current version).

### G-21 [M] 5xx responses are mapped to `ModelProvider` with no transient/permanent split
`crates/rustykrab-providers/src/anthropic.rs:256-265, 368`. Retry
logic decides separately, but callers can’t distinguish. Add a
`ModelTransient` variant.

### G-22 [M] `tool_input_bufs` is unbounded (cross-ref S-28)
Cap total bytes; on overrun, drop the in-flight tool call.

### G-23 [L] `SseContentBlock::Unknown` log doesn’t include the raw JSON
`crates/rustykrab-providers/src/anthropic.rs:750-752`. Include the
`block_type` string so unknown blocks (e.g. future Anthropic features)
can be diagnosed from a single log line.

### G-24 [L] `SseContentBlock::Text { .. }` matched but body discarded at `start`
`crates/rustykrab-providers/src/anthropic.rs:786-789`. Either drop
the arm or document why it’s a no-op for `content_block_start`.

### G-25 [L] `with_max_tokens` not validated against model
`crates/rustykrab-providers/src/anthropic.rs:69`. Out-of-range values
result in 400s; warn at construction.

---

## Providers — Ollama

### G-26 [H] Streaming chunk parse error kills the entire stream
`crates/rustykrab-providers/src/ollama.rs:817-819`. One malformed
NDJSON line → all accumulated text/tool calls lost. Skip the bad line
and require `done: true` to terminate.

### G-27 [H] Streaming tool calls only collected on the final chunk
`crates/rustykrab-providers/src/ollama.rs:829-837`. Multi-chunk tool
calls or tool calls in early chunks are dropped. Accumulate
incrementally as we do for text.

### G-28 [M] Context trim removes oldest messages with no surfaced ack
`crates/rustykrab-providers/src/ollama.rs:445-498`. Log per-trim, and
optionally insert a "trimmed N earlier turns" system message so the
agent loop knows.

### G-29 [M] `num_ctx` clamp emits only `debug!`
`crates/rustykrab-providers/src/ollama.rs:211-219`. Promote to `warn!`
when user-configured value exceeds the model’s native context.

### G-30 [M] Per-line `.trim()` before NDJSON parse can lose semantically meaningful whitespace
`crates/rustykrab-providers/src/ollama.rs:810`. Use `trim_end_matches('\n')`
or split on `\n` directly.

### G-31 [M] No "model not found / not loaded" classification
`crates/rustykrab-providers/src/ollama.rs:609-616, 648-649`. Pattern
match on response body and surface as a distinct error so the agent
doesn’t retry-storm.

### G-32 [L] `think` flag always sent
`crates/rustykrab-providers/src/ollama.rs:555`. Older Ollama servers
fail. Feature-detect or wrap in `Option`.

### G-33 [L] `with_temperature` not clamped
`crates/rustykrab-providers/src/ollama.rs:152-155`. Clamp to
`[0.0, 2.0]`.

### G-34 [L] `default_config_omits_num_ctx_when_env_unset` test mutates process env
`crates/rustykrab-providers/src/ollama.rs:1162-1191`. Multi-threaded
`cargo test` will race with siblings. Either serialize via a `Mutex`
fixture or use a process-isolated test harness.

