# Critical findings

Issues that should be addressed before any production deployment.
Each is reproducible from a careful read of the cited file. Cross-references
are given to the topical chapters where the same issue is discussed in
context.

---

## C-1 — DNS rebinding in every outbound HTTP tool

`crates/rustykrab-tools/src/security.rs:160-255` defines `ValidatedUrl`
with a `resolved_addrs: Vec<SocketAddr>` field, and the doc comment
explicitly says:

> Returns resolved socket addresses to prevent DNS rebinding (TOCTOU)
> attacks. Callers should use the returned addresses to pin connections
> rather than re-resolving the hostname.

But every caller throws those addresses away and re-fetches the URL by
string, which causes `reqwest` to do a fresh DNS lookup:

- `crates/rustykrab-tools/src/web_fetch.rs:83-91`
- `crates/rustykrab-tools/src/http_request.rs:84-103`
- `crates/rustykrab-tools/src/image.rs:76-84`
- `crates/rustykrab-tools/src/http_session.rs:143`
- `crates/rustykrab-tools/src/browser/mod.rs:285,311`

A malicious server can respond with a short-TTL DNS answer pointing to
a public IP for the validation lookup, then return an internal IP
(127.0.0.1, 169.254.169.254, the AWS/GCP metadata endpoint, the local
agent gateway itself, etc.) for the fetch lookup.

**Fix.** Either:

- Use `reqwest::ClientBuilder::resolve_to_addrs(&host, &resolved_addrs)`
  to pin the validated addresses for the request, or
- Switch to `Url::set_host(Some(ip_string))` with the validated address
  before sending and forward the original `Host` header.

---

## C-2 — HTTP redirects bypass SSRF protection

`crates/rustykrab-tools/src/web_fetch.rs:25` —
`reqwest::redirect::Policy::limited(10)`
`crates/rustykrab-tools/src/http_request.rs:21` —
`reqwest::redirect::Policy::limited(10)`

Only the initial URL is run through `security::validate_url`. A 302
redirect to `http://169.254.169.254/latest/meta-data/` or `http://10.0.0.1/`
is followed without re-validation.

**Fix.** Set `Policy::custom(...)` and call `validate_url` (or at least
`is_private_ip`) on each redirect destination. Or set `Policy::none()`
and follow redirects manually with validation on each hop.

---

## C-3 — Skill signature is not canonicalized

`crates/rustykrab-skills/src/verify.rs:54-64`:

```rust
pub fn verify_skill_bundle(
    &self,
    manifest_bytes: &[u8],
    code_bytes: &[u8],
    signature_bytes: &[u8],
) -> Result<(), Error> {
    let mut payload = Vec::with_capacity(manifest_bytes.len() + code_bytes.len());
    payload.extend_from_slice(manifest_bytes);
    payload.extend_from_slice(code_bytes);
    self.verify(&payload, signature_bytes)
}
```

The signed payload is `manifest || code` with no length prefix and no
domain separator. Given a signature for `(M, C)`, the same signature
verifies for any `(M', C')` such that `M' || C' == M || C`. An attacker
who controls bundling can therefore shift bytes between the manifest
and the code portion of an already-signed skill — for example, moving a
malicious tool binding from `code` into `manifest` (or vice versa)
where the loader treats the two segments differently.

In addition, there is no replay/version binding (S-3 in `02-security.md`).

**Fix.** Sign a domain-separated, length-prefixed payload, e.g.:

```
b"RUSTYKRAB-SKILL-v1\x00"
  || (manifest_len as u64 LE)
  || manifest_bytes
  || (code_len as u64 LE)
  || code_bytes
  || timestamp_ms as u64 LE
  || version_string_len as u32 LE
  || version_string
```

Verify all of those fields and reject signatures whose timestamp is
outside an acceptable freshness window.

---

## C-4 — TLS verification disabled on a configurable URL (Obsidian tool)

`crates/rustykrab-tools/src/obsidian.rs:25-34, 583, 637`:

```rust
let client = reqwest::Client::builder()
    .danger_accept_invalid_certs(true)
    .build()
    .unwrap_or_default();
```

