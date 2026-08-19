# RustyKrab vs. DSH Desktop — Architecture & Engineering Comparison

A full examination of [RustyKrab](https://github.com/gcbh/rustycrab) against
[`anywhere-labs/deepseek-harness-desktop`](https://github.com/anywhere-labs/deepseek-harness-desktop)
("DSH Desktop"), based on a direct read of both codebases.

- RustyKrab revision examined: `b84c422` (workspace version `4.5.15`)
- DSH Desktop revision examined: `e16fd26` (package version `2.0.1`, upstream pin `0.1.0-rc.7`)
- Date of examination: 2026-08-19

---

## 1. Executive summary

**These are not competing projects. They occupy different layers of the same stack.**

RustyKrab is an **agent runtime**: it implements the model loop, the tool
catalog, memory, persistence, and a network gateway, then exposes that engine to
remote clients over Telegram, Signal, Slack, WebChat, and MCP. It has no desktop
presence and no installer story for non-developers — you build it with `cargo`
and run it as a daemon.

DSH Desktop is a **distribution shell**: it implements no agent, no model
provider, no tool, and no memory. It pins an unmodified upstream agent runtime
(DeepSeek Harness `0.1.0-rc.7`) as a git submodule and wraps it in an Electron
application with a tray, terminal, profile manager, auto-updater, crash
recovery, a plugin marketplace, and signed/notarized installers for macOS and
Windows.

If you drew one architecture diagram, RustyKrab would be the box labeled
"agent host" and DSH Desktop would be the box labeled "desktop client" wrapped
around a *different* agent host. The genuinely comparable surfaces are narrow:
local process supervision, plugin/tool extensibility, credential handling, SSRF
defense, and engineering discipline. Everything else is apples to oranges.

The most useful framing for this repo: **RustyKrab is stronger where the
intelligence lives (agent loop, tools, memory, multi-channel reach); DSH Desktop
is stronger at everything that happens after the code works (packaging,
recovery, upgrade, documentation, contributor process).**

---

## 2. Identity and provenance

| | RustyKrab | DSH Desktop |
|---|---|---|
| What it is | Security-first modular AI agent gateway | Community desktop client for DeepSeek Harness |
| Language | Rust (edition 2021, MSRV 1.88) | TypeScript 6 / Node 22+ / Electron 43 |
| Owns the agent loop? | **Yes** — written from scratch | **No** — delegates to pinned upstream |
| Relationship to a vendor | Independent; Anthropic + Ollama are backends | Independent community fork; explicitly disclaims any affiliation with DeepSeek |
| Origin story | Rewrite of a Node.js predecessor to fix architectural security flaws | Fork of `deepseek-ai/deepseek-harness`, repurposed as a desktop product workspace |
| License | MIT | MIT |
| Visibility | Private/personal-scale project, one primary author | 15.1k stars, 715 forks, multi-contributor, public release channel |

DSH Desktop is unusually explicit about provenance. The README states in the
header that the project is not affiliated with, endorsed by, or staffed by
DeepSeek, and that the GitHub contributor list is inherited from fork history
rather than representing upstream participation. `upstream.json` pins the exact
upstream commit and version, and `AGENTS.md` forbids editing
`deepseek-harness/` from a desktop feature branch. That is a discipline
RustyKrab has no analogue for — but also no need for, because it vendors
nothing.

---

## 3. Quantitative profile

All figures measured directly from the checked-out trees.

| Metric | RustyKrab | DSH Desktop |
|---|---|---|
| Own source | 54,547 lines Rust, 151 files, 10 crates | 25,005 lines TS (14,537 desktop + 10,468 market), 2 shipped packages |
| Test code | ~9,566 lines in `#[cfg(test)]` modules (≈17.5% of source) | 23,238 lines in dedicated test dirs (≈93% of source size) |
| Test cases | 483 `#[test]` / `#[tokio::test]` functions | 714 `it()`/`test()` cases (500 desktop + 214 market) |
| Test-to-source ratio | ~0.18:1 | ~0.93:1 |
| Direct+transitive deps | 636 crates in `Cargo.lock` | 1,002 resolutions in `yarn.lock`, **plus** the upstream submodule's own pnpm tree, **plus** a bundled Electron/Chromium and Node runtime |
| Markdown docs | 6 files | 107 files, ~108k words (bilingual zh/en → ~54k per language) |
| Architecture decision records | none | 69 files under `.agents/notes/` (proposed/implemented, tri-lingual per note) |
| CI jobs | 4 + gate (check, clippy, test, fmt, cargo-audit) | 5 (change classifier, Linux full gate, Windows packaging, macOS packaging, upstream toolchain smoke) |
| CI platforms | ubuntu-latest only | ubuntu + windows + macOS runners |

Two numbers deserve comment.

**Test ratio.** DSH Desktop writes roughly as much test code as product code and
runs it on all three platforms. RustyKrab's 483 tests are respectable and
well-distributed (138 in tools, 105 in agent, 63 in core), but they are unit
tests colocated in modules; there is no integration suite, no end-to-end
gateway test, and no cross-platform matrix despite the codebase branching on
macOS vs Linux in the sandbox, keychain, and codesigning paths.

**Dependency surface.** RustyKrab's 636 crates is lean for what it does, and the
release profile (`lto = true`, `codegen-units = 1`, `strip`, `panic = "abort"`)
plus a rustls-only TLS story means no OpenSSL and a single static binary. DSH
Desktop ships an entire browser engine and two package managers (yarn for the
workspace, a bundled pnpm 11.7.0 for runtime plugin installs). That is inherent
to being a desktop app that installs plugins at runtime, but the attack surface
difference is roughly two orders of magnitude.

---

## 4. Architecture

### 4.1 RustyKrab — a layered Rust workspace

Ten crates with clean dependency direction: `core` defines the `Tool` and
`ModelProvider` traits plus `Capability`/`Session`; every other crate depends
inward on it; `cli` is the only crate that knows about concrete implementations
and wires them into adapter structs (`MemoryAdapter`, `CronAdapter`) that
satisfy the tool backend traits.

```
cli ──┬── gateway (axum: auth, rate_limit, origin, SSE, webchat)
      ├── agent   (runner 5,124 LOC, harness profiles, compaction, sandbox, RLM, voting)
      ├── providers (anthropic, ollama, backoff, line_buffer)
      ├── tools   (60 Tool impls across 64 files, 20,626 LOC — the center of mass)
      ├── channels (telegram, signal, slack, webchat, video, mcp, mcp_http)
      ├── memory  (vector + BM25 + graph + temporal, RRF fusion, 5-stage lifecycle)
      ├── store   (rusqlite WAL: conversations, secrets, jobs, chat maps, recall archive)
      ├── skills  (SKILL.md loader, ed25519 verifier)
      └── core    (traits, capability, session, schema validation, todo, recall)
```

The interesting engineering is concentrated in two places:

- **`agent/runner.rs` (5,124 LOC)** — the loop. It implements progressive tool
  disclosure: only meta-tools (`tools_list`, `tools_load`, `recall_*`) plus a
  small seeded set are in the schema at turn 0, and the model activates the rest
  per-conversation. That is a real context-economy design, and the reasoning is
  documented in-code at length. It also handles compaction with
  re-summarization passes bounded by provider-aware context budgets.
- **`memory/`** — four retrieval strategies run concurrently and fuse with
  Reciprocal Rank Fusion, on top of a five-stage lifecycle
  (Working → Episodic → {Semantic | Archival} → Tombstone) driven by
  importance-modulated exponential decay. This is the most novel component in
  either repository.

### 4.2 DSH Desktop — a generation-scoped Electron host

DSH Desktop's architecture doc describes a "thin Electron host," and the code
matches. The Host (upstream Cordis plugin graph) runs *in Electron's main
process*, binds a loopback HTTP+WebSocket port, and the renderer loads that
same-origin page in a fully sandboxed `BrowserWindow`. There is deliberately no
second IPC plugin system and no raw Electron API exposed to the page — the
preload script exposes exactly one function (`webUtils.getPathForFile`) for
resolving drag-and-drop payloads.

The organizing concept is the **generation**: every profile or mode switch
disposes the current `ElectronShellGeneration` — which solely owns its
`BrowserWindow`, `Tray`, listeners, navigation restrictions, and zoom shortcuts
— before starting the next. Service references, window objects, and subprocess
handles must not be cached across generations. Platform differences are
isolated behind an `ElectronPlatformStrategy` seam chosen once at startup.

The file layout reveals where the effort went. The five largest source files are
`install-recovery.ts` (36.7 KB), `startup-recovery-window.ts` (35.2 KB),
`desktop-plugins.ts` (31.2 KB), `main.ts` (30.6 KB), and `desktop-terminal.ts`
(30.1 KB). Two of the top five are **failure recovery**. There is nothing
comparable anywhere in RustyKrab.

### 4.3 Structural contrast

| Concern | RustyKrab | DSH Desktop |
|---|---|---|
| Process model | Single long-lived daemon, `tokio::spawn` tasks tracked in `infra_handles` for graceful shutdown | Electron main process hosting a Cordis graph, plus supervised subprocesses (pnpm, node-pty terminals) |
| Isolation boundary | In-process; tool calls are Rust trait objects | Main/renderer split with `contextIsolation`, `sandbox`, `nodeIntegration: false` on every window |
| State ownership | SQLite (WAL) in the data dir | Profile directories on disk, each an independent pnpm workspace |
| Restart semantics | Restart the daemon | Generation dispose → relaunch, with last-known-good commit and at-most-one automatic retry |
| Failure handling | Errors propagate to the model as tool errors; the daemon keeps running | Crash evidence capture, diagnostic export worker, recovery window UI, config-image rollback |

---

## 5. The agent layer

This is where the comparison is most lopsided, because DSH Desktop has no agent
layer of its own.

**RustyKrab implements:**

- Two providers (`anthropic.rs`, `ollama.rs`) with streaming, tool-use, backoff,
  and a line-buffer for SSE reassembly.
- Progressive tool disclosure with a per-conversation active-tool registry that
  survives compaction.
- Provider-aware compaction with a hard context ceiling, an effective summary
  cap of `max_context_tokens / 4`, up to 3 re-summarization passes, then
  truncation.
- Harness profiles (`coding`, `research`, `creative`) that select system
  prompts, tool sets, and limits, plus a `HarnessRouter`.
- Sub-agent orchestration (`sessions_spawn/send/yield/list/history`,
  `subagents`, `agents_list`) gated behind a dedicated `Subagent` capability.
- A recursive-language-model module (`rlm/`) with its own context manager and
  REPL tools, and a `voting.rs` for multi-sample consensus.
- A per-conversation recall archive so content can be stashed outside the prompt
  and searched back in after compaction.
- 60 `Tool` implementations spanning filesystem, patching, exec, browser (CDP
  via chromiumoxide), web fetch/search, HTTP sessions, email (IMAP+SMTP),
  CalDAV, Notion, Obsidian, Gmail, media (image/canvas/video), cron, computer
  use, MCP connector, and credential read/write.

**DSH Desktop implements:** none of the above, by design. It contributes two
Host services to the upstream graph — `desktopProfiles` (read active profile,
list selectable profiles, request a safe switch) and `desktopPnpm` (run bundled
pnpm / `dsh plugin` semantics) — and a desktop-owned Client layout, tray,
terminal, and update lifecycle.

What DSH Desktop *does* do at the agent layer is telling: it carries five
surgical patches against upstream packages via yarn `resolutions`. One is a real
correctness fix in the DeepSeek LLM adapter's streaming translator:

```diff
-if (call.id !== void 0) block.callId = call.id;
-if (call.function?.name !== void 0) block.name = call.function.name;
+if (call.id) block.callId = call.id;
+if (call.function?.name) block.name = call.function.name;
```

Empty-string `id`/`name` fields in streamed tool-call deltas were overwriting
accumulated values. Another suppresses a flashing console window in the Windows
ACL sandbox spawner (`dwFlags: 256 → 257`, `wShowWindow: 0`). A third fixes
`set-key-partition-list` receiving the cert password instead of the keychain
password in `app-builder-lib`'s macOS signing path. These are the fingerprints
of a downstream integrator doing careful work at the seams rather than
reimplementing.

---

## 6. Extensibility models

| | RustyKrab | DSH Desktop |
|---|---|---|
| Primary unit | `Tool` trait impl compiled into the binary | npm package loaded into the Cordis plugin graph at runtime |
| Runtime extension | MCP connector (stdio + HTTP), SKILL.md files | Full plugin install/uninstall/enable/disable from a built-in marketplace |
| Discovery | `tools_list` / `tools_load` by the model | Human browsing a catalog UI with four views |
| Recompile to extend? | Yes, for native tools | No |
| Install verification | Ed25519 verifier exists (see §7.4) | Multi-stage: npm registry identity, repository backlink, integrity, exact stable version, runtime compat, lifecycle-script rejection, product blocklist |

DSH Desktop's `dsh-community-market` is the single most impressive non-obvious
piece of engineering in either repo. It is 10,468 lines with 8,324 lines of
tests, and its threat model is spelled out to an unusual degree:

- **The catalog is untrusted.** Provider-supplied command strings and install
  snippets are discarded, never displayed as a Host hint, and never executed. If
  a manual hint is shown, the Host *reconstructs* the npm command from
  normalized identity and marks it as unverified.
- **The renderer is untrusted.** It submits only source/item or receipt
  identifiers; it never passes a command to a Desktop action.
- **Fail-closed candidacy.** The "Installable" list requires reviewed provider
  verification with a `repository_backlink`, an exact stable npm version, and a
  canonical repository. Preview re-verifies against the official registry;
  execution re-checks mutable facts again immediately before running.
- **Lifecycle scripts are a hard reject** — any target manifest defining
  `preinstall`, `install`, `postinstall`, or `prepare` is refused.
- **Protected installs are transactional.** A snapshot of exactly three
  allowlisted files (`package.json`, `pnpm-lock.yaml`, `pnpm-workspace.yaml`) is
  taken; the install stays *pending* until the next generation boots the Host and
  the renderer reports healthy within 30 seconds; a failed startup saves
  diagnostics, restores only a recognized before/after image, and relaunches at
  most once. Unknown file drift is never overwritten.
- **SSRF defense** in `network/restricted-http.ts`: DNS resolution is pinned,
  results are checked against an IPv4/IPv6 `BlockList` covering RFC1918,
  loopback, link-local, CGNAT, benchmark (198.18/15), and multicast ranges, with
  bounded redirects (3), body size (2 MB), and layered timeouts (8s connect /
  12s first byte / 30s total).
- **Honest limits.** The docs state plainly that these checks establish identity
  and compatibility and do *not* review plugin code for malicious behavior, and
  that installed plugins run with the user's full permissions.

RustyKrab's comparable surface — `tools/security.rs` — is genuinely good and
covers the same SSRF class (private-range blocking, explicit
`169.254.169.254` metadata-endpoint block as both IP and hostname, post-resolution
re-validation), and is applied consistently across `web_fetch`, `http_request`,
`http_session`, `browser/`, and `image`. But there is no equivalent of the
market's install transactionality because RustyKrab does not install anything at
runtime.

---

## 7. Security model — the head-to-head

Both projects lead with security in their README. The claims are worth checking
against the code, in both directions.

### 7.1 Where RustyKrab is genuinely strong

- **Secrets at rest.** `store/secret.rs` uses AES-256-GCM with a per-secret
  random 16-byte salt and 12-byte nonce, key derived per-secret from the master
  key via **Argon2id**, packed as `salt || nonce || ciphertext+tag`, with
  `Zeroizing` on derived keys. That is a correct construction, and better than
  the README's own description of it ("HMAC-SHA256 derived keystream"), which
  appears to describe an earlier implementation.
- **Gateway hardening.** Loopback-only bind, bearer auth with constant-time
  comparison, the read guard held across comparison to close a TOCTOU with token
  rotation, per-IP sharded sliding-window rate limiting (16 shards, 1024 entries
  each, hard cap against unique-IP floods, stale eviction that preserves
  lockouts), and Origin validation.
- **Capability model.** A 12-variant `Capability` enum with parameterized
  `Tool(String)` and `Channel(String)` grants, and deliberately separated
  concerns — `NetDiscovery` distinct from `HttpRequest`, `Subagent` and
  `ComputerUse` required *in addition to* the corresponding `Tool` grant. That
  second-key design is a good instinct.
- **Credential refs.** `ref:store:` and `ref:keychain:` indirection for MCP
  credentials, resolved inside the connector so resolved values never enter the
  model's context. Store refs are namespaced per server (`mcp.github.*`), so a
  misconfigured env var cannot pull another server's secret.
- **Linux/Docker fail-closed.** The daemon refuses to start without
  `RUSTYKRAB_MASTER_KEY` rather than generating an ephemeral key that would
  silently orphan previously-encrypted secrets.

### 7.2 Where DSH Desktop is genuinely strong

- **Electron hardening is uniform and correct.** Every window —
  compatibility, advanced, *and* the recovery window — sets
  `contextIsolation: true`, `nodeIntegration: false`, `sandbox: true`,
  `webSecurity: true`. The recovery window additionally sets
  `nodeIntegrationInSubFrames: false`, denies all `setWindowOpenHandler`
  requests, and intercepts `will-navigate`.
- **Minimal preload surface.** One exposed function. Not a bag of IPC channels.
- **Supply-chain rigor at install time.** See §6.
- **Secret masking.** A dedicated `mask-secrets.ts` with tests, applied to logs
  and diagnostic exports.
- **CI hygiene.** `persist-credentials: false` on every checkout,
  `permissions: contents: read`, telemetry disabled by env, and explicit
  commentary that no signing secret ever reaches the macOS CI job (the signed
  release is built on a credentialed machine; CI builds an unsigned smoke
  artifact only).

### 7.3 Where DSH Desktop makes a documented trade-off

`electronFuses: { runAsNode: true }` in the build config leaves
`ELECTRON_RUN_AS_NODE` enabled, which weakens one of Electron's hardening fuses
— the packaged Electron binary can be re-invoked as a plain Node interpreter.
This is almost certainly load-bearing (the bundled pnpm and node helpers are
executed through the Electron binary), and the app is signed and notarized so
the primary abuse path is a local attacker who already has code execution. It is
a defensible trade, but it is a real one and is not called out in the security
docs the way other trades are.

### 7.4 Where RustyKrab's README overstates the code

Three claims in the README's security table do not survive a read of the
implementation. Stating them plainly is more useful than restating the table.

**a) "Signed skills — external skill packages require ed25519 signatures."**
`SkillVerifier` in `crates/rustykrab-skills/src/verify.rs` is a correct
implementation, exported from `lib.rs` — and referenced by nothing. The load
path is `loader.rs::load_skills_from_dir` → `load_single_skill`, which parses
SKILL.md frontmatter and validates *requirements*; it never touches a verifier,
and neither `SkillMd` nor `SkillManifest` carries a signature field. In its
current form the ed25519 machinery is dead code and skills are loaded from disk
unverified. For local, operator-authored skills that is a reasonable posture —
but it is not what the README says, and the mitigation row mapping it to a
supply-chain CVE class is not currently earned.

**b) "Sandboxed execution — tool calls run within policy-constrained sandbox
boundaries," mapped to a sandbox-escape CVE.** The `Sandbox` trait has exactly
two implementations. `NoSandbox` is a no-op used in tests. `ProcessSandbox` —
the one wired in `main.rs:828` — calls `validate_tool_policy()` and returns
`Ok(())`. It is a **policy validator, not an isolation boundary**: after it
passes, the tool executes in-process in the daemon with the daemon's full
privileges. Real OS-level isolation does exist, but only inside two tools:
`code_execution` and `sandboxed_spawn` use macOS Seatbelt profiles
(`sandbox-exec`, network denied, writes restricted to a temp dir), Linux
`unshare()` for PID/IPC/network namespaces, and POSIX `setrlimit` for CPU, AS,
FSIZE, and NPROC. That is genuinely good work — it is just not what "every tool
call is sandboxed" implies. The doc comment on the trait is honest about the
intent ("Different backends *can* implement this"); the README is not.

