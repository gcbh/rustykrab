# Plan: Apollo iOS app + credential write-protection

**Status:** approved plan, not yet implemented
**Date:** 2026-08-19
**Repos:** `gcbh/rustykrab` (server work), `gcbh/apollo-ios` (app work)

## 1. Problem

Two intertwined goals:

1. **The agent can overwrite credentials.** Today any agent turn can silently
   replace or delete a stored credential. The requirement: the user can add
   credentials to the agent, the agent can *add* credentials, but any
   **overwrite or delete of an existing credential requires the user's
   explicit approval**.
2. **An iOS app ("Apollo")** that talks to a personal rustykrab instance over
   a secure connection, so credentials can be entered on the phone, stored on
   the Mac, and agent change-requests can be approved from the phone. Chat is
   included in v1. Distribution via TestFlight. Credential storage stays local
   to the Mac for now, with a clean seam to swap in an API-backed storage
   solution later.

## 2. Decisions (locked in with the user, 2026-08-19)

| # | Question | Decision |
|---|----------|----------|
| 1 | Agent write policy | **Create-only.** Agent may create credentials under new names; any update/delete/keychain-overwrite of an existing credential files a pending request. Enforced at the store layer, not per-tool. |
| 2 | Approval surface | **iOS app + WebChat.** One pending-requests REST API; the app is primary, WebChat gets an approvals panel. |
| 3 | Phone → Mac connectivity | **Tailscale.** No ports exposed to the internet; `tailscale serve` provides real HTTPS on the tailnet. |
| 4 | App v1 scope | **Credentials + approvals + chat.** |
| 5 | Apple Developer Program | Already enrolled — TestFlight available immediately. |
| 6 | App auth | **Per-device pairing** with per-device tokens (revocable, attributable). |
| 7 | Approval notifications | **In-app + badge in v1; APNs push in a later phase** (both wanted). |
| 8 | Multi-server | **One gateway in v1, data model designed for multiple** server profiles. |
| 9 | Evaluation | **Harness-first.** A build + end-to-end evaluation system is the first implementation stage, before any feature work. Every later phase's exit criteria are encoded as executable scenarios so agents can autonomously verify the agreed end state. |

## 3. Current state (what exists today)

### Secret storage
- `SecretStore` (`crates/rustykrab-store/src/secret.rs`) — SQLite, AES-256-GCM
  with per-secret Argon2id-derived keys. `set()` is a **silent upsert**
  (`INSERT … ON CONFLICT(name) DO UPDATE`, secret.rs:66). `delete()` is
  unconditional. No versioning, no provenance, no audit trail.
- macOS Keychain integration (`crates/rustykrab-store/src/keychain.rs`) —
  `set_credential()` **replaces** any existing item.
- Secret registry (`crates/rustykrab-store/src/registry.rs`) — resolves each
  known credential env var → keychain → store at startup and *persists
  downward* (system-initiated writes).

### Agent-reachable write paths (all currently unguarded)
| Path | Location | Can overwrite? | Can delete? |
|------|----------|----------------|-------------|
| `credential_write` tool (`set`, `delete`, `import_from_keychain`) | `rustykrab-tools/src/credential_write.rs` | yes | yes |
| Gmail configure flow | `rustykrab-tools/src/gmail.rs` | yes | — |
| CalDAV configure flow (`KEY_APP_PASSWORD`) | `rustykrab-tools/src/caldav.rs:183` | yes | — |
| Obsidian configure flow (`KEY_API_URL`) | `rustykrab-tools/src/obsidian.rs:120` | yes | — |
| macOS Keychain via `credential_write` (`source: "keychain"`) | keychain.rs `set_credential` | yes | yes |

### User/HTTP write paths
- `POST /api/secrets` (upsert), `DELETE /api/secrets/{name}` —
  `rustykrab-gateway/src/routes.rs:44-46`. Gated by a **single shared bearer
  token** (`auth.rs`), rotatable via `POST /api/logout`.

