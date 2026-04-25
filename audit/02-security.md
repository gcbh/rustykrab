# Security findings

Critical security issues are also enumerated in `01-critical.md`. This
file is the full security inventory grouped by class. Severities follow
the format `[Sev]`.

---

## SSRF / outbound HTTP

### S-1  [Critical] DNS rebinding across all outbound HTTP tools
See `01-critical.md` §C-1. `tools/security.rs` already returns the
resolved addresses; the callers must pin them.

### S-2  [Critical] HTTP redirects bypass SSRF
See `01-critical.md` §C-2.

### S-3  [Medium] `validate_url` blocks `localhost` literal but the loopback IP guard already covers `127.0.0.1`/`::1`
This is a defense-in-depth note, not a hole. Worth keeping. But the
allowlist `["localhost", "metadata.google.internal", "metadata.google.com"]`
should also include `metadata` (Azure shorthand) and the AWS IPv6
metadata endpoint `fd00:ec2::254`.

### S-4  [Medium] No request body / response body cap consistency
- `http_request.rs` has a 5MB cap (line 109).
- `web_fetch.rs` has a content-length-based truncate but reads the
  full body via `resp.text()` first (line 105-108) before truncation.
  A server returning chunked encoding without a Content-Length can OOM
  the agent before truncation kicks in.
- `image.rs` does enforce streaming size limits.

**Fix.** Make `web_fetch.rs` stream the body with a hard cap matching
`http_request.rs`.

### S-5  [Medium] `image` and `web_fetch` don’t verify Content-Type
A server that promises `text/html` can send anything. Worse for
`image`, which trusts the bytes are an image. Verify the
`content-type` header matches the tool’s contract before reading.

### S-6  [High] Outbound clients reuse `reqwest::Client` per tool but new ones per `obsidian.rs:583,637`
`crates/rustykrab-tools/src/obsidian.rs:583` and `:637` build a
*fresh* `reqwest::Client` (with `danger_accept_invalid_certs(true)`,
again) every call. This leaks file descriptors and connection pools
under load and re-opens the §C-4 surface twice more.

---

## Authentication / authorization

### S-7  [High] Auth token rotation doesn’t kill live SSE streams
See `01-critical.md` §C-7.

### S-8  [Medium] `constant_time_eq` is technically correct but loop length leaks the longer string’s length
`crates/rustykrab-core/src/crypto.rs:5-16`. The implementation pads
each side via `unwrap_or(0)` and starts `result` with the
length-mismatch bit, so the *comparison work* is constant-time once
the inputs are determined. The total wall time is still proportional
to `max(len_a, len_b)`, leaking the upper bound of the longer secret.
For the current call sites (Telegram secret token, server bearer
token) both sides are server-controlled or have known lengths, so this
is a hardening note — but if this helper is ever reused for
user-supplied secrets, fix it by clamping to the *expected* length.

### S-9  [Medium] No CSRF defenses on state-changing API endpoints
Auth is bearer-token, so this is reduced — there’s no ambient cookie a
forged page could ride. But `Origin` is mandatory only on `/api/` and
`/webhook/` paths (`gateway/src/origin.rs:74-90`). Static assets
served from the same origin can `fetch('/api/...', { headers: { Authorization } })`
if they ever obtain the token through `localStorage` etc. The CSP
permits `script-src 'unsafe-inline'`, which makes any reflected XSS
trivially exfiltrate the token from the WebChat UI.

### S-10 [Low] CSP permits `'unsafe-inline'`
`crates/rustykrab-gateway/src/lib.rs:36-40`. Drop `'unsafe-inline'`
from `script-src` and `style-src`; use nonces or hashes instead.

### S-11 [Medium] Origin policy panics on unparseable header
`crates/rustykrab-gateway/src/origin.rs:97,100,104` use
`.parse().unwrap()` on header values to insert into `headers`. If a
malformed origin makes it past the validation and the `parse()` for
the response header fails, the request panics rather than 400s.

### S-12 [Low] No CSRF on `/api/logout`
A hostile site that already knows the bearer can rotate it via
`POST /api/logout`. With the bearer, the attacker can do anything
already, so this is informational.

---

## Input validation / injection

### S-13 [Critical] Exec/Process allow `KEY=val` env prefixes (PATH hijack)
See `01-critical.md` §C-5.