**c) "No shell execution — tools are Rust trait implementations, not shell
command strings."** `tools/exec.rs:282` runs
`tokio::process::Command::new("sh").arg("-c").arg(command)` on a
model-controlled string. The mitigations are real and thoughtfully built —
newline rejection, `$(`/backtick rejection, per-segment allowlist checking
across `|`, `;`, `&&`, `||`, leading `VAR=value` assignment skipping, basename
matching, `env_clear()` with a synthetic `HOME`, and a 120s timeout ceiling —
but this is an allowlisted shell, not the absence of one. Worth noting that
allowlist-over-`sh -c` is a design that has historically leaked (process
substitution `<(...)`, `$'...'` quoting, `${VAR}` expansion, and glob-driven
argument injection are not all covered by the current filter), and that the
allowlist admits interpreters: `ALLOWED_COMMANDS` includes `python3`, `python`,
`node`, `npm`, `npx`, `make`, `cargo`, and `find`, so `python3 -c "..."` or
`find . -exec ... \;` is arbitrary code execution that passes the filter cleanly.
The allowlist meaningfully raises the bar against a confused model; it does not
contain a hostile one.

None of these are catastrophic for a loopback-bound, single-operator daemon.
They matter because the README markets them as CVE-class mitigations, and
because a reader would reasonably conclude that a compromised model turn is
contained. It currently is not — a tool call that reaches `exec`, `write`, or
`process` runs with the daemon's privileges.