### Gateway
- Axum server, loopback-only bind `127.0.0.1:3000` (hard-coded,
  `rustykrab-cli/src/main.rs:1204`). Middleware stack: security headers →
  request logging → rate limit → origin check → bearer auth
  (`rustykrab-gateway/src/lib.rs:57`).
- Apollo-shaped conversation/message DTOs and SSE streaming already exist in
  `routes.rs` (camelCase, epoch-millis). The contract doc they cite was
  missing; it now lives at `docs/integrations/apollo.md`.
- WebChat UI is embedded static assets (`rust_embed`, `gateway/static/`).

**Key observation:** the agent runs in-process with the gateway and its tools
call `SecretStore` directly — the agent never holds the HTTP bearer token. So
principal separation is enforced at the **store/tool layer** (agent side) and
the **HTTP layer** (user side); the agent physically cannot call the approval
endpoints.

## 4. Target architecture

```
┌─────────────── iPhone ───────────────┐        ┌──────────────── Mac ────────────────┐
│ Apollo app (SwiftUI)                 │        │ rustykrab daemon                    │
│  • device token in iOS Keychain      │  HTTPS │  axum gateway (127.0.0.1:3000)      │
│  • chat (REST + SSE)                 │◄──────►│   ├─ bearer auth: master OR device  │
│  • add credential (create-only POST) │ tailnet│   ├─ /api/credential-requests       │
│  • approvals (Face ID gate)          │  (ts.  │   ├─ /api/pair, /api/devices        │
│  Tailscale VPN                       │  net   │   └─ WebChat UI + approvals panel   │
└──────────────────────────────────────┘  cert) │  agent loop (in-process)            │
                                                │   └─ tools → GuardedSecrets         │
                                                │        create ✓ / overwrite → queue │
                                                │  SecretStore (SQLite, AES-256-GCM)  │
                                                │   ├─ secrets + versions + audit     │
                                                │   ├─ credential_requests            │
                                                │   └─ devices + pairing_codes        │
                                                └─────────────────────────────────────┘
```

## 5. Workstream A — credential write-protection (rustykrab)

### A1. Store layer: authority-aware writes
Introduce an explicit write authority instead of one `set()` for everyone:

```rust
pub enum WriteAuthority {
    /// Explicit human action via an authenticated client (REST/CLI/UI).
    User { device: Option<String> },
    /// Startup/registry persistence (env → keychain → store mirroring).
    System,
    /// Agent tool execution. Create-only.
    Agent { conversation_id: Option<Uuid> },
}
```

`SecretStore` gains:
- `create(name, value)` — `INSERT` only; `Error::AlreadyExists` on conflict.
- `overwrite(name, value, authority)` — refuses `Agent` authority; archives
  the previous value into `secret_versions`; writes an audit row.
- `delete(name, authority)` — refuses `Agent`; archives; audits.
- The old upserting `set()` is **removed** (compile-time guarantee that no
  call site keeps silent-overwrite semantics).

New tables (added idempotently in `Store::run_migrations()`):

```sql
CREATE TABLE IF NOT EXISTS credential_requests (
  id              TEXT PRIMARY KEY,            -- UUID
  name            TEXT NOT NULL,
  action          TEXT NOT NULL,               -- 'update' | 'delete'
  proposed_data   BLOB,                        -- encrypted like secrets; NULL for delete
  reason          TEXT,                        -- agent-supplied justification
  conversation_id TEXT,
  status          TEXT NOT NULL DEFAULT 'pending', -- pending|approved|denied|expired
  created_at      INTEGER NOT NULL,
  decided_at      INTEGER,
  decided_by      TEXT                         -- device id / 'webchat' / 'cli'
);

CREATE TABLE IF NOT EXISTS secret_versions (
  name        TEXT NOT NULL,
  version     INTEGER NOT NULL,
  data        BLOB NOT NULL,                   -- encrypted superseded value
  replaced_at INTEGER NOT NULL,
  replaced_by TEXT NOT NULL,                   -- authority description
  PRIMARY KEY (name, version)
);

CREATE TABLE IF NOT EXISTS secret_audit (
  id         INTEGER PRIMARY KEY AUTOINCREMENT,
  name       TEXT NOT NULL,
  op         TEXT NOT NULL,                    -- create|overwrite|delete|approve|deny
  authority  TEXT NOT NULL,
  request_id TEXT,
  at         INTEGER NOT NULL
);
```