### S-14 [High] Exec allowlist permits interpreters with arbitrary `-c` / `-e`
Same source. `python3`, `bash`, `sh`, `node`, `awk`, `sed`, `perl`,
etc. can execute arbitrary code under an allowlist that names the
interpreter. The advertised guarantee ("only allowlisted commands
run") is true at the binary level but false at the *behavior* level.

### S-15 [Medium] Path traversal: `validate_path` uses `starts_with` on raw strings
`crates/rustykrab-tools/src/security.rs:64-71`. `is_path_blocked`
checks `path_str.starts_with(prefix)`. `/root` blocks both `/root/.ssh`
(intended) and `/root_safe_dir/file` (false positive); more
importantly, `/etc/shadow` blocks the file but not `/etc//shadow` or
`/etc/./shadow`, since canonicalization only happens for paths that
already exist (line 102 — `if path_buf.exists()`).

For non-existent paths, the parent is canonicalized and checked, but
the final filename component is not joined back through canonicalize.
A symlink created after validation but before file operation can
escape (TOCTOU).

**Fix.** Always canonicalize against the parent and join the final
component. Use `Path::starts_with` (component-aware) rather than
string `starts_with`. Defer file open to immediately after the check
and use `openat`-style APIs (or `cap_std`) to defeat TOCTOU.

### S-16 [Low] `validate_path` rejects non-UTF-8 paths via `to_string_lossy`
`crates/rustykrab-tools/src/security.rs:94`. `to_string_lossy()` would
mangle non-UTF-8 paths into U+FFFD, which then doesn’t match any
prefix. Either reject non-UTF-8 paths up front or compare on bytes.

### S-17 [Info] SQL string interpolation in `memory/storage.rs:631-636`
`format!("SELECT * FROM memories WHERE id IN ({placeholders})")` with
`placeholders` built by quoting each `Uuid::to_string()`. UUIDs are
hex+hyphens by construction, so this is functionally safe today, but
it’s a footgun. Use `?` placeholders and `rusqlite::params_from_iter`.

### S-18 [Info] `format!` with const `JOB_COLUMNS` in `store/jobs.rs:130`
Safe (operand is a constant), but flagged so future edits don’t turn
it into a real injection.

### S-19 [Low] `web_search.rs:284-286` URL decoder builds a `String` byte-by-byte
Pushes raw decoded bytes as `as char`, which mis-handles multi-byte
UTF-8. Accumulate into `Vec<u8>` and `String::from_utf8_lossy` at the
end.

### S-20 [Low] Telegram webhook logs the raw payload at `debug!`
`crates/rustykrab-channels/src/telegram.rs:357-360`. Raw user
messages, including private content, end up in logs whenever `debug`
is enabled. Either gate behind a more explicit env or log only
metadata (chat id, message id, length).

---

## Replay / freshness

### S-21 [Critical] Telegram update replay
See `01-critical.md` §C-6.

### S-22 [High] Skill signatures have no timestamp / version binding
`crates/rustykrab-skills/src/verify.rs:54-64`. A skill signed once
verifies forever. Compromised publisher keys, or even older known-bad
versions of skills, cannot be revoked. Add a signed timestamp +
version + skill id and reject too-old timestamps and known-bad
versions. Pair with a revocation list distributed alongside trusted
keys.

### S-23 [High] Skill signature canonicalization (manifest||code)
See `01-critical.md` §C-3.

### S-24 [Medium] Signal webhook freshness window hardcoded
`crates/rustykrab-channels/src/signal.rs:22-24,248-259`. 300s is
reasonable; just expose it as `SIGNAL_WEBHOOK_MAX_AGE_SECS`.

---

## Crypto / secrets

### S-25 [Medium] Encrypted-secret keychain fallback silently generates an ephemeral key
`crates/rustykrab-store/src/keychain.rs:242-265`. If the OS keychain
fails *and* `RUSTYKRAB_MASTER_KEY` is unset, a random key is generated
in memory. On the next process start, all stored secrets are
unreadable, but no error is surfaced to the operator.

**Fix.** Fail hard. Force the operator to choose: keychain, env var,
or explicit `--insecure-ephemeral-key`.

### S-26 [Low] Token comparison loop length leaks max length
See S-8. Hardening only.

### S-27 [Info] Secret name validator is conservative
`crates/rustykrab-store/src/secret.rs:106-122`. Only `[A-Za-z0-9_.\-]`
allowed. Conservative is correct; this is a usability note.

---

## Resource exhaustion / DoS

### S-28 [High] Anthropic streaming `tool_input_bufs` is unbounded
`crates/rustykrab-providers/src/anthropic.rs:464-467`. A malicious or
flapping API response can grow the per-tool input buffer without
limit.

### S-29 [Medium] Rate-limiter per-IP `attempts` Vec is unbounded
`crates/rustykrab-gateway/src/rate_limit.rs:89-112`. The map size is
pruned, but a single IP can grow its `Vec<Instant>` indefinitely.

### S-30 [Medium] Rate limiting is in-memory only
Same file. Process restart resets all counters. Either persist or
state this in the README so operators don’t depend on it.

### S-31 [Low] `code_execution` temp files cleanup uses `let _ =`
`crates/rustykrab-tools/src/code_execution.rs:233-234,169`. Failures
are silent; long-running daemons can accumulate temp files. Add a
janitor sweep on a timer.

### S-32 [Medium] `web_search` HTML parser has no input cap
`crates/rustykrab-tools/src/web_search.rs:124-174`. DDG response is
parsed without a size limit. Cap at e.g. 1 MB before parsing.

---

## Channel-specific

### S-33 [High] Signal webhook secret verification path is suspicious
`crates/rustykrab-gateway/src/signal_webhook.rs:13-41` defers to
`signal.parse_webhook_payload(&body, secret_header)`. If the Signal
channel is constructed without `with_webhook_secret(...)`, the
implementation must still reject unauthenticated payloads — verify
this matches Telegram’s "no secret configured ⇒ refuse" pattern at
`crates/rustykrab-channels/src/telegram.rs:347-355`. (Telegram is
correct; double-check the Signal symmetry — the line numbers
237-259 in `signal.rs` suggest equivalent behavior, but the
gateway-side test coverage looks lighter.)

### S-34 [Medium] MCP tool spawns arbitrary commands found via PATH
`crates/rustykrab-channels/src/mcp.rs:121`. The MCP command is taken
verbatim from config and run with `tokio::process::Command::new(...)`,
which performs a `PATH` lookup. If the agent or operator can write
to a directory earlier in `PATH`, this is a TOCTOU. Resolve to an
absolute path at config load time, or refuse non-absolute commands.

---

## Logging / observability

### S-35 [Low] Errors propagated verbatim from `x_search.rs` may include the bearer in URL/error formatting
`crates/rustykrab-tools/src/x_search.rs:77-81` does
`format!("search request failed: {e}")`. If `e` carries the URL
(reqwest does, with query parameters), and any caller ever puts a
token in the query string, it lands in tool errors. Strip the URL
from outbound errors.