### 7.5 Security posture summary

| Dimension | RustyKrab | DSH Desktop |
|---|---|---|
| Crypto at rest | AES-256-GCM + Argon2id per-secret | Delegates to upstream credentials plugin + OS |
| Network auth | Bearer token, constant-time, rate limit, origin check | Loopback same-origin, no auth surface exposed |
| SSRF defense | Strong (`tools/security.rs`), applied broadly | Strong (`restricted-http.ts`), DNS-pinned |
| Renderer/process isolation | None (in-process tools) | Full Electron sandbox on every window |
| Supply chain | Ed25519 verifier present but unwired; `cargo-audit` in CI | Multi-stage install verification, lifecycle-script rejection, pinned upstream, patched deps |
| Blast radius of a bad tool call | Daemon-level (except `code_execution`) | Plugin runs with user privileges — stated plainly in docs |
| Claim/implementation gap | **Present** (§7.4) | Low — docs are conspicuously careful to disclaim what is *not* verified |

The most instructive difference is not technical. DSH Desktop's security
documentation repeatedly says what it does *not* guarantee. RustyKrab's says
what it does. The former is the posture that survives audit.

---

## 8. Distribution and operations

| | RustyKrab | DSH Desktop |
|---|---|---|
| Install path | `git clone && make` (requires Rust toolchain) | Download DMG or NSIS installer, double-click |
| Targets | `x86_64-unknown-linux-gnu` (zigbuild, glibc 2.35 floor), `aarch64-apple-darwin` | macOS Universal (Intel + Apple Silicon), Windows x64 |
| macOS signing | Developer ID codesign + `.app` bundle | Developer ID, `hardenedRuntime: true`, **notarized** |
| Windows | Not built | NSIS installer + portable archive, per-user install, elevation optional |
| Notarization | **No** — no `xcrun notarytool` / stapler step in `release.yml` | `notarize: true` |
| Auto-update | None | `update-checker.ts` + `update-download.ts` + `update-lifecycle.ts` (~37 KB total) |
| Crash handling | Rolling log file under the data dir | Crash evidence capture, diagnostic export worker, recovery window, config rollback |
| Config | Environment variables only, no config files | Profile directories, settings UI, per-profile plugin sets |
| Release automation | Version bump → tag → matrix build → GitHub Release with changelog extraction | Manual signed release on a credentialed machine; CI builds unsigned smoke artifacts |