The proposed value inside a pending request is encrypted with the same
AES-256-GCM scheme as secrets (AAD = request id) so a pending request is never
a plaintext copy of a credential.

### A2. `GuardedSecrets`: the only handle agent tools receive
A wrapper in `rustykrab-store` holding `Agent` authority:

- `set(name, value)` → if the name is **new**: creates it, returns
  `Created`. If it **exists**: files a `credential_requests` row and returns
  `PendingApproval { request_id }` (surfaced to the model as a normal tool
  result, not an error — the agent should relay "waiting for your approval,
  request `<id>`" to the user in-channel, which covers the v1 notification
  decision alongside the app badge).
- `delete(name)` → always files a request (`action='delete'`).
- `get` / `list_names` → pass-through (read policy unchanged for now).
- Keychain writes follow the same rule: existing service/account pair →
  request; new pair → allowed. `import_from_keychain` is a `set` internally
  and inherits the policy automatically.

Wiring: every tool constructor that today takes `SecretStore`
(`credential_write`, `credential_read`, `gmail`, `caldav`, `obsidian`,
`mcp_connector`, …) takes `GuardedSecrets` instead — one type change in
`rustykrab-cli/src/main.rs` where tools are built, matching the existing
adapter-struct pattern. The raw `SecretStore` remains reachable only from:
- `registry::resolve()` (System authority — startup mirroring only),
- gateway REST handlers (User authority),
- approval execution (below).

A new `Error::PendingApproval { request_id, name }` variant in
`rustykrab-core` lets non-credential tools (Gmail/CalDAV/Obsidian configure
flows) propagate a friendly, self-explanatory message without per-tool code.

### A3. Approval flow
- `GET /api/credential-requests?status=pending` — list (name, action, reason,
  createdAt, conversationId; **never** the proposed value).
- `POST /api/credential-requests/{id}/approve` — executes the stored change
  with `User` authority (archiving the old value), marks `approved`, audits
  with the deciding device.
- `POST /api/credential-requests/{id}/deny` — marks `denied`, wipes
  `proposed_data`.
- Requests expire after **7 days** (swept lazily on list/approve), and are
  auto-superseded if a newer request targets the same name.
- Approving is idempotent-safe: a request whose target changed since filing
  (version bumped by the user) is rejected with `409` so a stale approval
  can't clobber a fresher user edit.

### A4. User-side REST hardening
`POST /api/secrets` today silently upserts for any token holder. Change:
- default becomes **create-only**; overwriting requires explicit
  `"overwrite": true` in the body, which UIs send only after a native confirm
  dialog (this is what "explicit user approval" means for user-initiated
  writes).
- `GET /api/secrets` response gains metadata per entry:
  `{name, createdAt, updatedAt, version}` (values never returned by any
  endpoint, unchanged).
- `DELETE /api/secrets/{name}` stays, now archiving + auditing.

### A5. WebChat approvals panel
Small addition to the embedded static UI: a "Pending approvals" badge and
panel (list → approve/deny buttons hitting A3 endpoints), plus a confirm
dialog on credential overwrite in the existing secrets form. This is the
"continuation of a front-end" — the same API the app uses.

### A6. Tests
- Store: create-vs-overwrite semantics, authority refusal, version archiving,
  request lifecycle (pending → approved/denied/expired/superseded), stale
  approval `409`.
- Tool: `credential_write` `set` on existing name returns pending JSON with a
  request id; `delete` always queues.
- Gateway: endpoint auth (device + master), create-only default on
  `POST /api/secrets`.

## 6. Workstream B — device pairing & per-device auth (rustykrab)

New tables:

