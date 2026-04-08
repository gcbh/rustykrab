# RustyKrab Security Analysis Report

**Date:** 2026-04-05
**Scope:** Full codebase audit — authentication, authorization, injection, sandboxing, network, cryptography, dependencies, supply chain
**Overall Grade:** B+ (production-ready with mitigations)

---

## Executive Summary

The RustyKrab codebase demonstrates solid Rust engineering with zero `unsafe` blocks, modern crypto libraries (`rustls`, `ed25519-dalek`), and thoughtful middleware design. However, **12 CRITICAL**, **10 HIGH**, and **12 MEDIUM** severity findings were identified across five analysis dimensions. The most urgent issues are: unenforced sandbox/capability model, arbitrary command execution, path traversal in file tools, SSRF in HTTP tools, and weak secret encryption.

---

## CRITICAL Findings (12)

### C1. WebSocket Authentication Bypass
**Files:** `crates/rustykrab-gateway/src/auth.rs:23-29`, `ws.rs:15-24`
**Issue:** The `require_auth` middleware exempts all non-`/api/` and non-`/webhook/` paths. The WebSocket endpoint at `/ws/chat` is completely unauthenticated — any attacker can connect, send messages, and read conversations without a Bearer token.
**Impact:** Full conversation access without credentials.

### C2. No Conversation Ownership / Authorization
**File:** `crates/rustykrab-gateway/src/routes.rs:32-76`
**Issue:** REST and WebSocket handlers perform no ownership checks. Any authenticated user (or unauthenticated WS client) can read, delete, or inject messages into any conversation by UUID. The `GET /api/conversations` endpoint enumerates all IDs.
**Impact:** Complete lateral movement between conversations; data exfiltration and poisoning.

### C3. Session/Capability Model Defined But Never Enforced
**Files:** `crates/rustykrab-core/src/session.rs`, `capability.rs`, `crates/rustykrab-gateway/src/routes.rs`
**Issue:** A sophisticated `Session` + `CapabilitySet` system exists but the gateway never creates sessions or passes them to the agent runner. All tools are available to all conversations.
**Impact:** Designed least-privilege model is completely bypassed.

### C4. Sandbox Is a Non-Functional Placeholder
**File:** `crates/rustykrab-agent/src/sandbox.rs:90-131`
**Issue:** `ProcessSandbox` logs policy constraints but returns args unchanged — no seccomp-bpf, no namespace isolation, no enforcement. The runner calls the sandbox *and then* calls the tool directly, meaning the sandbox result is ignored entirely (`runner.rs:570-574`).
**Impact:** All tool-level security policies are unenforced.

### C5. Command Injection — `exec` Tool
**File:** `crates/rustykrab-tools/src/exec.rs:73-76`
**Issue:** `Command::new("sh").arg("-c").arg(command)` — user/agent input passed directly to shell without sanitization.
**Impact:** Arbitrary OS command execution.

### C6. Command Injection — `process` Tool
**File:** `crates/rustykrab-tools/src/process.rs:70-76`
**Issue:** Same pattern as exec — `sh -c` with unsanitized input, plus process spawning for persistent background execution.
**Impact:** Arbitrary command execution with persistence.

### C7. Arbitrary Python Code Execution
**File:** `crates/rustykrab-tools/src/code_execution.rs:55-76`
**Issue:** User-supplied Python code written to a predictable temp file (`rustykrab_exec_{pid}.py`) and executed via `python3`. No sandboxing, no validation. Temp file name collision across concurrent sessions.
**Impact:** Full code execution; cross-session code injection via race condition.

### C8. Path Traversal — `read` Tool
**File:** `crates/rustykrab-tools/src/read.rs:57-63`
**Issue:** `tokio::fs::read_to_string(path)` with no bounds checking. Can read `/etc/shadow`, SSH keys, application secrets.
**Impact:** Unrestricted file disclosure.

### C9. Path Traversal — `write` Tool
**File:** `crates/rustykrab-tools/src/write.rs:60-74`
**Issue:** Creates arbitrary directories and writes to any path. Can overwrite system files, plant backdoors, modify cron jobs.
**Impact:** Arbitrary file write; system compromise.

