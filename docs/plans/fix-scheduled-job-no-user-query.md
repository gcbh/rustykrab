# Fix: scheduled jobs fail with Ollama `no user query found in messages`

**Status:** diagnosed, not yet fixed
**Diagnosed:** 2026-08-20
**Affected branch / worktree:** `claude/phase2-device-pairing` at
`/Users/geoff/projects/rustycrab/.claude/worktrees/ios-app-credentials`
(HEAD `3c5da9f`)

---

## 1. TL;DR for the implementer

Every scheduled (cron) job now fails at the model provider with:

```
Ollama API returned 500 Internal Server Error: {"error":"no user query found in messages"}
```

Ollama >= 0.32 rejects any `/api/chat` request whose `messages` array contains no
`user`-role entry. Two independent defects in this repo combine to produce exactly
that shape on scheduled runs:

1. **`task_queue::execute_cron_task` never appends a user turn** before invoking the
   agent. It passes the scheduled prompt as the `user_content` *string argument*, which
   is only used for profile routing and system-prompt construction — it never becomes a
   message.
2. **`OllamaProvider::trim_to_budget` has no floor protecting the last user turn.** It
   drops oldest-first from the front of the non-system region and will happily evict
   every user message in a long conversation.

Fix #1 is the primary fix. Fix #2 is defensive and should also be done — without it the
same failure can recur on any sufficiently long conversation, including interactive ones.

---

## 2. Evidence

### 2.1 The provider-side behaviour is real and reproducible

Ollama 0.32.14, running locally:

```bash
curl -s http://127.0.0.1:11434/api/chat \
  -H 'Content-Type: application/json' \
  -d '{"model":"qwen3.8:27b","stream":false,"messages":[{"role":"system","content":"You are a helpful assistant."}]}'
```

Returns:

```json
{"error":"no user query found in messages"}
```

A `messages` array with no `user` role is a hard 400/500. Older Ollama tolerated it,
which is why this is surfacing now rather than when the code was written.

### 2.2 The trimmer fires ~1 second before every failure

From `~/.rustykrab/rustykrab.log`:

```
2026-08-20T07:29:54.847963Z  WARN rustykrab_providers::ollama: trimmed conversation history to fit Ollama context window
                                  num_ctx=32768 budget=22528 estimated_tokens_before=23491 estimated_tokens_after=19293 messages_dropped=5
2026-08-20T07:29:55.079449Z  WARN rustykrab_providers::ollama: Ollama streaming API error ... "no user query found in messages"
```

The pairing is consistent across all 25 occurrences. `budget=22528` checks out exactly:
`RUSTYKRAB_NUM_CTX=32768` (set in the LaunchAgent plist) minus `num_predict=8192`
minus `SAFETY_OVERHEAD_TOKENS=2048`.

### 2.3 Scope of the failure

- 25 occurrences, first at `2026-08-20T07:29:55Z`, recurring on each scheduled run
  (07:30, 09:00, 14:30, 16:30 UTC), logged from `rustykrab_cli::task_queue` and
  `rustykrab_gateway::orchestrate`.
- Interactive paths (webchat `routes.rs`, Slack/Telegram `main.rs`) are **not** affected,
  because they push a real user message before running the agent.

### 2.4 Ruled out

- **Not an Ollama upgrade at the moment of breakage.** The ollama server process has run
  the same binary continuously since Aug 18 11:02 (`/Applications/Ollama.app`), and
  rustykrab succeeded against it at `2026-08-20T06:53:45Z`, 36 minutes before the first
  failure. The upgrade is a precondition, not the trigger.
- **Not a rebuild.** The deployed binary `RustyKrab.app/Contents/MacOS/rustykrab-cli`
  is dated Jun 6 and unchanged.
- **Not the master key / keychain.** `~/.rustykrab/rustykrab-error.log` is full of
  `keychain read failed ... User interaction is not allowed`, but that file has not been
  written since **Jun 6**. The master key loads cleanly on the current daemon.
- **Not model availability.** `qwen3.8:27b` is present and `/api/tags` returns 200.

### 2.5 Why the onset was gradual