RustyKrab's release pipeline is more automated than one would expect for a
single-author project — the zigbuild trick to pin the glibc floor at 2.35 while
building on `ubuntu-latest` is a nice piece of work, as is the workaround for
the `macos-14` runner's `cargo` shim dispatching to `rustup-init`. But the
absence of notarization means a macOS user downloading the artifact gets a
Gatekeeper warning, which in practice is the difference between a developer tool
and a product.

The bigger gap is operational maturity. DSH Desktop treats startup failure as a
first-class state with its own window, its own controller (`startup-recovery-controller.ts`,
24 KB), its own state-commit protocol (last-known-good only after a healthy
renderer report inside 30s), and its own test file for each. RustyKrab has no
concept of a failed start that it can recover from — if the daemon won't boot,
the operator reads a log.

---

## 9. Engineering process

**RustyKrab.** CLAUDE.md prescribes `cargo fmt --all -- --check`,
`cargo clippy --workspace --all-targets`, and `cargo test --workspace` before
every commit, enforced in CI with `RUSTFLAGS: -Dwarnings` and a `ci-pass` gate
job designed to be the required status check. `cargo-audit` runs on every PR.
Commit messages are conventional and scoped (`perf(tools):`, `fix(store):`), and
the recent history shows a coherent performance campaign (IMAP session reuse,
HTTP client reuse, regex hoisting, streamed file reads, append-only conversation
storage, lazy embedder, background index rebuild). Single author, 50 commits in
the shallow window since 2026-05-16, PR-numbered up to #485.

