# Dead code, stubs, and missing implementations

Rolled up from the four audits and from a direct grep pass:

```
grep -rn "todo!\|unimplemented!\|panic!(" crates/*/src --include="*.rs"
grep -rn "TODO\|FIXME\|XXX\|HACK" crates/*/src --include="*.rs"
grep -rn "let _ =" crates/*/src --include="*.rs"
```

Notable: there are **no** `todo!()`, `unimplemented!()`, or
`FIXME/XXX/HACK` markers anywhere in the workspace. The single `panic!`
is in a test (`crates/rustykrab-agent/src/runner.rs:2435`).

The dead-code surface is therefore quite small. What remains:

---

## Real stubs

### D-1  `WebChat::receive` always returns an error
`crates/rustykrab-channels/src/webchat.rs:44-50`. Cross-ref §C-8 / §G-1.
The trait shape can’t accommodate `&mut self` semantics so the impl
hardcodes `Err(...)`. Whatever path expects `Channel::receive` to
work for WebChat is silently broken.

### D-2  `agent::sandbox::ProcessSandbox::execute` is a no-op
`crates/rustykrab-agent/src/sandbox.rs:130-162`. Validates the policy,
applies a timeout, then `Ok(args)`. Tests pass because they assert on
the unchanged input. Cross-ref §A-19.

---

## Unused fields / arguments

### D-3  `HarnessRouter::_classifier` field
`crates/rustykrab-agent/src/router.rs:16-18`. Stored, never read.

### D-4  `_completion_tokens` parameter in `classify_response`
`crates/rustykrab-agent/src/runner.rs:166`. Refactor leftover.

### D-5  `JsonRpcResponse::jsonrpc`, `JsonRpcError::data` annotated `#[allow(dead_code)]`
`crates/rustykrab-channels/src/mcp.rs:83,94`. Actually populated by
serde — drop the `allow` and add a one-line comment.

### D-6  `SkillRegistry` has both an in-process and a persistent path that aren’t obviously wired
`crates/rustykrab-skills/src/skill.rs`. Worth one tracing pass to be
sure both paths are reachable from `cli`.

---

## Swallowed errors (`let _ = …`)

The repo uses this pattern intentionally for cleanup. Audit each call
site for "is it actually OK to ignore?":

| File:line | Verdict |
| --- | --- |
| `agent/src/runner.rs:2147` | test fixture, fine |
| `channels/src/mcp.rs:182,330,339,360` | shutdown / cancel — fine |
| `channels/src/telegram.rs:453,476` | reply send failure → user sees nothing. **Fix.** §G-9 |
| `cli/src/task_queue.rs:157,173` | DB record_run failure dropped → metrics gap. Promote to `error!` |
| `cli/src/main.rs:321` | "agent_id" file write fails silently → next start picks a different id. **Should warn.** |
| `cli/src/main.rs:1208,1210,1351` | credential persistence failures hidden — see §A-25 |
| `gateway/src/routes.rs:268,275` | SSE close + panic post — fine *after* §G-3 fix |
| `memory/src/writer.rs:210,211` | invalidate / record_access best-effort — fine |
| `store/src/keychain.rs:101,120` | delete-credential, fine |
| `store/src/registry.rs:114,116,124,133` | persistence failures hidden — see §A-25 |
| `tools/src/gmail.rs:90,147,196,338,454,492,531,571` | IMAP `session.logout()` cleanup — fine |
| `tools/src/exec.rs:282 …` (timeout drop on `output()`) | n/a |

---

## Unused / partial features

- **Group routing in Signal** (§G-13). `send_to_group` exists but
  group-id extraction from inbound Signal envelopes isn’t tested or
  threaded back through the agent. Effective dead code.
- **MCP tool cache invalidation** (§G-14). The cache is one-way; no
  refresh path and no TTL — the *implementation* is fine, the
  *design* is half-done.
- **Constant-time comparison utility** (`core/crypto.rs`) is used
  twice (auth bearer + Telegram secret). Reasonable.

---

## What I did NOT find

For audit completeness, here is what I explicitly searched for and
came up empty on (a positive sign):

- `unsafe` blocks outside libc-required (`unshare`, `prctl`,
  `setrlimit`) and a single test env-var fixture in
  `providers/src/ollama.rs` (only valid in tests).
- Unbounded `tokio::spawn` without join handles. Spawned tasks have
  associated handles in `infra_handles` and `task_queue`.
- `format!`-based SQL is limited to two known-safe sites
  (`memory/storage.rs:636` and `store/jobs.rs:130`) — see §S-17/§S-18.
- Anything that disables TLS verification beyond the Obsidian tool
  (§C-4).
- Plaintext secret storage at rest (encryption is on, see
  `store/src/keychain.rs`).