Scheduled-job conversations are deliberately persistent and reused
(`resume_or_create_conversation`, and the comment at `task_queue.rs:423`: the
conversation is intentionally not deleted between runs). They accumulate
assistant/tool turns from every run, plus any user turns contributed by the bound
Telegram/Slack channel. Since scheduled runs add no user turn of their own, the only
user messages present are old ones near the front. Once the conversation crossed the
22528-token budget, `trim_to_budget` began dropping from the front — taking those stale
user turns with it and leaving a system+assistant/tool-only array.

---

## 3. Root cause in the source

### 3.1 The contract

`orchestrate.rs` only ever injects a **System** message and expects the caller to have
already appended the user turn. It says so itself at `crates/rustykrab-gateway/src/orchestrate.rs:299`:

```rust
// The inbound user message was pushed onto conv.messages by
// routes.rs before the runner was constructed, so it never goes
// through push_message.
```

`run_agent*`'s `user_content: &str` parameter is used for `state.profile_for(user_content)`
and system-prompt construction only. It is never turned into a message.

### 3.2 Call-site audit

| Call site | Pushes `Role::User` first? |
|---|---|
| `rustykrab-gateway/src/routes.rs:383` | yes (`routes.rs:371-379`) |
| `rustykrab-gateway/src/routes.rs:521` | yes (`routes.rs:461-469`) |
| `rustykrab-cli/src/main.rs:1991` (Slack/Telegram) | yes (`main.rs:1978`) |
| **`rustykrab-cli/src/task_queue.rs:302`** | **no — this is the bug** |

`grep -n "messages.push\|Role::User" crates/rustykrab-cli/src/task_queue.rs` returns nothing.

### 3.3 The trimmer has no floor

`crates/rustykrab-providers/src/ollama.rs:502-553`. The drop loop walks forward from the
first non-system message with no lower bound on what must survive:

```rust
let mut drop_end = system_count;
while current > budget && drop_end < trimmed.len() {
    current = current.saturating_sub(estimate_message_tokens(&trimmed[drop_end]));
    drop_end += 1;
}
// ... orphan-tool skip ...
trimmed.drain(system_count..drop_end);
```

Nothing guarantees a `user` message remains.

---

## 4. Fix 1 (primary) — append a user turn in `execute_cron_task`

**File:** `crates/rustykrab-cli/src/task_queue.rs`
**Location:** immediately before the `run_agent_streaming_with_options` call at line 302,
after `prompt` is built (lines 274-280).

`prompt` is currently passed as `&prompt` to the run call, so clone it for the message.

```rust
// Scheduled runs must contribute their own user turn. `run_agent_*` injects
// only the system prompt and relies on the caller to have appended the user
// message (see orchestrate.rs:299). Without one, a resumed job conversation
// reaches the provider carrying only system/assistant/tool messages, which
// Ollama >= 0.32 rejects with `{"error":"no user query found in messages"}`.
// Appending here also guarantees the newest message is a user turn, so the
// provider's oldest-first trimming can never strand the request without one.
conv.messages.push(Message {
    id: Uuid::new_v4(),
    role: Role::User,
    content: MessageContent::Text(prompt.clone()),
    created_at: Utc::now(),
    agent_version: Message::version_stamp(),
});
conv.updated_at = Utc::now();
```

Imports: `task_queue.rs:8` currently pulls `{Conversation, MessageContent}` from
`rustykrab_core::types`. Add `Message` and `Role`. `Uuid` and `Utc` are already in scope.

Mirror the exact field set used at `routes.rs:371-379` so persistence and version
stamping stay consistent.

### Persistence note
`prepare_agent` (`orchestrate.rs:298-308`) fires the memory callback for the last message
only when it is `Role::User`. Today that branch never runs for scheduled jobs. After this
fix it will, so the scheduled prompt gets persisted alongside the run. Confirm that is
desired and that it does not double-write against the post-run save in
`execute_cron_task`.

---

## 5. Fix 2 (defensive) — never trim away the last user turn

**File:** `crates/rustykrab-providers/src/ollama.rs`, in `trim_to_budget`, after the
orphan-tool skip loop and before `let dropped = drop_end - system_count;`.

```rust
// Never trim past the most recent user turn. A request whose message array
// carries no user role is rejected outright by Ollama with
// `no user query found in messages`, turning an over-long conversation into a
// hard failure instead of a merely degraded one. Clamping here can leave the
// request above budget; that is strictly preferable to a guaranteed 500.
if let Some(idx) = trimmed.iter().rposition(|m| m.role == "user") {
    drop_end = drop_end.min(idx);
}
```