**DSH Desktop.** A layered gate: `yarn check` = layout verification + per-package
`check`, where the desktop package's `check` alone is
`build && typecheck && test && verify:closure && verify:cli && verify:loader && verify:profile && verify:licenses`.
Typecheck runs across four separate tsconfigs (src, client, tests, client
tests). CI classifies changed paths first so documentation-only PRs don't leave
required checks pending, then runs the full gate on Linux plus real Windows and
macOS packaging jobs. There is a license verifier that cross-checks
`THIRD_PARTY_NOTICES.md`.

The process difference that stands out most is the **`.agents/notes/` ADR
system**: 69 files split into `proposed/` and `implemented/`, categorized by
`architecture/` and `process/`, each note existing in English, Chinese, and an
`.i18n.yaml` sync manifest, and each dated. `AGENTS.md` requires topology changes
to stay consistent with their owning note. The architecture doc's "Maintainer
reading" section links to the specific notes that own each decision. This is a
level of decision provenance most commercial codebases do not maintain.

RustyKrab's equivalent is inline documentation, and here it deserves real credit:
the comment block above `META_TOOL_NAMES` in `runner.rs` explaining *why*
`task_complete` is deliberately excluded from turn-0 schema (it tempts the model
to call it for greetings), and why `memory_search`/`memory_save` are seeded but
`memory_get`/`memory_delete` stay lazy, is the kind of rationale that usually
gets lost. It just lives in one file instead of a searchable record.

