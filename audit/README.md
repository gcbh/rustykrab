# RustyKrab Codebase Audit — 2026-04-25

A comprehensive analysis of the RustyKrab workspace covering security,
correctness, dead code, missing implementations, and resource handling.
Performed across all 10 crates (~28k LOC, 123 Rust files) on branch
`claude/codebase-analysis-WmuB6`.

## Method

Four parallel audit passes were run, one each focused on:

1. Security (full workspace).
2. Bugs / dead code in `core` + `store` + `agent` + `cli`.
3. Bugs / dead code in `gateway` + `channels` + `providers`.
4. Bugs / dead code in `tools` + `memory` + `skills`.

In addition, I independently spot-checked the most impactful findings
against the source, ran `cargo fmt --check`, `cargo clippy`, and
attempted `cargo test --workspace`. Findings the agents reported
incorrectly were corrected or dropped.

## Headline numbers

| Category | Count |
| --- | --- |
| Critical                                               | **8**   |
| High                                                   | **18**  |
| Medium                                                 | **30+** |
| Low / Info                                             | **40+** |
| Substantive findings total                             | **~100**|

## How to read this report

- `01-critical.md` — issues that should block production. Fix first.
- `02-security.md` — auth/authz, SSRF, injection, supply chain, replay.
- `03-bugs-core-agent.md` — `core`, `store`, `agent`, `cli` correctness.
- `04-bugs-gateway-providers.md` — HTTP/SSE, channels, model providers.
- `05-bugs-tools-memory-skills.md` — tool implementations, retrieval math, skill loading.
- `06-dead-code-and-stubs.md` — unused code, stubs, `todo!()`/swallowed errors.
- `07-tooling-and-build.md` — what `cargo fmt`/`clippy`/`test` say, and why the workspace can’t be fully exercised here.
- `08-positives.md` — practices the codebase already gets right.

Each finding lists `severity`, `file:line`, a short description, and a
suggested fix.

## Top-of-mind summary

The codebase is unusually security-aware for an agent gateway: it has a
proper SSRF allow/deny module, Ed25519 skill signatures, encrypted
secrets at rest, an Origin policy, a per-IP rate limiter, constant-time
token comparison, and a real Linux/macOS sandbox for code execution.

The most important issues are concentrated in a small number of spots:

1. **DNS rebinding** in every outbound HTTP tool (`web_fetch`,
   `http_request`, `image`, `http_session`, `browser`). The
   `validate_url` API in `tools/security.rs` already returns resolved
   addresses specifically to defeat rebinding; no caller uses them.
   See `02-security.md` §S-1.
2. **HTTP redirect bypass** of SSRF protection — only the initial URL is
   validated. `02-security.md` §S-2.
3. **Skill bundle signature canonicalization** — `manifest_bytes ||
   code_bytes` is signed without length prefixes or a domain separator,
   so the manifest/code boundary can be shifted while preserving the
   signature. `02-security.md` §S-3.
4. **TLS validation disabled for a configurable URL** in the Obsidian
   tool. The “self-signed local server” justification doesn’t hold once
   the URL is settable from the agent. `02-security.md` §S-4.
5. **Exec/Process command validators** allow `FOO=bar`-style env
   prefixes and unrestricted args, enabling PATH hijacking and
   `python3 -c <code>` style escapes through the allowlist.
   `02-security.md` §S-5/§S-6.
6. **Webhook replay** — Telegram webhook secret is validated, but
   `update_id` is never checked, so the same update can be processed
   repeatedly. `02-security.md` §S-7.
7. **Token rotation does not revoke in-flight SSE streams** — `/api/logout`
   only swaps the in-memory token; long-running SSE that authenticated
   under the old token continues. `02-security.md` §S-8.
8. **WebChat `Channel::receive` is a stub that always returns an error.**
   The Channel trait shape is incompatible with the underlying
   `mpsc::Receiver`, and the impl admits it in a comment. `04-bugs-gateway-providers.md` §G-1.

`cargo fmt --check`, `cargo clippy --workspace --all-targets`, and a
non-fastembed build all pass cleanly. The full workspace cannot be
built here because `ort-sys` (transitive via the optional `fastembed`
feature on `rustykrab-memory`, which `rustykrab-cli` enables) downloads
prebuilt ONNX Runtime binaries from `cdn.pyke.io`, and that endpoint is
blocked in this environment (HTTP 403). See `07-tooling-and-build.md`.