Clamping to `idx` is safe with respect to the orphan-tool rule: the first surviving
message is then the user message itself, never a dangling `tool` result.

Two follow-ups for whoever implements this:

- `current` becomes stale after clamping (fewer messages are dropped than accounted for).
  Recompute it for the log line so `estimated_tokens_after` is not misleading.
- If the clamped request still exceeds budget, emit a distinct `warn!`/`error!` — that
  means a single turn is too large for the context window, which needs its own handling
  rather than silently shipping an over-budget request.

---

## 6. Tests

Add to `crates/rustykrab-providers/src/ollama.rs` unit tests:

1. `trim_to_budget` with a tiny budget over `[system, user, assistant, tool, assistant]`
   asserts at least one `role == "user"` survives.
2. Same, asserting the first surviving non-system message is not a `tool` (no orphan).
3. A conversation whose only user turn is the oldest message and whose budget forces
   maximal trimming — asserts that user turn is retained.

For `task_queue.rs`, assert that after the pre-run setup the last element of
`conv.messages` has `role == Role::User` and its text equals the built scheduled prompt.
The existing test helpers `empty_conv()` / `channel_conv()` (`task_queue.rs:700-720`)
are a reasonable starting point; a small refactor extracting the conversation-preparation
step from `execute_cron_task` would make this directly testable.

---

## 7. Verification after the fix

1. Rebuild and redeploy (see §8 — the deployed binary is stale).
2. Confirm the negative case still errors at the provider, proving the repro is valid:
   the `curl` in §2.1 should keep returning `no user query found in messages`.
3. Trigger a scheduled job (or wait for the next cron fire) and confirm:
   - no `no user query found in messages` in `~/.rustykrab/rustykrab.log`
   - `trimmed conversation history` warnings may still appear — that is fine and expected;
     what matters is that they are no longer followed by a provider error.
4. Regression-check an interactive turn through webchat and Telegram.

---

## 8. Build / deploy caveat — read before starting

There is a mismatch between the deployed binary and the checked-out source, and it needs
resolving before anyone concludes a fix "did not work":

- The **running daemon** is `RustyKrab.app/Contents/MacOS/rustykrab-cli`, dated **Jun 6**,
  launched by `~/Library/LaunchAgents/com.rustykrab.agent.plist`.
- It logs a `rustykrab_cli::task_queue` module that **does not exist in the main working
  tree** at `/Users/geoff/projects/rustycrab`. `task_queue.rs` exists only in the
  `ios-app-credentials` worktree (branch `claude/phase2-device-pairing`).

So the deployed build came from a branch that has since diverged from `main`. Confirm
which branch corresponds to the deployed artifact and which branch should receive this
fix before writing code. Restarting the daemon after redeploy:

```bash
launchctl kickstart -k gui/$(id -u)/com.rustykrab.agent
```

---

## 9. Related findings (out of scope for this fix)

These were found while diagnosing. They are not causes of this bug, but they are real.

1. **Gmail app password is in plaintext on disk.** `rustykrab_providers::ollama` logs raw
   API responses including full model reasoning. At `2026-08-19T21:17:53Z` in
   `~/.rustykrab/rustykrab.log`, the model recited both Gmail credentials verbatim.
   Rotate that app password and drop raw-body logging to `debug`.
2. **`TELEGRAM_BOT_TOKEN` is in plaintext** in the LaunchAgent plist, despite the secret
   registry existing precisely to avoid that.
3. **`gmail_email` credential was corrupted** — it held the literal string `gmail_email`
   instead of an address, which made `caldav` build
   `https://apidata.googleusercontent.com/caldav/v2/gmail_email/events/` and 401.
   Already fixed on 2026-08-20 via
   `rustykrab-cli keychain set gmail-email <address>`, which writes both the OS keychain
   and the encrypted store. Note `caldav.rs:110` (`calendar_id.unwrap_or(email)`) means a
   bad stored email silently becomes a bad URL — the `set_strict` validation added in this
   worktree is the right guard and is not in the deployed build.
4. **A stale error log is actively misleading.** `~/.rustykrab/rustykrab-error.log` holds
   1,693 `keychain read failed` errors last written Jun 6. Consider rotating or clearing it.