### C10. Path Traversal — `edit` and `apply_patch` Tools
**Files:** `crates/rustykrab-tools/src/edit.rs:72-107`, `apply_patch.rs:238-256`
**Issue:** Same unrestricted file access. Patch tool additionally parses paths from untrusted patch content.
**Impact:** Arbitrary file modification.

### C11. SSRF — HTTP Request Tools (3 tools)
**Files:** `crates/rustykrab-tools/src/http_request.rs:66-76`, `http_session.rs:125-142`, `web_fetch.rs:69-80`
**Issue:** No URL scheme validation, no private IP blocking. Can access `http://169.254.169.254/` (cloud metadata), internal services, file:// URIs.
**Impact:** Internal network access, credential theft, service enumeration.

### C12. Command Injection — `pdf` Tool
**File:** `crates/rustykrab-tools/src/pdf.rs:82-96, 118-145`
**Issue:** Path passed to `pdftotext` without validation. Python fallback uses `format!()` string interpolation with unescaped user input in Python code.
**Impact:** Arbitrary code execution via Python string injection.

---

## HIGH Findings (10)

### H1. XOR Encryption Without Authentication
**File:** `crates/rustykrab-store/src/secret.rs:69-97`
**Issue:** Secrets encrypted with XOR against HMAC-SHA256 keystream. No authentication tag (AEAD). Ciphertext is malleable — attacker can flip bits without detection. Deterministic: same name always produces same keystream.
**Recommendation:** Upgrade to AES-256-GCM or ChaCha20-Poly1305.

### H2. Weak Key Derivation
**File:** `crates/rustykrab-store/src/secret.rs:81-97`
**Issue:** Master key used directly with HMAC-SHA256 for keystream derivation. No salt, no proper KDF (Argon2/PBKDF2).
**Recommendation:** Use Argon2 for key derivation.

### H3. Master Key Stored in Environment Variable
**File:** `crates/rustykrab-cli/src/main.rs:27-35`
**Issue:** `RUSTYKRAB_MASTER_KEY` visible via `/proc/[pid]/environ`, inherited by child processes. Ephemeral key generated if not set — secrets lost on restart.
**Recommendation:** Use OS keychain or file with 0600 permissions.

### H4. Rate Limiting Bypassed for WebSocket
**File:** `crates/rustykrab-gateway/src/rate_limit.rs:97-100`
**Issue:** Rate limiting only applies to `/api/` paths. WebSocket connections at `/ws/chat` are unlimited.
**Recommendation:** Apply per-IP rate limiting to WS connections.

### H5. No Auth-Specific Rate Limiting
**File:** `crates/rustykrab-gateway/src/auth.rs:12-48`
**Issue:** No exponential backoff or lockout on repeated authentication failures. Enables brute-force attacks on Bearer tokens.

### H6. Weak Webhook Secret Validation (Telegram & Signal)
**Files:** `crates/rustykrab-channels/src/telegram.rs:150-156`, `signal.rs:205-211`
**Issue:** Plain string comparison (timing-vulnerable). `verify_hmac()` function exists but is never called. No payload body validation.
**Recommendation:** Use constant-time comparison + HMAC-SHA256 payload validation.

### H7. Unbounded Agent Recursion
**Files:** `crates/rustykrab-tools/src/subagents.rs:52-61`, `sessions_spawn.rs:53-64`
**Issue:** No recursion depth limits, no parent tracking, no max-nested-agents enforcement. Agents can spawn agents infinitely.
**Impact:** Resource exhaustion DoS.

### H8. Shared Tracer Leaks Cross-Session Data
**File:** `crates/rustykrab-agent/src/runner.rs:64, 175-179`
**Issue:** `ExecutionTracer` shared per runner instance. Session B can receive Session A's execution history (tool names, errors, timing) via trace context injected into conversation.
**Impact:** Information disclosure across sessions.

### H9. Mutex Unwrap Cascade in Tracer
**File:** `crates/rustykrab-agent/src/trace.rs:72, 86, 91, 96, 101, 107-118, 132`
**Issue:** Multiple `.lock().unwrap()` calls. If any thread panics holding the lock, all subsequent lock attempts panic — cascading crash.
**Impact:** Denial of service.

### H10. Information Disclosure via WebSocket Errors
**File:** `crates/rustykrab-gateway/src/ws.rs:56, 135, 144`
**Issue:** Internal error details (JSON parse errors, model provider errors, API details) sent directly to clients via WebSocket `error` frames.
**Recommendation:** Return generic error messages; log details server-side.