The justification in the comment is "localhost-only, self-signed". But
the API URL comes from the secret store (`obsidian_api_url`,
default `https://127.0.0.1:27124`) and can be set by the agent itself
through the `obsidian(action='setup', api_url=...)` tool (line 107).
So an attacker who can write to that key — or simply mistype it once —
becomes a man-in-the-middle on the API key sent in `Authorization:
Bearer …`.

**Fix.** Only disable verification when the URL’s host is one of
`127.0.0.1`, `::1`, or `localhost`. Reject anything else with cert
validation off. Better: don’t disable verification at all and document
that users must trust the cert (e.g., add the cert to a custom
`reqwest::Certificate` set).

---

## C-5 — Exec/Process tools allow `KEY=val` prefix → PATH hijacking

`crates/rustykrab-tools/src/exec.rs:198-220`:

```rust
for token in sub.split_whitespace() {
    // Variable assignments (KEY=value) are not commands — skip them.
    if token.contains('=') && !token.starts_with('=') {
        continue;
    }
    let cmd_name = token.rsplit('/').next().unwrap_or(token);
    if ALLOWED_COMMANDS.contains(&cmd_name) { found_allowed = true; break; }
    return Err(format!("command '{cmd_name}' is not in the allowlist..."));
}
```

This means `PATH=/tmp/evil python3 -c "…"` passes the validator because
`PATH=/tmp/evil` is skipped and `python3` is allowed. The command is
then run via `sh -c`, which honours the `PATH=` prefix as an exec-time
override, so the agent runs `/tmp/evil/python3` instead of the system
binary.

The same class of bypass also applies through `python3 -c '…'`,
`bash -c '…'`, `sh -c '…'`, `node -e '…'`, `perl -e '…'`,
`awk 'BEGIN{system("…")}'`, etc., which the allowlist permits but does
not constrain.

**Fix.** Reject any `KEY=value` token. Maintain a per-command argument
policy that forbids `-c`/`-e`/equivalents on interpreters in the
allowlist. Don’t hand the command string to `sh -c` at all — parse it
into argv yourself and `Command::new(argv[0]).args(&argv[1..])`.

---

## C-6 — Telegram webhooks have no replay protection

`crates/rustykrab-channels/src/telegram.rs:340-364`. The
`X-Telegram-Bot-Api-Secret-Token` header is checked with
`constant_time_eq`. There is no idempotency check on `update.update_id`,
which is the canonical Telegram dedup key. Anyone in possession of one
valid request body + secret header (e.g. a leaked log line, or a
captured TLS-terminated proxy hop) can replay the message any number of
times.

**Fix.** Keep a bounded time-windowed set of seen `update_id`s
(LRU/LFU keyed on `(chat_id, update_id)`) and drop replays.

---

## C-7 — Auth token rotation does not invalidate live SSE streams

`crates/rustykrab-gateway/src/auth.rs:38-53` and
`crates/rustykrab-gateway/src/state.rs:151-...`. `rotate_token()` swaps
the in-memory token, but the `require_auth` middleware only runs once
per request, before `next.run(request)` returns the SSE response body.
A long-lived SSE stream that authenticated with the *old* token keeps
streaming events forever.

**Fix.** Add a per-stream cancellation token tied to the auth-token
generation; when `rotate_token()` is called, signal cancellation. Or
embed a token generation number in the SSE stream and have the agent
loop re-check it between events.

---

## C-8 — WebChat `Channel::receive` is a stub that always errors

`crates/rustykrab-channels/src/webchat.rs:44-50`. The trait method
takes `&self`, but `mpsc::Receiver::recv` requires `&mut self`. The
implementation returns a hardcoded "WebChat does not support polling"
error and the comment admits this is unresolved.

Anything in the codebase that expects to receive from WebChat through
the unified `Channel` trait silently never gets messages.

**Fix.** Wrap the receiver in `Arc<Mutex<...>>` (tokio mutex, not std)
and acquire it inside `receive`, or change the `Channel` trait to take
`&mut self`, or split `Channel` into `Sender` + `Receiver` halves.