```sql
CREATE TABLE IF NOT EXISTS devices (
  id           TEXT PRIMARY KEY,   -- UUID
  name         TEXT NOT NULL,      -- "Graham's iPhone"
  token_hash   BLOB NOT NULL,      -- SHA-256 of the opaque device token
  created_at   INTEGER NOT NULL,
  last_seen_at INTEGER,
  revoked_at   INTEGER,
  push_token   TEXT                -- APNs, phase 3
);

CREATE TABLE IF NOT EXISTS pairing_codes (
  code_hash  BLOB PRIMARY KEY,     -- SHA-256; 8-char code, 5-min TTL, single use
  expires_at INTEGER NOT NULL,
  used_at    INTEGER
);
```

Flow:
1. On the Mac: `rustykrab pair` (new CLI subcommand) or a WebChat "Pair
   device" button → prints/displays the code and a QR payload
   `{"url": "https://<mac>.<tailnet>.ts.net", "code": "XXXXXXXX"}`.
2. App calls `POST /api/pair {code, deviceName}` (the **only**
   unauthenticated `/api` endpoint besides `/api/health`; strictly
   rate-limited via the existing `rate_limit` middleware; code single-use).
3. Response `{deviceId, deviceToken}` — token is random 32 bytes, stored
   hashed server-side, shown exactly once.
4. `auth::require_auth` extends to accept `Bearer <master-token>` **or**
   `Bearer <device-token>` (hash lookup against non-revoked devices, then
   constant-time compare), inserting a `Principal` request extension
   (`Master` | `Device{id, name}`) used for approval attribution and audit.
5. Device management: `GET /api/devices`, `DELETE /api/devices/{id}`
   (revocation — lost-phone story), master token or another device required.

Existing clients (CLI chat, WebChat) keep using the master token unchanged.

## 7. Workstream C — Apollo iOS app (`gcbh/apollo-ios`)

Full app-side details live in that repo's README. Summary:

- **Stack:** SwiftUI, iOS 17+, Xcode project checked in, **zero third-party
  dependencies** (URLSession incl. SSE via `bytes(for:)`, Security/Keychain,
  LocalAuthentication, AVFoundation for QR scan).
- **Screens:** Pairing (QR/manual) → tabs: Chat (conversations + streaming
  thread), Credentials (list metadata / add / explicit overwrite / delete),
  Approvals (badge, Face ID-gated approve/deny), Settings (server profile,
  device management, health check).
- **Security posture:** device token in iOS Keychain
  (`kSecAttrAccessibleWhenUnlockedThisDeviceOnly` — never syncs to iCloud);
  credential values exist in memory only during entry and are never persisted
  or echoed back; ATS stays fully on (real TLS via the ts.net cert);
  Face ID (`LAContext`) before approve/deny; approvals list shows names and
  reasons, never values.
