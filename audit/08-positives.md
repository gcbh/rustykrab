# What the codebase already does right

A balanced review needs to call out the practices that are already in
place. Each of these is genuinely uncommon to find on first read and
worth preserving against future churn.

---

## Defense in depth

- **Per-tool sandbox requirements**: `Tool::sandbox_requirements()`
  declares `needs_fs_read`, `needs_spawn`, etc. so the agent loop can
  enforce capabilities per session.
- **Real OS sandbox** for code execution
  (`crates/rustykrab-tools/src/sandboxed_spawn.rs`):
  - `setrlimit` for CPU, file size, memory (Linux only on RLIMIT_AS),
    and process count
  - macOS Seatbelt profile that denies network and constrains writes
  - Linux `unshare` (PID/IPC/NET) and `PR_SET_NO_NEW_PRIVS`
- **`env_clear()` + curated PATH** on subprocess launches.
- **Origin policy** on `/api/` and `/webhook/` (loopback always
  allowed; everything else explicit).
- **Bearer auth via constant-time compare** (`core::crypto::constant_time_eq`).
- **Per-IP rate limiter** layered with auth, origin, logging, and
  security headers (`gateway::lib::router`).
- **Security headers** on every response: `X-Frame-Options: DENY`,
  `X-Content-Type-Options: nosniff`, `X-XSS-Protection`, CSP. (CSP
  needs hardening — §S-10.)
- **Telegram webhook secret** verified with constant time, plus
  optional HMAC-SHA256 path (`channels::telegram::verify_hmac`).

---

## Cryptography

- **Ed25519 skill signing** (`crates/rustykrab-skills/src/verify.rs`)
  with a configurable trusted-publisher key set. Signature
  canonicalization needs work (§C-3) but the underlying primitive
  choice is right.
- **Encrypted secrets at rest** with a key derived from the OS
  keychain or `RUSTYKRAB_MASTER_KEY` (`store::keychain`,
  `store::secret`). Argon2id + AES-GCM stack visible in
  `Cargo.toml`.
- **Constant-time** comparisons everywhere a secret is checked.
- **`reqwest` + `rustls`** as the default TLS stack — no native-tls
  surface. (One tool — Obsidian — opts into
  `danger_accept_invalid_certs`; see §C-4.)

---

## SSRF design (when used correctly)

`crates/rustykrab-tools/src/security.rs::validate_url` resolves the
hostname through `tokio::net::lookup_host`, walks every resolved
address, and rejects RFC 1918, link-local, loopback, broadcast,
unspecified, CGNAT (100.64/10), IPv6 ULA, IPv6 link-local,
IPv4-mapped, and the AWS/GCP metadata IP. It returns the resolved
addresses so callers can pin them — the **API is correct**; the
callers just don’t use it (see §C-1).

---

## Async hygiene

- Spawned tasks have join handles tracked for graceful shutdown.
- Mutex held across `.await` was checked: I did not find a case
  where a `std::sync::Mutex` guard crosses an await point. Locks
  taken in async contexts are dropped before `.await`.
- Locks recover from poison rather than crashing the process
  (`unwrap_or_else(|e| e.into_inner())`).

---

## Code quality signals

- `cargo fmt --check` is clean.
- `cargo clippy --workspace --all-targets` is clean (modulo the
  `ort-sys` build-time download, which isn’t a code issue).
- Zero `todo!()` / `unimplemented!()` / `FIXME` markers.
- Only one `panic!()` in the entire tree, gated behind `#[cfg(test)]`.
- Public surface is well-commented.

---

## Where to keep pressure on

The high-leverage hardening items, in order:

1. Make every existing security primitive **actually used** at the
   call site (DNS rebinding, redirects, validate_url addresses,
   path canonicalization for new files).
2. Bind signed payloads to time/version so revocation is possible.
3. Move from "log and continue" to "fail and report" on data-shape
   surprises (NaN scores, embedding dim mismatch, malformed
   timestamps).
4. Track + invalidate auth tokens on rotation (live SSE streams).
5. Add deduplication on Telegram `update_id`.

The codebase is in good enough shape that fixing the items in
`01-critical.md` would put it ahead of the typical agent gateway in
the wild.