---

## MEDIUM Findings (12)

| ID | Issue | File | Impact |
|----|-------|------|--------|
| M1 | No CSRF protection on POST/DELETE endpoints | routes.rs | Cross-site request forgery |
| M2 | Origin policy allows all localhost ports | origin.rs:29-34 | CSRF from shared-host services |
| M3 | Session expiration optional and never enforced | session.rs:22-35 | Sessions never expire |
| M4 | Credential tools lack granular access control | credential_read/write.rs | All-or-nothing secret access |
| M5 | No input validation on secret names | secret.rs:29-55 | Unicode normalization attacks |
| M6 | Memory store search lacks session isolation | memory.rs:57-74 | Cross-conversation data leakage |
| M7 | No request body size limits | routes.rs, webhooks | Memory exhaustion DoS |
| M8 | No connection limits configured | main.rs:203-208 | Unlimited WS connections |
| M9 | Missing security headers (HSTS, CSP, X-Frame-Options) | lib.rs | Browser-based attacks |
| M10 | Unwrap panics in WebSocket handler | ws.rs:112, 164 | Task crash on serialization failure |
| M11 | No TLS enforcement in server config | main.rs:199-208 | Cleartext if proxy misconfigured |
| M12 | No audit logging for credential operations | credential_read/write.rs | No forensic trail |

---

## LOW Findings (5)

| ID | Issue | File |
|----|-------|------|
| L1 | Constant-time comparison leaks length via early return | auth.rs:51-59 |
| L2 | Webhook secret comparison may not be constant-time | telegram/signal_webhook.rs |
| L3 | Static routes exempt from rate limiting | auth.rs:22-25 |
| L4 | Router classification manipulable (affects iteration limits) | router.rs:83-115 |
| L5 | `sled` database is unmaintained upstream | Cargo.toml |

---

## Positive Security Findings

- **Zero `unsafe` blocks** in application code
- **Ed25519 skill verification** — excellent supply chain protection
- **rustls-tls** instead of OpenSSL — eliminates Heartbleed-class bugs
- **Constant-time token comparison** implemented (auth.rs:51-59)
- **Default-deny channel allowlists** — Telegram/Signal deny all if not configured
- **Origin validation middleware** with explicit allowlist
- **Sliding-window rate limiting** with lockout (for API endpoints)
- **Structured tracing** throughout the codebase
- **No wildcard dependency versions** in Cargo.toml

---

## Remediation Priority

### Immediate (Before Any Deployment)
1. Require authentication on WebSocket endpoint (C1)
2. Implement conversation ownership checks (C2)
3. Enforce session/capability model in gateway (C3)
4. Add path validation to file tools — canonicalize + base directory check (C8-C10)
5. Add SSRF protection — block private IPs, validate URL schemes (C11)
6. Replace `sh -c` with allowlisted commands or argument arrays (C5, C6, C12)
7. Sandbox Python code execution with resource limits (C7)

### Short-Term (1-2 Weeks)
8. Implement real sandbox (seccomp-bpf / namespaces) (C4)
9. Upgrade to AES-256-GCM with Argon2 KDF (H1-H3)
10. Apply rate limiting to WebSocket + auth failures (H4-H5)
11. Fix webhook HMAC validation (H6)
12. Add recursion depth limits for agent spawning (H7)
13. Per-session tracers instead of shared (H8)
14. Add security headers middleware (M9)
15. Add request body size limits (M7)

### Medium-Term (1-2 Months)
16. Add `cargo audit` and `cargo deny` to CI/CD
17. Implement audit logging for credential operations
18. Plan `sled` migration to maintained database
19. Add CSRF token validation
20. Implement connection limits

---

## Architecture Recommendations

1. **Defense in depth for tools:** Each tool should independently validate capabilities, not rely solely on runner-level checks
2. **Sandbox-first execution:** Tools should execute *inside* the sandbox, not after it
3. **Per-session isolation:** Separate tracers, memory stores, and temp directories per session
4. **Principle of least privilege:** Default capability sets should be minimal; dangerous tools (exec, process, code_execution) should require explicit opt-in
5. **Security testing:** Add fuzzing for webhook parsers, property-based testing for crypto, integration tests for auth flows