---

## 10. Documentation

| | RustyKrab | DSH Desktop |
|---|---|---|
| Files | 6 | 107 |
| Words | ~6.6k | ~108k (bilingual; ~54k/language) |
| Languages | English | English + Simplified Chinese, with `.i18n.yaml` sync manifests |
| Audience split | Single README for everyone | Explicit user docs (guide, FAQ, why-desktop) vs. maintainer docs (architecture, plugin dev, service contracts, RFCs) |
| Contribution docs | None | `CONTRIBUTING.md`, `CODE_OF_CONDUCT.md`, issue templates, PR template — all bilingual |
| Specs/RFCs | None | 4 RFCs in `dsh-community-fabric` + research notes on VS Code's extension model and mature plugin frameworks |

RustyKrab's README is dense and good — the env var table is genuinely useful,
the Linux/Docker section explains *why* the master key is mandatory rather than
just asserting it, and the MCP credential-ref walkthrough is end-to-end. But it
carries the full weight of onboarding, configuration reference, security
narrative, and architecture, in one file. `MEMORY_ARCHITECTURE.md` is the
exception and it is excellent — schema tables, lifecycle diagram, tunable
defaults, and file pointers.

DSH Desktop's docs include something RustyKrab has no analogue for:
`dsh-community-fabric`, a **draft RFC series** for a unified plugin contract
(manifest/capabilities/events, runtime/presentation/invocation transport,
service providers and composition, provenance validation and diagnostics) with
prior-art research on VS Code's extension model — published as a scaffold that
`AGENTS.md` explicitly forbids from declaring loadable entry points until
schemas and a reviewed reference adapter exist. That is ecosystem-building, not
just documentation.

