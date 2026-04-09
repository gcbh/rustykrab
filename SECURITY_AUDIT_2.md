# OpenClaw Security Audit Report (Comprehensive)

**Date:** 2026-04-09
**Auditor:** Automated multi-domain security analysis (9 parallel audit agents)
**Scope:** Full codebase — 18K lines across 9 Rust crates
**Previous Audit:** 2026-04-05 (`SECURITY_ANALYSIS.md`)
**Overall Risk:** CRITICAL — Not production-ready without remediation

---

## Methodology

Nine specialized audit agents ran in parallel, each covering a distinct attack surface:

| # | Domain | Files Audited |
|---|--------|---------------|
| 1 | Authentication & Access Control | auth.rs, rate_limit.rs, origin.rs, routes.rs, state.rs, lib.rs |
| 2 | Command Injection & Sandbox Escapes | exec.rs, code_execution.rs, process.rs, apply_patch.rs, browser.rs, sandbox.rs, harness.rs, runner.rs |
| 3 | Cryptographic Implementation | secret.rs, keychain.rs, auth.rs, verify.rs, credential_read/write.rs, security.rs |
| 4 | Network Security & SSRF | http_request.rs, http_session.rs, web_fetch.rs, web_search.rs, gmail.rs, anthropic.rs, ollama.rs, webhooks, channels |
| 5 | Data Storage & Secrets Management | conversation.rs, memory.rs, knowledge_graph.rs, secret.rs, keychain.rs, logging.rs |
| 6 | Prompt Injection & Agent Safety | orchestrator/*, rlm/*, router.rs, trace.rs, sanitize.rs, tool.rs, capability.rs |
| 7 | File I/O & Path Traversal | read.rs, write.rs, edit.rs, apply_patch.rs, pdf.rs, image.rs, canvas.rs, tts.rs, skills/loader.rs |
| 8 | Dependency Supply Chain | Cargo.toml, Cargo.lock, all crate-level Cargo.toml files |
| 9 | Session, Channel & Skill Security | session.rs, channels/*, session tools, subagents.rs, verify.rs, cron.rs, message.rs |

---

## Executive Summary

| Severity | Previous Audit (Apr 5) | This Audit (Apr 9) | Delta |
|----------|----------------------|-------------------|-------|
| CRITICAL | 12 | 21 | +9 new |
| HIGH | 10 | 19 | +9 new |
| MEDIUM | 12 | 16 | +4 new |
| LOW | 5 | 6 | +1 new |
| **Total** | **39** | **62** | **+23 new** |

**Key Changes Since Previous Audit:**

The previous audit prompted several improvements:
- Secret encryption upgraded from XOR to AES-256-GCM with Argon2 KDF
- New `security.rs` module with `validate_path()` and `validate_url()` for SSRF/path traversal prevention
- Security headers (X-Frame-Options, CSP, etc.) added to all HTTP responses
- Webhook secret validation now uses constant-time comparison

However, many critical issues persist (sandbox no-op, no conversation ownership, permissive capabilities, command injection), and this deeper audit uncovered 23 additional vulnerabilities — including a use-after-free via `unsafe { transmute }`, workspace boundary non-enforcement, prompt injection vectors, unencrypted conversation storage, and significant supply chain risks.

---

## Remediation Status (Previous Findings)

### Fixed or Improved

| Previous ID | Issue | Status |
|-------------|-------|--------|
| H1 | XOR encryption without authentication | **FIXED** — Now AES-256-GCM with AAD |
| H2 | Weak key derivation (no KDF) | **IMPROVED** — Now Argon2id (but weak defaults, see C15) |
| H6 | Webhook secret timing-vulnerable comparison | **FIXED** — Now constant-time |
| C8-C10 | Path traversal (no validation) | **IMPROVED** — `validate_path()` exists (but incomplete, see C10-C12) |
| C11 | SSRF (no URL validation) | **IMPROVED** — `validate_url()` with private IP blocking (but bypassable, see H8) |
| M9 | Missing security headers | **FIXED** — X-Frame-Options, CSP, etc. added |
| L1 | Constant-time comparison leaks length | **FIXED** — Now compares full max length |

### Still Present (Unresolved)

| Previous ID | Issue | Current Status |
|-------------|-------|----------------|
| C2 | No conversation ownership | **UNRESOLVED** — See C2 below |
| C3 | Session/capability model never enforced | **UNRESOLVED** — `for_tools_permissive()` still used, see C3 |
| C4 | Sandbox is non-functional placeholder | **UNRESOLVED** — See C4 |
| C5-C6 | Command injection via `sh -c` | **UNRESOLVED** — See C5 |
| C7 | Arbitrary Python code execution | **UNRESOLVED** — See C6 |
| H3 | Master key in environment variable | **UNRESOLVED** — See H4 |
| H7 | Unbounded agent recursion | **UNRESOLVED** — See H13 |

---

## CRITICAL Findings (21)

### C1. Unsafe `transmute` — Use-After-Free in Streaming Handler
**File:** `crates/openclaw-gateway/src/orchestrate.rs:262-263`
**Domain:** Memory Safety
**New in this audit.**

```rust
let event_fn: &'static (dyn Fn(AgentEvent) + Send + Sync) =
    unsafe { std::mem::transmute(heartbeat_event) };
```

A non-`'static` reference is transmuted to `'static` and passed to `tokio::spawn`. If the client disconnects before the spawned heartbeat task completes, the task dereferences freed memory. This is **undefined behavior** — potential crash or arbitrary code execution.

**Fix:** Replace with `Arc<dyn Fn(AgentEvent) + Send + Sync>`.

---

### C2. No Conversation Ownership or Multi-Tenancy
**Files:** `crates/openclaw-gateway/src/routes.rs:67-100`, `crates/openclaw-store/src/conversation.rs`
**Domain:** Authorization
**Persists from previous audit (previously C2).**

All conversation endpoints accept arbitrary UUIDs with no ownership verification. `list_conversations()` returns ALL conversation IDs globally. Any authenticated user can read, modify, or delete any conversation.

```rust
async fn get_conversation(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,  // User-supplied, no ownership check
) -> Result<Json<Conversation>, StatusCode> {
    state.store.conversations().get(id) // Returns any conversation
```

**Impact:** Complete lateral movement between users in multi-user deployments.

---

### C3. Capability Model Universally Permissive
**File:** `crates/openclaw-gateway/src/orchestrate.rs:125-126`
**Domain:** Authorization
**Persists from previous audit (previously C3).**

Every session gets `CapabilitySet::for_tools_permissive()` which grants FileRead, FileWrite, ShellExec, HttpRequest to ALL conversations regardless of trust level. The safer `for_tools()` exists but is never used.

---

### C4. Sandbox Is a Non-Functional Placeholder
**File:** `crates/openclaw-agent/src/sandbox.rs:88-126`
**Domain:** Isolation
**Persists from previous audit (previously C4).**

`ProcessSandbox::execute()` logs policy constraints but returns args unchanged. No seccomp-bpf, no namespaces, no resource limits. `SandboxPolicy` fields (`allow_fs_write`, `allow_net`, `allow_spawn`, `max_memory_bytes`) are completely ignored.

---

### C5. Command Injection via `sh -c` in Exec Tool
**File:** `crates/openclaw-tools/src/exec.rs:164-171`
**Domain:** Injection
**Persists from previous audit (previously C5). Allowlist added but bypassable.**

Commands are passed to `sh -c`. The allowlist validation (`validate_command`) has bypass vectors:
- Command substitution: `EVIL=$(whoami) echo test` — validation sees `echo` (allowed) but shell expands `$(whoami)`
- Variable assignments with `=` are skipped: `FOO=$(/bin/rm -rf /) echo`
- Glob expansion and backtick substitution not blocked

---

### C6. Arbitrary Python Code Execution Without Sandboxing
**File:** `crates/openclaw-tools/src/code_execution.rs:85-157`
**Domain:** Code Execution
**Persists from previous audit (previously C7).**

User-supplied Python code is written to `/tmp/openclaw_sandbox/exec_<uuid>.py` and executed with `env_clear()` but: no seccomp, no resource limits (CPU/memory/fds), no filesystem isolation. Python can `os.system()`, `subprocess.Popen()`, `os.fork()`, access the entire filesystem, and open network connections.

---

### C7. Webhook Authentication Bypass — Optional Secrets
**Files:** `crates/openclaw-channels/src/telegram.rs:289-296`, `signal.rs:204-211`
**Domain:** Authentication

When `webhook_secret` is `None` (the default), ALL webhook requests are accepted without authentication:

```rust
if let Some(ref secret) = self.webhook_secret {
    // Only validates IF secret is configured
}
// If None, falls through with no auth at all
```

Combined with empty `allowed_chats`/`allowed_numbers` defaulting to accept-all, an attacker can POST arbitrary messages to `/webhook/telegram` or `/webhook/signal` and have them processed by the agent.

---

### C8. Unencrypted Conversation Storage at Rest
**File:** `crates/openclaw-store/src/conversation.rs:32-41`
**Domain:** Data Protection
**New in this audit.**

Conversations (containing complete chat histories, personal data, credentials discussed) are stored as plaintext JSON in Sled. Only `SecretStore` uses AES-256-GCM; conversations and memories have zero encryption.

---

### C9. Unencrypted Memory Storage at Rest
**File:** `crates/openclaw-store/src/memory.rs:48-55`
**Domain:** Data Protection
**New in this audit.**

Memory entries (agent-extracted facts like health information, financial status, personal details) are stored as plaintext JSON. An attacker with filesystem access can reconstruct complete user profiles.

---

### C10. Path Validation Does Not Enforce Workspace Boundary
**File:** `crates/openclaw-tools/src/security.rs:11-17, 26-90`
**Domain:** Path Traversal
**New in this audit.**

`workspace_root()` is defined but **never called** by `validate_path()`. The function only blocks a small hardcoded list (`/etc/shadow`, `/etc/sudoers`, `/root/.ssh`, `/proc`, `/sys`, `/dev`). Absolute paths like `/home/user/.ssh/id_rsa`, `/home/user/.aws/credentials`, or any file outside blocked prefixes are freely accessible.

---

### C11. Arbitrary File Write Outside Workspace
**Files:** `crates/openclaw-tools/src/write.rs:57-93`, `edit.rs:65-123`
**Domain:** Path Traversal
**New in this audit.**

Same as C10 — `write` and `edit` tools use `validate_path()` which doesn't enforce workspace boundaries. An agent can write SSH authorized_keys, cron jobs, or system files.

---

### C12. Symlink Escape in New File Creation Path
**File:** `crates/openclaw-tools/src/security.rs:71-89`
**Domain:** Path Traversal
**New in this audit.**

For new files: if the parent directory exists but contains a symlink, the file creation path follows the symlink without workspace boundary verification. Only the parent is canonicalized, not the full target path.

---

### C13. Indirect Prompt Injection — System Context Concatenation
**File:** `crates/openclaw-agent/src/orchestrator/decomposer.rs:82-85`
**Domain:** Prompt Injection
**New in this audit.**

User-provided context is concatenated directly into system prompts with weak delimiters (`\n\n---\n\n`):

```rust
let system_prompt = match context {
    Some(ctx) => format!("{ctx}\n\n---\n\n{decompose_instructions}"),
    None => decompose_instructions,
};
```

An attacker can inject override instructions that appear before the real instructions in the system prompt.

---

### C14. Indirect Prompt Injection — Executor Sub-task Context
**File:** `crates/openclaw-agent/src/orchestrator/executor.rs:140-142`
**Domain:** Prompt Injection
**New in this audit.**

Sub-task results from previous tasks are concatenated into system prompts without encapsulation. A malicious sub-task result can inject instructions that poison all downstream tasks.

---

### C15. Unvalidated Tool Arguments from LLM
**File:** `crates/openclaw-agent/src/runner.rs:914-927`
**Domain:** Tool Safety
**New in this audit.**

Tool argument validation only checks that required parameters are **present**, not their types, ranges, or formats. LLM-generated arguments pass through with no schema validation — enabling path traversal, oversized reads, and injection through tool parameters.

---

### C16. Recursive Call Marker Injection
**File:** `crates/openclaw-agent/src/rlm/recursive_call.rs:280-301`
**Domain:** Prompt Injection
**New in this audit.**

`[SUB_CALL: ...]` markers are extracted from model output and fed back as new prompts without sanitization. Sub-call results are substituted back via `String::replace()` without escaping — if a result contains `[SUB_CALL: ...]`, it creates nested injection.

---

### C17. Browser JavaScript Injection
**File:** `crates/openclaw-tools/src/browser.rs:549-567, 622-662`
**Domain:** Injection
**New in this audit.**

The browser tool constructs JavaScript via string interpolation with incomplete escaping (only single quotes escaped in some actions, not double quotes, backticks, or closing parens). The `evaluate` action executes arbitrary JS with no sanitization at all.

---

### C18. Session Tools Allow Arbitrary Cross-Session Access
**Files:** `crates/openclaw-tools/src/sessions_send.rs:52-60`, `sessions_history.rs:48-54`, `sessions_list.rs:43-45`
**Domain:** Authorization
**New in this audit.**

Session management tools delegate to `SessionManager` backend with no ownership validation. Any agent can read/write/enumerate any session by guessing or learning session IDs.

---

### C19. `sled 0.34.7` — Unmaintained Database with Data Corruption Risks
**File:** `Cargo.toml:44`, `Cargo.lock`
**Domain:** Supply Chain
**Upgraded from L5 in previous audit.**

sled never reached v1.0, has known data corruption issues on unclean shutdown, and has had minimal maintenance since 2022-2023. It is the single point of failure for ALL persistent data. No security patches, no CVE tracking, no vulnerability response process.

---

### C20. Dual TLS Stack via `native-tls` + OpenSSL
**File:** `Cargo.toml:75`
**Domain:** Supply Chain
**New in this audit.**

`imap` and `lettre` (with `tokio1-native-tls` feature) pull in `native-tls 0.2.18` → `openssl-sys 0.9.112`, creating a parallel TLS stack alongside `rustls`. System OpenSSL version is outside project control. `lettre` supports `tokio1-rustls` but the wrong feature flag is selected.

---

### C21. Weak Argon2 Default Parameters
**File:** `crates/openclaw-store/src/secret.rs:165-171`
**Domain:** Cryptography
**New in this audit.**

`Argon2::default()` uses minimal parameters (4 MiB memory, 3 iterations, 1 lane). Modern GPUs can compute ~100K iterations/second at these settings. NIST recommends 64+ MiB memory and 10+ iterations for key derivation protecting sensitive data.

---

## HIGH Findings (19)

### H1. Derived Encryption Keys Not Zeroized
**File:** `crates/openclaw-store/src/secret.rs:165-171`
**Domain:** Cryptography

`derive_key()` returns a raw `[u8; 32]` that persists on the stack after use. `master_key` in `SecretStore` is `Vec<u8>` not wrapped in `Zeroizing<>`. A memory dump (via `/proc/self/mem`, core dump, or swap) can recover encryption keys.

---

### H2. Constant-Time Comparison Leaks Token Length
**Files:** `crates/openclaw-gateway/src/auth.rs:66-77`, `crates/openclaw-channels/src/telegram.rs:524-535`, `signal.rs:332-343`
**Domain:** Cryptography

The XOR loop iterates to `max(a.len(), b.len())` which is good, but the initial `(a_bytes.len() != b_bytes.len()) as u8` is a data-dependent branch that leaks length difference through timing. Should use the `subtle` crate's `ConstantTimeEq`.

---

### H3. Auth Token Printed to stdout on Rotation
**File:** `crates/openclaw-gateway/src/routes.rs:52`
**Domain:** Information Disclosure

`println!("\n  New OPENCLAW_AUTH_TOKEN={new_token}\n")` — the new auth token is printed to stdout on every `/api/logout` call. In containerized deployments, CI/CD, or shared systems, stdout is often captured in logs accessible to unauthorized parties.

---

### H4. Master Key Exposed via Environment Variable
**File:** `crates/openclaw-store/src/keychain.rs:133-141`
**Domain:** Secret Management
**Persists from previous audit (previously H3).**

`OPENCLAW_MASTER_KEY` is readable via `/proc/$PID/environ`, inherited by all child processes (including Python code execution), and often logged in CI/CD pipelines.

---

### H5. Context Window Poisoning via Refinement Loop
**File:** `crates/openclaw-agent/src/orchestrator/refiner.rs:95-98, 133-136`
**Domain:** Prompt Injection
**New in this audit.**

No length validation on `request` or `response` before inclusion in prompts. A 50KB response consumes most of the context budget, pushing safety instructions out of the window. XML delimiter breakout also possible if input contains `</user_input>`.

---

### H6. Synthesis Results Without Length Bounds
**File:** `crates/openclaw-agent/src/orchestrator/synthesizer.rs:73-75`
**Domain:** Prompt Injection
**New in this audit.**

All sub-task outputs concatenated without size limits and injected into synthesis prompts. A single sub-task returning 100KB causes context overflow.

---

### H7. Missing Validation of Decomposed Task Descriptions
**File:** `crates/openclaw-agent/src/orchestrator/decomposer.rs:123-160`
**Domain:** Prompt Injection
**New in this audit.**

Task descriptions from LLM decomposer output are used directly as new prompts for sub-tasks without sanitization. A compromised model can generate malicious task descriptions.

---

### H8. SSRF Bypass via Redirect Chains
**Files:** `crates/openclaw-tools/src/http_request.rs:21`, `http_session.rs:39`, `web_fetch.rs:25`
**Domain:** Network Security
**New in this audit.**

Redirect policy allows 10 hops. SSRF validation occurs only on the initial URL. A redirect chain from `attacker.com → attacker.com → ... → localhost:8080/admin` bypasses all SSRF protection.

---

### H9. Header Injection in HTTP Session Tool
**File:** `crates/openclaw-tools/src/http_session.rs:152-158`
**Domain:** Injection
**New in this audit.**

Custom headers accepted from user input with no validation. Attacker can inject `Host`, `Transfer-Encoding`, `Content-Length`, or override `Authorization` headers.

---

### H10. Skill Loader Does Not Verify Signatures
**File:** `crates/openclaw-skills/src/loader.rs:59-71`
**Domain:** Supply Chain
**New in this audit.**

`load_single_skill()` reads SKILL.md from disk without calling the signature verification infrastructure in `verify.rs`. An attacker who can modify files in the skills directory can inject arbitrary skill prompts.

---

### H11. Cross-Conversation Memory Search Leakage
**Files:** `crates/openclaw-tools/src/memory_search.rs:56-76`, `memory_backend.rs:7`
**Domain:** Data Isolation
**New in this audit.**

The `memory_search` tool's backend trait doesn't include a `conversation_id` parameter. Depending on implementation, searches may return memories from ALL conversations, leaking data across tenant boundaries.

---

### H12. Empty Channel Allowlist Defaults to Accept-All
**Files:** `crates/openclaw-channels/src/telegram.rs:348-356`, `signal.rs:232-239`
**Domain:** Authentication

If `allowed_chats` or `allowed_numbers` is empty (the default), the allowlist check is skipped entirely. Messages from ANY Telegram user or Signal number are accepted and processed with full tool access.

---

### H13. No Resource Limits on Python Execution
**File:** `crates/openclaw-tools/src/code_execution.rs:125-133`
**Domain:** Denial of Service
**Persists from previous audit (part of C7).**

No `setrlimit()` calls. Python can allocate unlimited memory (`[0] * 10**9`), fork-bomb (`while True: os.fork()`), or fill disk. No CPU time limit.

---

### H14. TOCTOU Race Condition in File Operations
**Files:** `crates/openclaw-tools/src/read.rs:70-88`, `write.rs:66-86`
**Domain:** Path Traversal
**New in this audit.**

Path validation and file I/O are separate operations. Between `validate_path()` and `tokio::fs::read_to_string()`, the file can be replaced with a symlink to a sensitive location.

---

### H15. Sled Database File Permissions Not Controlled
**File:** `crates/openclaw-store/src/lib.rs:41`, `crates/openclaw-cli/src/main.rs:27`
**Domain:** Data Protection
**New in this audit.**

`sled::open(path)` and `create_dir_all()` use default permissions (0o755 on Unix). Other users on shared systems can read the database containing unencrypted conversations, memories, and encrypted secrets.

---

### H16. Gmail Attachment Path Traversal
**File:** `crates/openclaw-tools/src/gmail.rs:345-350`
**Domain:** Path Traversal
**New in this audit.**

Email attachment filenames are used directly in file paths without sanitization. A filename like `../../../../../../tmp/malicious.sh` writes outside the intended download directory.

---

### H17. Missing Audit Trail for Tool Invocations
**File:** `crates/openclaw-agent/src/trace.rs:92-112`
**Domain:** Forensics
**New in this audit.**

Tool invocation traces record tool names but not arguments, results, session IDs, or timestamps. Traces are in-memory only with no persistence. No forensic capability for security incidents.

---

### H18. Cron Backend Has No Task Validation
**File:** `crates/openclaw-tools/src/cron.rs:60-93`
**Domain:** Injection
**New in this audit.**

The cron tool accepts arbitrary schedule expressions and task strings without validation. If the backend executes tasks as commands, this enables scheduled arbitrary command execution.

---

### H19. No Conversation Cleanup on Deletion
**Files:** `crates/openclaw-store/src/conversation.rs:67-72`, `memory.rs:134-151`
**Domain:** Data Protection
**New in this audit.**

Conversation deletion doesn't clean up associated memories. Sled's WAL retains deleted entries until compaction. No secure overwrite — deleted data remains recoverable from disk. GDPR "right to be forgotten" non-compliance.

---

## MEDIUM Findings (16)

| ID | Issue | File(s) | Domain |
|----|-------|---------|--------|
| M1 | Rate limiting only on `/api/` — webhooks exempt | `rate_limit.rs:109-112` | DoS |
| M2 | Rate limiter runs AFTER auth middleware (wrong order) | `lib.rs:48-59` | DoS |
| M3 | Origin validation uses `starts_with()` — `http://127.0.0.1.attacker.com` bypasses | `origin.rs:28-34` | CSRF |
| M4 | Ed25519 skill verification lacks key pinning (any trusted key accepted) | `verify.rs:36-49` | Supply Chain |
| M5 | Unsafe URL decoding in web search — null bytes, invalid UTF-8 | `web_search.rs:274-295` | Injection |
| M6 | `javascript:` protocol not filtered in href extraction | `sanitize.rs:176-204` | XSS |
| M7 | Response size limits inconsistent across HTTP tools (5MB/100KB/50KB/none) | `http_request.rs`, `http_session.rs`, `web_fetch.rs`, `web_search.rs` | DoS |
| M8 | DNS rebinding window between validation and request | `security.rs:139-150` | SSRF |
| M9 | Fact/entity storage without sanitization (stored XSS risk) | `memory_save.rs:59-80`, `knowledge_graph.rs:46-60` | XSS |
| M10 | Temp file permissions world-readable (0o666) in code execution | `code_execution.rs:94-107` | Info Disclosure |
| M11 | `HOME=/tmp` in Python sandbox enables cross-session side channels | `code_execution.rs:127-131` | Info Disclosure |
| M12 | Symlink following in skill directory loading | `loader.rs:15-57` | Path Traversal |
| M13 | Session expiry not checked during tool execution | `runner.rs:140-165` | Authorization |
| M14 | No rate limiting on message/channel tools | `message.rs:55-75` | DoS |
| M15 | Master key resolution strategy logged (leaks config to attackers) | `keychain.rs:135, 145, 150` | Info Disclosure |
| M16 | Missing timeouts on Telegram/Signal HTTP clients | `telegram.rs:57`, `signal.rs:52` | DoS |

---

## LOW Findings (6)

| ID | Issue | File(s) |
|----|-------|---------|
| L1 | Webhook URL not validated before registration with Signal/Telegram | `signal.rs:292-322`, `telegram.rs:302-326` |
| L2 | Knowledge graph search performs full-table scan with no pagination | `knowledge_graph.rs:105-122` |
| L3 | Browser select action has incomplete JS escaping | `browser.rs:622-662` |
| L4 | Loose JSON extraction in decomposer (first `{` to last `}`) | `decomposer.rs:164-186` |
| L5 | No format validation on session IDs (accepts any string) | `sessions_send.rs:52-60` |
| L6 | Incomplete redirect URL parsing in web search | `web_search.rs:258-271` |

---

## Dependency Supply Chain Audit

| Dependency | Version | Risk | Issue |
|------------|---------|------|-------|
| **sled** | 0.34.7 | CRITICAL | Unmaintained, pre-1.0, known data corruption, no CVE tracking |
| **native-tls** | 0.2.18 | CRITICAL | Forces OpenSSL on Linux; creates dual TLS stack with rustls |
| **chromiumoxide** | 0.9.1 | CRITICAL | Unmaintained (~2023); pulls second `reqwest 0.13.2` |
| **openssl-sys** | 0.9.112 | CRITICAL | System OpenSSL dependency; no version control |
| **reqwest** | 0.12.28 + 0.13.2 | HIGH | Dual versions compiled (from chromiumoxide) |
| **imap** | 2.4.1 | HIGH | Forces native-tls; no rustls support available |
| **lettre** | 0.11.21 | HIGH | Using `tokio1-native-tls` instead of available `tokio1-rustls` |
| **parking_lot** | 0.11.2 + 0.12.5 | HIGH | Dual versions (sled pins 0.11.x); potential deadlock |
| **axum** | 0.8.8 | OK | Well-maintained; monitor HTTP/2 advisories |
| **ed25519-dalek** | 2.1.1 | OK | Should add `zeroize` feature flag |
| **tokio** | 1.50.0 | OK | Well-maintained, current |
| **rustls** | 0.23.37 | EXCELLENT | Modern, memory-safe TLS — should be used more broadly |

**Total packages in Cargo.lock:** ~335
**Dual-compiled crates:** reqwest, parking_lot, base64
**No `cargo audit` or `cargo deny` integration detected.**

---

## Positive Security Findings

Despite the volume of findings, the codebase demonstrates good security awareness in several areas:

1. **AES-256-GCM with AAD** — `secret.rs` uses authenticated encryption with the key name as associated data, binding ciphertext to its key
2. **SSRF protection** — `validate_url()` blocks private IPs, cloud metadata, and performs DNS resolution checks
3. **Path traversal defense** — `validate_path()` rejects `..` components and canonicalizes existing paths against blocked prefixes
4. **Constant-time token comparison** — compares all bytes up to max length to prevent early-exit timing leaks
5. **Security headers** — X-Frame-Options, CSP, X-Content-Type-Options on all HTTP responses
6. **rustls for HTTP clients** — `reqwest` configured with `rustls-tls` instead of native OpenSSL
7. **Per-secret salts and nonces** — each encrypted secret gets unique random salt and nonce
8. **Loopback-only binding** — server binds to `127.0.0.1:3000`, never `0.0.0.0`
9. **Ed25519 skill verification infrastructure** — signature verification exists (just not wired into loader)
10. **Structured tracing** — comprehensive logging throughout the codebase
11. **Token rotation** — `/api/logout` generates new tokens via cryptographically secure RNG

---

## Remediation Roadmap

### Phase 1: Immediate (Before Any Deployment)

| Priority | Finding | Action |
|----------|---------|--------|
| P0 | C1 | Remove `unsafe { transmute }` — use `Arc<dyn Fn>` |
| P0 | C2, C18 | Implement per-user ownership on conversations and sessions |
| P0 | C7, H12 | Make webhook secrets mandatory; flip allowlist to default-deny |
| P0 | C10-C12 | Enforce workspace boundary in `validate_path()` |
| P0 | C5 | Replace `sh -c` with direct argument arrays or strict allowlist that blocks `$()` and backticks |
| P0 | C6, H13 | Add seccomp/resource limits to Python execution; use per-session temp dirs with 0o700 |
| P0 | C15 | Add full JSON Schema validation for tool arguments |
| P0 | C21 | Increase Argon2 parameters to 64 MiB / 10 iterations / 4 lanes |

### Phase 2: Short-Term (1-2 Weeks)

| Priority | Finding | Action |
|----------|---------|--------|
| P1 | C3 | Use `CapabilitySet::for_tools()` (safe default); require explicit escalation |
| P1 | C4 | Implement real sandbox (seccomp-bpf / namespaces) |
| P1 | C8-C9, H15 | Encrypt conversations/memories at rest; set DB permissions to 0o700 |
| P1 | C13-C14, H5-H7 | Use XML-delimited prompt sections with length bounds |
| P1 | C16 | Escape `[SUB_CALL:]` markers in substituted results |
| P1 | H1 | Wrap derived keys in `Zeroizing<[u8; 32]>` |
| P1 | H4 | Remove env var after reading; migrate to OS keychain |
| P1 | H8 | Reduce redirect limit to 1; validate redirect destinations |
| P1 | H9 | Implement header whitelist for HTTP session tool |
| P1 | H10 | Add signature verification call in skill loader |
| P1 | H3 | Remove token printing from stdout; return via secure channel only |

### Phase 3: Medium-Term (1-2 Months)

| Priority | Finding | Action |
|----------|---------|--------|
| P2 | C19 | Plan migration from sled to SQLite/RocksDB |
| P2 | C20 | Change lettre feature to `tokio1-rustls`; evaluate `async-imap` |
| P2 | H11 | Add `conversation_id` to memory backend trait |
| P2 | H17 | Implement persistent audit logging with session tracking |
| P2 | H19 | Cascade-delete memories on conversation deletion; secure overwrite |
| P2 | M2 | Reorder middleware so rate limiting is outermost layer |
| P2 | M3 | Fix origin validation to use exact matching, not `starts_with()` |
| P2 | -- | Add `cargo audit` and `cargo deny` to CI/CD pipeline |
| P2 | -- | Add fuzz testing for webhook parsers, path validation, prompt construction |

### Phase 4: Long-Term (2-3 Months)

| Priority | Finding | Action |
|----------|---------|--------|
| P3 | C4 | Consider WASM-based sandboxing for tool execution |
| P3 | C17 | Replace JS string interpolation with CDP typed input methods |
| P3 | -- | Remove `chromiumoxide` or maintain local fork |
| P3 | -- | Implement per-tool capability declarations on the `Tool` trait |
| P3 | -- | Add property-based testing for cryptographic operations |
| P3 | -- | Implement connection limits and WebSocket rate limiting |

---

## Conclusion

This audit identified **62 total findings** (21 CRITICAL, 19 HIGH, 16 MEDIUM, 6 LOW) — a significant increase from the 39 found in the April 5 audit. While several improvements were made (AES-256-GCM encryption, SSRF protection, path validation), the deeper analysis revealed that many of these mitigations are incomplete or bypassable.

**The three most urgent issues are:**

1. **`unsafe { transmute }` use-after-free** (C1) — the only memory safety violation in the codebase; trivial to fix with `Arc`
2. **No workspace boundary enforcement** (C10-C12) — `validate_path()` exists but doesn't confine operations to the workspace, making all file tools effectively unrestricted
3. **Webhook authentication bypass** (C7) — default configuration accepts unauthenticated messages from any sender

**The codebase is not production-ready** without addressing at minimum the Phase 1 items. The development team has demonstrated good security instincts (the crypto upgrade and SSRF protection are solid foundations), but the gap between security infrastructure that *exists* and security infrastructure that is *enforced* remains the primary systemic risk.