- **Server model:** `[ServerProfile]` array persisted from day one, UI pinned
  to one profile in v1 (decision #8).
- **TestFlight:** existing Apple Developer account; App Store Connect record,
  automatic signing, `ITSAppUsesNonExemptEncryption=false` (standard TLS
  only), archive → upload → **internal** testing group (no Beta App Review
  wait).

## 8. Workstream D — connectivity (Tailscale, ops only)

1. Install Tailscale on the Mac and iPhone, same tailnet, MagicDNS on.
2. On the Mac: `tailscale serve --bg 3000` (older CLIs:
   `tailscale serve https / http://127.0.0.1:3000`) — publishes
   `https://<mac-hostname>.<tailnet>.ts.net` with an auto-provisioned
   Let's Encrypt certificate, proxying to the loopback-only gateway.
3. Verify from the phone (Tailscale VPN on, any network incl. cellular):
   `https://<mac>.<tailnet>.ts.net/api/health` → `ok`.

Gateway keeps its `127.0.0.1:3000` bind — nothing is exposed beyond the
tailnet, and the app needs no ATS exceptions. This is Phase 0 and requires no
code.

## 9. Workstream E — APNs push (phase 3)

- Apple side: enable Push Notifications capability; create an APNs **key**
  (.p8) in the developer account.
- Gateway side: small push module (HTTP/2 to `api.push.apple.com`, ES256
  JWT token auth — `a2` crate or hand-rolled with the existing HTTP stack);
  fires on new `credential_requests` rows.
- App side: register token via `POST /api/devices/{id}/push-token`; payload is
  deliberately generic ("Apollo: approval requested for `<name>`" — never a
  value); tap deep-links to the Approvals tab.
- Until then (v1): approvals surface via app badge/refresh-on-foreground and
  the agent saying so in-channel (decision #7).

## 10. Later: swapping local storage for an API-backed secret store

Per the repo's backend-trait pattern, define a `SecretsBackend` trait once
Workstream A lands (the authority/guard/request layer sits **above** it, so
the policy is backend-agnostic):

```rust
trait SecretsBackend: Send + Sync {
    async fn create(&self, name: &str, value: &str) -> Result<()>;
    async fn overwrite(&self, name: &str, value: &str) -> Result<()>;
    async fn get(&self, name: &str) -> Result<String>;
    async fn delete(&self, name: &str) -> Result<()>;
    async fn list(&self) -> Result<Vec<SecretMeta>>;
}
```

The SQLite implementation is the only one for now (decision: keep the Macs as
the credential home). Candidates later: HashiCorp Vault, 1Password Connect, or
a small self-hosted service — swap without touching tools, policy, or the app.

## 11. Workstream F — the evaluation harness (built first)

**Goal:** any agent session — cloud sandbox, CI runner, or a Mac — can build
every component, boot the system, and execute end-to-end scenarios that encode
each phase's exit criteria. The harness is the *executable definition of
done*: the target scenarios are written on day one and marked expected-fail
(`xfail`); shipping a phase means flipping its scenarios to must-pass. An
agent has reached the agreed end state exactly when the suite is green with
zero xfails.

### F1. Make the daemon buildable in agent sandboxes
Empirical finding (2026-08-19, this Claude environment): the workspace fails
to build because `ort-sys` (via `fastembed`) downloads a prebuilt ONNX
runtime from `cdn.pyke.io`, which the environment's network policy blocks.
But `rustykrab-memory` already feature-gates it (`default = []`,
`fastembed = ["dep:fastembed"]`) and the gateway builds without it — only
`rustykrab-cli` hard-enables the feature. Fix:

- `rustykrab-cli` gains `[features] default = ["embeddings"];
  embeddings = ["rustykrab-memory/fastembed"]`, with the embedding config
  path in `main.rs` behind `#[cfg(feature = "embeddings")]`.
- Release/Mac builds are unchanged (default on). Sandboxes and the E2E lane
  build with `cargo build -p rustykrab-cli --no-default-features`.
- Optionally, allowlisting `cdn.pyke.io` in the Claude environment's network
  policy restores full-fidelity builds in sandboxes too.

### F2. `ScriptedProvider` — deterministic agent for E2E
A `ModelProvider` test double that replays a scripted sequence of tool calls
and text turns, so scenarios like “the agent tries to overwrite
`notion_api_token`” run deterministically, fast, and free — no live model.
The same scenarios can run in **live-fire mode** against the real Anthropic
provider when `ANTHROPIC_API_KEY` is present (optional env secret in the
Claude environment; scripted mode is the default everywhere).

### F3. E2E runner (`scripts/e2e.sh` + `crates/rustykrab-e2e`)
Builds the daemon (`--no-default-features`), boots it on a temp data dir with
`RUSTYKRAB_MASTER_KEY`, `RUSTYKRAB_AUTH_TOKEN`, and dummy values for the
registry's `required` secrets (Notion, Obsidian — startup validation refuses
to boot without them), waits for `/api/health`, then drives scenarios over
HTTP and asserts on responses **and** on store state:

1. pair a device (mint code → exchange → call API with device token)
2. create a secret; `POST /api/secrets` on an existing name → `409`; with
   `overwrite: true` → applied, old value archived
3. scripted agent creates a new credential → succeeds silently
4. scripted agent `set` on an existing name → value unchanged, one pending
   request
5. approve → new value live, version archived, audit row attributed to the
   deciding device
6. deny → value unchanged, proposal wiped
7. agent `delete` → request; expiry sweep; stale approval → `409`
8. revoked device → `401`

Output is JSON + exit code so agents can assert mechanically. Every scenario
maps to a phase exit criterion; a new feature ships with its scenario in the
same PR. CI gains an `e2e` job running the same script.

### F4. `ApolloKit` — the app's protocol layer, testable on Linux
The app splits into a SwiftPM package + thin SwiftUI shell. `ApolloKit`
(DTOs, API client, SSE parser, pairing, approvals flows) uses only
Foundation/FoundationNetworking, so `swift build && swift test` works on
Linux and macOS. An `apollo-e2e` executable target drives the *same scenario
list* as F3 against a live gateway — one sandbox session can then run true
cross-repo contract E2E: Rust daemon up, Swift client exercising it.
Empirical finding: no Swift toolchain in the current sandbox and
`download.swift.org` is blocked (the sandbox is Ubuntu 24.04, for which
swift.org ships an official toolchain) — see F6. Until allowed, the Swift
lane runs in CI.

### F5. CI lanes (the only place an iOS `.app` can be verified from the cloud)
Building the actual iOS app, simulator testing, and TestFlight are
macOS-only — no sandbox configuration changes that. So:

- **rustykrab `ci.yml`** (ubuntu): existing check/clippy/test/fmt/audit jobs
  + new `e2e` job running `scripts/e2e.sh`.
- **apollo-ios CI** (new): `swift test` for ApolloKit on `ubuntu-latest`;
  `xcodebuild build test` for the app + unit tests on `macos-latest`.
  Cloud agents iterate on the app by pushing and reading CI results (they
  can subscribe to PR check events). macOS runner minutes bill at 10× on
  private repos — keep the macOS job to build + unit tests per push; UI
  tests (XCUITest) run nightly or on demand.
- **The Mac** (local Claude Code session or the user): full-fidelity lane —
  simulator, XCUITest against a local gateway, TestFlight archive/upload.

### F6. Environment prerequisites (user-actionable)
For the Claude cloud environment used on these repos:

| Item | Unblocks | Priority |
|------|----------|----------|
| Allowlist `download.swift.org` (+ `swift.org`) and install the Swift toolchain in the environment setup script | F4's Swift lane inside sandboxes (build ApolloKit + run cross-repo E2E locally) | high |
| Allowlist `cdn.pyke.io` | full-fidelity (embeddings-on) daemon builds in sandboxes; otherwise `--no-default-features` covers everything the guard needs | nice-to-have |
| `ANTHROPIC_API_KEY` as an environment secret | live-fire E2E lane (scripted mode needs nothing) | optional |
| Apple signing: none needed for CI build/test; App Store Connect API key only if TestFlight upload should ever run from CI | automated TestFlight from CI (otherwise upload stays on the Mac) | later |

### F7. Running the whole loop locally on the Mac
The Mac is the fullest lane (Xcode, simulator, TestFlight, unrestricted
network, real Keychain), so day-to-day development can move there entirely —
the branch and these docs are the hand-off. Bootstrap:

```sh
mkdir -p ~/dev/apollo && cd ~/dev/apollo
git clone https://github.com/gcbh/rustykrab.git
git clone https://github.com/gcbh/apollo-ios.git
(cd rustykrab  && git checkout claude/ios-app-credentials-ikd7bm)
(cd apollo-ios && git checkout claude/ios-app-credentials-ikd7bm)
cd rustykrab && claude --add-dir ../apollo-ios
```

Opening message for the local session:
> Read docs/plans/apollo-ios-and-credential-guard.md and
> docs/integrations/apollo.md, then start Phase 1 (Workstream F) on this
> branch.

Local sessions keep committing and pushing to the same branch, so cloud
sessions and CI remain interchangeable with the Mac at any time. The
sandbox-facing pieces of Workstream F (feature gate, ScriptedProvider,
`scripts/e2e.sh`, CI lanes) stay in scope regardless of where development
runs — they are what CI and any future cloud session use to verify the end
state without a Mac.

### What each lane can verify

| Capability | Cloud sandbox | GitHub Actions | Mac |
|------------|---------------|----------------|-----|
| Build daemon + run guard E2E (scripted agent) | ✓ (after F1) | ✓ | ✓ |
| Live-fire agent E2E | with env key | with secret | ✓ |
| Build + test ApolloKit (Swift) | after F6 allowlist | ✓ (ubuntu) | ✓ |
| Cross-repo contract E2E (Swift client ↔ Rust daemon) | after F6 allowlist | ✓ | ✓ |
| Build the iOS app target | ✗ | ✓ (macos runner) | ✓ |
| Simulator / XCUITest | ✗ | ✓ (macos runner) | ✓ |
| TestFlight archive + upload | ✗ | possible, deferred | ✓ |

## 12. Phasing

| Phase | Contents | Exit criteria |
|-------|----------|---------------|
| **0 — Ops** | Tailscale on Mac + phone; `tailscale serve`; contract doc committed | `/api/health` reachable from the phone over HTTPS on cellular |
| **1 — Evaluation harness** | Workstream F: cli `embeddings` feature gate, `ScriptedProvider`, E2E runner with the full scenario list (guard scenarios xfail), ApolloKit package skeleton + `swift test`, CI lanes in both repos | One command builds daemon (+ ApolloKit where Swift is available), boots, and runs the suite green in a fresh sandbox and in CI; target scenarios exist as xfail |
| **2 — Server guard** | Workstream A (store, guard, requests, REST) + Workstream B (pairing, device auth) + WebChat approvals panel — each landing flips its xfail scenarios to must-pass | E2E scenarios 1–8 pass for real; `cargo fmt/clippy/test` green |
| **3 — App MVP** | Workstream C: pairing → credentials → approvals → chat (SSE) → **TestFlight build 1** to internal testers | ApolloKit E2E green against a live gateway; macOS CI builds the app; add a credential and approve a request from the phone; hold a streamed chat |
| **4 — Notify & polish** | Workstream E (APNs), device management UI, version history + rollback UI | Lock-screen push on new request; revoke a device from the app |
| **5 — Deferred** | `SecretsBackend` extraction; multi-server profile UI | — |

Phases 2 and 3 can overlap once Phase 2's REST surface is merged (the app can
develop against it, and the harness pins the contract from both sides).