---

## 11. Where they actually overlap

Stripping away the layer difference, four surfaces are directly comparable:

1. **SSRF defense.** Both implemented it independently and both did it well.
   RustyKrab's is applied to more call sites (10 modules); DSH Desktop's adds
   DNS pinning and layered timeouts that RustyKrab's does not have. Neither is
   clearly ahead.
2. **Local process supervision.** RustyKrab supervises `tokio` tasks and
   sandboxed child processes; DSH Desktop supervises pnpm runs and node-pty
   terminals with full process-tree ownership. DSH Desktop's is more rigorous
   because the failure modes are user-visible.
3. **Credential handling.** RustyKrab is ahead here — Argon2id + AES-GCM per
   secret, namespaced refs, keychain integration, and a `/set` REPL flow that
   keeps values out of the model's context entirely. DSH Desktop delegates to
   upstream and focuses on masking secrets in logs.
4. **Extension trust.** DSH Desktop is far ahead — its install verification
   pipeline is a real, tested, fail-closed system, while RustyKrab's equivalent
   is an unwired verifier.

Everything else — agent loop, memory, model providers, messaging channels,
tray/window/installer/updater — exists in exactly one of the two.

---

## 12. What RustyKrab could take from DSH Desktop

Ordered by value-to-effort, and scoped to things that make sense for a headless
Rust daemon.

1. **Close the README/implementation gap (§7.4).** Cheapest, highest-value
   change in this document. Either wire `SkillVerifier` into
   `load_single_skill` behind a `RUSTYKRAB_REQUIRE_SIGNED_SKILLS` flag, or
   restate the claim as "signature verification available for externally-sourced
   skills." Same for the sandbox row: say "capability policy enforced on every
   tool call; OS-level isolation for `code_execution` and `sandboxed_spawn`,"
   which is both accurate and still impressive. Same for "no shell execution" →
   "allowlisted shell execution with substitution blocking."