## 13. Threat-model notes

Protected against:
- **Agent clobbering or deleting credentials** — enforced in the store layer
  under a compile-time-distinct authority, covering every tool path including
  Gmail/CalDAV/Obsidian configure flows and keychain writes; not a
  prompt-level restriction.
- **Stale/spoofed approvals** — approvals require an authenticated User
  principal over HTTP; the agent has no HTTP credentials and runs in-process
  with no route to the approval endpoints; version check rejects stale
  approvals.
- **Lost phone** — per-device token, revocable server-side; token
  non-exportable (Keychain, this-device-only); Face ID on approvals.
- **Network exposure** — gateway stays loopback-only; tailnet-internal HTTPS;
  no public ports; pairing codes short-lived, single-use, rate-limited.
- **Value leakage** — no endpoint returns secret values; pending proposals
  encrypted at rest; APNs payloads carry names only.

Explicitly out of scope:
- A compromised Mac user account (the master key and store live there — same
  as today).
- The user rubber-stamping approvals without reading them.
- Malicious tampering with the rustykrab binary itself.

## 14. Open items (non-blocking, defaults chosen)

- Request expiry window: defaulting to 7 days.
- macOS CI runner minutes bill at 10× on private repos — start with build +
  unit tests per push and revisit if the bill matters.
- Version history retention: unlimited for now; a purge command can come with
  the rollback UI in Phase 3.
- Whether `credential_read` should ever gate value reads (e.g. per-name agent
  read policy) — out of scope here, worth revisiting after Phase 1.