2. **Add a "what this does not protect against" section.** DSH Desktop's single
   biggest documentation advantage. One paragraph stating that a tool call runs
   with daemon privileges, that skills are trusted local code, and that the
   allowlist is a speed bump rather than a boundary.
3. **Notarize the macOS release.** `release.yml` already imports a Developer ID
   certificate; adding `xcrun notarytool submit --wait` and `stapler staple` is
   a short step and removes the Gatekeeper wall.
4. **Add a Windows or at minimum a macOS CI job.** The codebase branches on
   platform in `sandboxed_spawn.rs`, `keychain.rs`, and `codesign.sh`, but only
   `ubuntu-latest` ever runs tests. The seatbelt profile tests in particular
   never execute in CI.
5. **Adopt a lightweight ADR habit.** Not 69 tri-lingual notes — but the
   reasoning currently living in `runner.rs` comment blocks (progressive
   disclosure, seeded tool sets, compaction ceilings) deserves a
   `docs/decisions/` directory. It is the knowledge most at risk in a
   single-author project.
6. **Raise the integration-test floor.** 483 unit tests with zero end-to-end
   coverage of the gateway, channel loops, or agent-with-real-tools path is the
   clearest quality gap. A handful of `tests/` integration files exercising
   auth → conversation → tool call → SSE would catch a class of wiring bugs that
   unit tests structurally cannot — the unwired `SkillVerifier` being the
   canonical example.
7. **Formalize a startup-failure path.** Not a recovery *window* — but a
   documented preflight (master key present, DB writable, provider reachable)
   with distinct exit codes beats a stack trace in a log file.

## 13. What DSH Desktop could take from RustyKrab

For symmetry, and because the comparison is otherwise one-directional:

1. **Capability scoping.** RustyKrab's `CapabilitySet` with second-key grants
   (`Subagent` and `ComputerUse` required *in addition to* the tool grant) is a
   sharper model than "installed plugins run with the user's permissions."
   `dsh-community-fabric` RFC 0001 is explicitly reaching for this; RustyKrab
   has a working implementation to study.
2. **Secrets at rest.** Per-secret Argon2id salt + AES-256-GCM with `Zeroizing`
   is a stronger default than delegating to the OS, particularly for a desktop
   app whose profile directories are user-readable.
3. **Dependency minimalism as a security property.** 636 Rust crates producing a
   single stripped static binary versus 1,002 npm resolutions plus Chromium plus
   two package managers is a real difference in auditable surface — one that the
   market's excellent install-time verification does not address, because it
   governs *new* plugins, not the shipped tree.
4. **Non-desktop reach.** RustyKrab's channel abstraction (Telegram, Signal,
   Slack, WebChat, MCP) is precisely the "mobile remote control" feature DSH
   Desktop lists as coming soon.

---

## 14. Verdict

**As engineering artifacts, both are above average for their category, and they
are strong in complementary places.**

RustyKrab is the more *ambitious* system. It implements, from scratch and in
Rust, an agent loop with progressive tool disclosure, a four-strategy hybrid
memory with a real lifecycle model, 60 tools, six transport channels, and a
hardened gateway — with a clean crate boundary discipline that keeps
`core` dependency-free and `cli` as the only wiring point. The memory
architecture and the tool-disclosure design are novel enough to be interesting
on their own. Its weaknesses are the classic single-author ones: documentation
that has drifted ahead of the code in exactly the places that matter most
(security claims), tests that cover units but not seams, and no story for what
happens when a user who is not the author tries to run it.

DSH Desktop is the more *disciplined* system. It is not trying to be clever —
it explicitly refuses to reimplement upstream, refuses to invent a second plugin
format, and refuses to expose Electron to the page. What it does instead is
treat every boundary as a contract, write as much test code as product code,
document what it does not guarantee, and build the unglamorous machinery
(recovery, rollback, update, packaging, i18n, ADRs) that turns working code into
a product 15k people star and download. Its weakness is that its ceiling is set
by a runtime it does not own.

The single most actionable takeaway for this repository is §7.4 and item 1 of
§12: RustyKrab's security engineering is real, and it is undersold by a README
that overstates it. Fixing the gap costs an afternoon and makes every other
claim in the document more credible.

---

*Method: both trees were read directly at the revisions noted above. RustyKrab
figures come from the working checkout; DSH Desktop figures come from a shallow
clone of `master` with the `deepseek-harness` submodule left uninitialized (its
line counts are therefore excluded throughout — only code the desktop project
owns is counted). Star and fork counts are as reported by GitHub on the
examination date.*
