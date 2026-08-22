# RustyKrab (Rust)

A security-first, modular AI agent gateway written in Rust. RustyKrab runs a long-lived agent loop backed by Claude (Anthropic) or local Ollama models, exposes it over a loopback HTTP+SSE gateway, and bridges it to Telegram, Signal, Slack, WebChat, and MCP clients. The agent has access to 40+ built-in tools spanning filesystem, web, browser automation, code execution, scheduling, media, and a hybrid retrieval memory (vector + BM25 + temporal + graph).

Built from scratch to address the architectural security flaws in the original Node.js version — no shared-memory single process, no plaintext credentials, no unsandboxed tool execution, ed25519-signed skill packages, and per-conversation least-privilege capability scoping.

## Prerequisites

- **Rust 1.75+** (install via [rustup](https://rustup.rs))
- **One of:**
  - An [Anthropic API key](https://console.anthropic.com/) for Claude
  - [Ollama](https://ollama.com) installed locally for open-source models

## Installation

```bash
git clone https://github.com/gcbh/rustycrab.git
cd rustycrab
make              # release build (+ codesign on macOS)
make debug        # debug build (+ codesign on macOS)
```

The binary will be at `target/release/rustykrab-cli`.

On macOS, `make` automatically ad-hoc codesigns the binary with the
`keychain-access-groups` entitlement required by the Data Protection Keychain.
You can also run `make codesign` or `make codesign-debug` separately.

### macOS LaunchAgent installation

For a signed release bundle, install the app and register its per-user
LaunchAgent with:

```bash
scripts/install.sh /path/to/RustyKrab.app
```

The installer verifies the bundle signature, installs it under
`~/Applications/RustyKrab.app`, preserves the previous bundle for rollback,
and starts `com.gcbh.rustykrab` at login. Use `scripts/uninstall.sh` to remove
the agent while retaining the app and data, or `scripts/uninstall.sh --purge`
to remove the app bundle as well. `--allow-adhoc` is available only for local
development builds; published releases should pass strict signature
verification.

## Quick Start

### Option A: Run with Claude (recommended)

```bash
export ANTHROPIC_API_KEY=sk-ant-your-key-here
cargo run --release -p rustykrab-cli
```

On first launch, the CLI generates and prints an auth token. Save it:

```
  RUSTYKRAB_AUTH_TOKEN=64-char-hex-token
```

### Option B: Run with a local model via Ollama

```bash
# Pull a model (Gemma 4 26B recommended for tool-use)
ollama pull gemma4:26b

# Start RustyKrab with Ollama
export RUSTYKRAB_PROVIDER=ollama
cargo run --release -p rustykrab-cli
```

### Option C: Run against any OpenAI-compatible server

Works with `llama-server` (llama.cpp), mistral.rs, vllm-mlx, `mlx_lm.server`,
LM Studio's headless daemon, exo, and OpenAI-compatible hosted APIs.

```bash
llama-server -m model.gguf --jinja --cache-reuse 256 -np 1 -c 32768 --port 8080

export RUSTYKRAB_PROVIDER=llama-server
export OPENAI_BASE_URL=http://localhost:8080
export OPENAI_MODEL=my-model
cargo run --release -p rustykrab-cli
```

Size the context deliberately: the system prompt plus the built-in tool
schemas is roughly 8k tokens before any conversation, and `-np N` divides the
context between N slots. `--jinja` is required for tool calling.

Point `OPENAI_BASE_URL` at another machine on the LAN or tailnet to run
inference on separate hardware — see [Delegating to a peer node](#delegating-to-a-peer-node)
below to also delegate whole *agent tasks*, not just inference, to that
machine.

#### Tuning the Ollama server for KV-cache reuse

An agent loop re-sends the whole conversation on every iteration, so almost
all of its prompt is a prefix Ollama already evaluated on the previous turn.
Reusing the cached KV for that prefix is the difference between a fast turn
and one that re-reads the entire history. RustyKrab does its part on the
client — it pins one `num_ctx` for the process, sends `keep_alive` so the
runner stays resident, and trims history in infrequent deep cuts rather than
a little every turn — but three settings on the server side matter just as
much:

```bash
# Must be >= RUSTYKRAB_NUM_CTX, or the server silently truncates prompts.
export OLLAMA_CONTEXT_LENGTH=65536

# One slot keeps a single conversation pinned to a single warm cache.
# Raising this splits KV memory across slots and round-robins requests, so
# consecutive turns of one conversation can land on a cold slot.
export OLLAMA_NUM_PARALLEL=1

# Halves KV-cache memory, buying back the VRAM a larger window costs.
# q8_0 requires flash attention and is close to lossless; q4_0 is cheaper
# but degrades quality on long contexts.
export OLLAMA_FLASH_ATTENTION=1
export OLLAMA_KV_CACHE_TYPE=q8_0
```

Keep `OLLAMA_CONTEXT_LENGTH` and `RUSTYKRAB_NUM_CTX` in sync. If the server
window is the smaller of the two, RustyKrab's own trimming budget is too
generous and the server truncates from the front of the prompt — which moves
the truncation point every turn and throws away the cached prefix each time.

#### Where the context window goes

The pinned window is not all available for conversation history. On the 64k
default, a turn is budgeted roughly like this:

| Reservation | Tokens | Why |
|---|---|---|
| `num_predict` (output) | 4,096 | The turn's own response has to fit alongside the prompt |
| Tool schemas | ~1,800–10,000 | Measured per request; grows as the model loads tools |
| Template framing | 512 | Role tags, tool preamble, BOS/EOS |
| **Usable for history** | **~50,000–58,000** | Compaction fires at 85% of this |

Two consequences worth knowing:

- **Compaction runs before trimming, by design.** Compaction summarizes
  displaced history into the `recall_*` archive, so detail stays reachable;
  trimming just drops the oldest turns. RustyKrab derives both budgets from
  the same figure to keep that order. If you see the "history trimming will
  pre-empt compaction" warning, the loaded tool schemas have grown large
  enough to invert it — raise `RUSTYKRAB_NUM_CTX` or load fewer tools.

- **Going above ~64k takes two settings, not one.** `RUSTYKRAB_NUM_CTX` sizes
  the window; `RUSTYKRAB_COMPACTION_CONTEXT_CEILING` caps the budget the agent
  loop will actually grow into, and defaults to 65,536. Raise only the first
  and compaction still fires at ~56k — the extra window goes unused. RustyKrab
  logs a warning when the ceiling is clipping the window, but the pairing is
  easy to miss:

  ```bash
  export RUSTYKRAB_NUM_CTX=131072
  export OLLAMA_CONTEXT_LENGTH=131072
  export RUSTYKRAB_COMPACTION_CONTEXT_CEILING=131072
  ```

  The default is deliberately below what a long-context model advertises.
  A model supporting 256k does not mean 256k is the right operating point:
  the KV cache is allocated for the whole window up front, and attention cost
  grows with how much of it is filled. At startup RustyKrab logs the measured
  footprint for your model and window (`estimated KV cache footprint`, with
  both f16 and q8_0 figures and what the model's native maximum would cost) —
  pick a window from those numbers and your free VRAM rather than from the
  model's advertised limit.

- **Loading tools invalidates the KV cache.** Tool definitions render into the
  prompt *prefix*, ahead of the conversation, so a tool set that changes
  mid-run moves every token after it and forces a full prompt re-evaluation.
  This is the price of the `tools_list` / `tools_load` design, which exists
  because the full catalog is ~10k tokens and cannot be sent every turn. The
  cost is per *change*, not per tool, so loading everything a task needs in
  one `tools_load` call is much cheaper than discovering tools incrementally.
  The `tool set changed since the last request` log line marks each one.

## Configuration

All configuration is via environment variables. No plaintext config files.

| Variable | Default | Description |
|---|---|---|
| `RUSTYKRAB_PROVIDER` | `anthropic` | Model backend: `anthropic`, `ollama`, `scripted` (E2E harness), or an OpenAI-compatible alias (`openai`, `llama-server`, `llamacpp`, `mistralrs`, `lmstudio`, `mlx`, `exo`, `vllm`) |
| `ANTHROPIC_API_KEY` | — | Anthropic API key (required for Claude) |
| `ANTHROPIC_MODEL` | `claude-sonnet-4-20250514` | Claude model to use. The Claude 4.X family (Opus 4.7 `claude-opus-4-7`, Sonnet 4.6 `claude-sonnet-4-6`, Haiku 4.5 `claude-haiku-4-5-20251001`) is recommended for new deployments |
| `ANTHROPIC_CONTEXT_LENGTH` | `200000` | Context window in tokens for the selected Claude model. Anthropic doesn't expose a discovery endpoint, so set this when enabling a non-default window (e.g. the 1M-token beta) so compaction thresholds stay in sync |
| `RUSTYKRAB_MAX_CONTEXT_TOKENS` | `128000` (cloud) / `32000` (ollama) | Context budget used to compute the compaction threshold. Default is provider-aware: 128k for cloud providers (Anthropic) and 32k for local Ollama, where prompt evaluation on consumer GPUs times out long before a 128k window fills. Set to override the default for either provider |
| `RUSTYKRAB_COMPACTION_CONTEXT_CEILING` | `65536` | Hard upper bound on the context window used to compute the compaction threshold. Keeps compaction firing at a sane size even when the backing model advertises a much larger window |
| `RUSTYKRAB_COMPACTION_SUMMARY_MAX_TOKENS` | `8192` | Env-configurable upper bound on the final compaction summary. The effective cap is further bounded by `RUSTYKRAB_MAX_CONTEXT_TOKENS / 4`, so on a 32k local-Ollama deployment the summary stays under 8k regardless of this value. If the summarizer returns a summary larger than the effective cap, it is re-summarized (up to 3 passes) and eventually truncated |
| `OLLAMA_MODEL` | `gemma4:26b` | Ollama model name |
| `OLLAMA_BASE_URL` | `http://localhost:11434` | Ollama server address |
| `RUSTYKRAB_NUM_CTX` | `65536` | Context window pinned on every Ollama request. Takes precedence over `OLLAMA_NUM_CTX`. Clamped down to the model's native context length when that is smaller. The whole window is allocated as KV cache when the model loads, so lower this (or enable KV quantization, below) if the model won't fit in VRAM. Set to `server` to omit the field and let the server's `OLLAMA_CONTEXT_LENGTH` decide — this disables client-side trimming, since the client then cannot know what the server allocated |
| `OLLAMA_NUM_CTX` | — | Legacy alias for `RUSTYKRAB_NUM_CTX`. Used only when `RUSTYKRAB_NUM_CTX` is unset |
| `OLLAMA_KEEP_ALIVE` | `30m` | How long Ollama keeps the model and its KV cache resident after a request (Ollama duration syntax; `-1` for forever). Ollama's own default is 5 minutes, short enough that a sporadically-used gateway reloads the model on most messages. Set to `server` to omit the field |
| `OLLAMA_THINK` | `auto` | Whether to request thinking mode: `true`, `false`, or `auto` to decide from the model tag. Ollama returns a 400 for `think` against models that don't support it, so `auto` only enables it for known thinking families |
| `OLLAMA_VISION` | `auto` | Whether the model accepts image input: `true`, `false`, or `auto` to decide from the model tag |
| `OLLAMA_TIMEOUT_SECS` | `900` | HTTP request timeout for Ollama in seconds |
| `OPENAI_MODEL` | `local-model` | Model name sent to the OpenAI-compatible server |
| `OPENAI_BASE_URL` | `http://localhost:8080` | OpenAI-compatible server address (with or without a `/v1` suffix) |
| `OPENAI_API_KEY` | — | Bearer token; optional, ignored by most local servers |
| `OPENAI_TEMPERATURE` | `0.1` | Sampling temperature |
| `OPENAI_MAX_TOKENS` | `8192` | Max tokens to generate per response |
| `OPENAI_INCLUDE_USAGE` | `1` | Request usage in the final stream chunk; set `0` for servers that reject `stream_options` |
| `RUSTYKRAB_NODES` | unset | JSON array of peer instances the `nodes` tool can delegate to. See [Delegating to a peer node](#delegating-to-a-peer-node) |
| `RUSTYKRAB_NODE_TIMEOUT_SECS` | `900` | Per-request timeout for delegated tasks. Generous by default: a peer running a local model at single-digit tokens/sec can legitimately take minutes |
| `CHROME_CDP_URL` | `ws://127.0.0.1:9222` | Chrome DevTools Protocol endpoint |
| `RUSTYKRAB_AUTH_TOKEN` | auto-generated | Bearer token for API auth |
| `RUSTYKRAB_MASTER_KEY` | auto-generated | Encryption key for secrets at rest |
| `TELEGRAM_BOT_TOKEN` | — | Telegram bot token from @BotFather |
| `TELEGRAM_ALLOWED_CHATS` | — | Comma-separated chat IDs allowed to use the bot |
| `TELEGRAM_WEBHOOK_URL` | — | Public webhook URL (omit for long-polling mode) |
| `TELEGRAM_WEBHOOK_SECRET` | — | Secret token for webhook validation |
| `SIGNAL_ACCOUNT` | — | Your Signal phone number (E.164, e.g. `+1234567890`) |
| `SIGNAL_CLI_URL` | `http://localhost:8080` | signal-cli-rest-api URL (shares its default port with `OPENAI_BASE_URL` — change one if running both) |
| `SIGNAL_ALLOWED_NUMBERS` | — | Comma-separated E.164 numbers allowed to message |
| `SIGNAL_WEBHOOK_URL` | — | Webhook URL (omit for polling mode) |
| `SIGNAL_WEBHOOK_SECRET` | — | Shared secret for webhook validation |
| `RUST_LOG` | `info` | Log level (`info`, `debug`, `rustykrab_gateway=debug`) |
| `RUSTYKRAB_LOG_STDOUT` | auto | Force stdout logging on (`1`) or off (`0`). Default: enabled only when stdout is a terminal. The rolling log file under the data directory is always written |
| `RUSTYKRAB_OUTCOME_CAPTURE` | `0` | Record how each completed run went, and which skill, memories, and tools were in play, into the `outcome_records` table. Observational only — it changes nothing about how the agent behaves. Groundwork for the self-improvement outer loop; see `DREAMING.md` |
| | | When enabled, this also starts a **downtime analysis worker**: read-only, it aggregates recorded outcomes and logs a digest once the system has been quiet for 10 minutes, abandoning a pass if activity arrives mid-flight. It never writes and never calls a model |

### Persisting credentials

To avoid setting env vars every launch:

```bash
# Generate a stable master key (save this somewhere safe)
export RUSTYKRAB_MASTER_KEY=$(openssl rand -hex 32)

# Generate a stable auth token
export RUSTYKRAB_AUTH_TOKEN=$(openssl rand -hex 32)
```

Add these to your shell profile or a secrets manager. The master key encrypts all secrets stored in the database — if you lose it, stored secrets become unreadable.

### Linux / Docker setup

On macOS the master key is stored in the Data Protection Keychain. On Linux and Docker there is no comparable session-persistent backend that's safe to rely on, so **`RUSTYKRAB_MASTER_KEY` is required** — the daemon refuses to start without it. (This is intentional: an auto-generated ephemeral key would render previously-encrypted secrets in `store.db` permanently unreadable on the next restart.)

```bash
# One-time: generate and persist the key somewhere durable.
export RUSTYKRAB_MASTER_KEY=$(openssl rand -hex 32)
```

**systemd unit:** keep the key out of the unit file by sourcing it from a 0600-perm env file:

```ini
# /etc/systemd/system/rustykrab.service
[Service]
EnvironmentFile=/etc/rustykrab/env
ExecStart=/usr/local/bin/rustykrab-cli
Restart=on-failure
```

```bash
# /etc/rustykrab/env  (chmod 0600, owned by the service user)
RUSTYKRAB_MASTER_KEY=<hex>
RUSTYKRAB_AUTH_TOKEN=<hex>
ANTHROPIC_API_KEY=<sk-ant-...>
```

**Docker:** pass the key via `--env-file` or a Docker/Compose secret mounted as a file and exported in the entrypoint:

```bash
docker run --rm \
  --env-file /path/to/rustykrab.env \
  -v rustykrab-data:/var/lib/rustykrab \
  -e RUSTYKRAB_DATA_DIR=/var/lib/rustykrab \
  rustykrab:latest
```

Per-credential values (Anthropic, Notion, Telegram, etc.) come from their `RUSTYKRAB_*`/service-specific env vars — see the table above and the registry at `crates/rustykrab-store/src/registry.rs`. Anything resolved from an env var is also persisted into the encrypted SQLite store on first run, so subsequent restarts only need `RUSTYKRAB_MASTER_KEY` plus whatever you want to rotate.

The `rustykrab-cli keychain` subcommand is macOS-only; on Linux/Docker use env vars or the gateway's secrets API.

## Usage

The gateway listens on `127.0.0.1:3000` (loopback only, by design).

### API Endpoints

```bash
# Health check (no auth required)
curl http://127.0.0.1:3000/api/health

# Create a conversation
curl -X POST http://127.0.0.1:3000/api/conversations \
  -H "Authorization: Bearer $RUSTYKRAB_AUTH_TOKEN"

# List conversations
curl http://127.0.0.1:3000/api/conversations \
  -H "Authorization: Bearer $RUSTYKRAB_AUTH_TOKEN"

# Get a conversation
curl http://127.0.0.1:3000/api/conversations/{id} \
  -H "Authorization: Bearer $RUSTYKRAB_AUTH_TOKEN"

# Delete a conversation
curl -X DELETE http://127.0.0.1:3000/api/conversations/{id} \
  -H "Authorization: Bearer $RUSTYKRAB_AUTH_TOKEN"
```

### Telegram

Talk to your agent through Telegram.

**1. Create a bot** — message [@BotFather](https://t.me/BotFather) on Telegram, use `/newbot`, and save the token.

**2. Get your chat ID** — message [@userinfobot](https://t.me/userinfobot) to find your numeric chat ID.

**3a. Long-polling mode (local dev, no public IP needed):**

```bash
export TELEGRAM_BOT_TOKEN=123456:ABC-DEF...
export TELEGRAM_ALLOWED_CHATS=your_chat_id
cargo run --release -p rustykrab-cli
```

That's it — message your bot on Telegram and the agent responds.

**3b. Webhook mode (production, requires public URL):**

```bash
export TELEGRAM_BOT_TOKEN=123456:ABC-DEF...
export TELEGRAM_ALLOWED_CHATS=your_chat_id
export TELEGRAM_WEBHOOK_SECRET=$(openssl rand -hex 16)
export TELEGRAM_WEBHOOK_URL=https://your-domain.com/webhook/telegram
cargo run --release -p rustykrab-cli
```

If running locally, use ngrok or Cloudflare Tunnel to expose port 3000:

```bash
ngrok http 3000
# Then set TELEGRAM_WEBHOOK_URL=https://xxxx.ngrok.io/webhook/telegram
```

**Security notes:**
- `TELEGRAM_ALLOWED_CHATS` is **required** — without it, the bot denies all messages
- Webhook mode validates the `X-Telegram-Bot-Api-Secret-Token` header
- The webhook endpoint (`/webhook/telegram`) bypasses bearer auth but uses Telegram's own secret token

### Signal (E2E encrypted)

The most secure messaging option. Uses [signal-cli-rest-api](https://github.com/bbernhard/signal-cli-rest-api) as a bridge — all messages are end-to-end encrypted via the Signal protocol.

**1. Run signal-cli-rest-api in Docker:**

```bash
docker run -d --name signal-api \
  -p 8080:8080 \
  -v $HOME/.local/share/signal-cli:/home/.local/share/signal-cli \
  -e MODE=normal \
  bbernhard/signal-cli-rest-api
```

**2. Register your phone number** (one-time setup):

```bash
# Request a verification code via SMS
curl -X POST 'http://localhost:8080/v1/register/+1234567890'

# Verify with the code you received
curl -X POST 'http://localhost:8080/v1/register/+1234567890/verify/123456'
```

**3. Start RustyKrab with Signal:**

```bash
export SIGNAL_ACCOUNT=+1234567890
export SIGNAL_ALLOWED_NUMBERS=+1987654321,+1555555555
cargo run --release -p rustykrab-cli
```

Now message the registered number from an allowed phone — the agent responds via Signal.

**Webhook mode** (optional, for lower latency):

```bash
export SIGNAL_WEBHOOK_URL=http://localhost:3000/webhook/signal
export SIGNAL_WEBHOOK_SECRET=$(openssl rand -hex 16)
cargo run --release -p rustykrab-cli
```

**Security notes:**
- All messages are E2E encrypted by the Signal protocol — neither the server nor RustyKrab sees plaintext on the wire
- `SIGNAL_ALLOWED_NUMBERS` is **required** — without it, the bot denies all messages
- signal-cli-rest-api runs on localhost only — no external exposure
- Use a dedicated phone number for the bot, not your personal number

### WebChat UI

Open `http://127.0.0.1:3000` in a browser for the embedded WebChat interface.

### Terminal chat (`chat` subcommand)

A small REPL client that talks to the running daemon over loopback. Useful
when no messaging channel is configured, and as the safest path for
onboarding new credentials.

```bash
rustykrab-cli chat
```

Slash commands:

- `/set <name>` — prompt for a value with no echo and store it in the
  encrypted local secret store. The value is sent straight to
  `POST /api/secrets` and never enters the model's context, the
  conversation history, or any messaging channel.
- `/set <name> --keychain <service>/<account>` — store in the macOS
  Keychain instead.
- `/list` — list stored secret names (no values).
- `/delete <name>` — delete a secret.
- `/help`, `/quit`.

Anything that isn't a slash command is sent to the agent as a normal
chat turn. The client uses `RUSTYKRAB_AUTH_TOKEN` (or the OS keychain /
encrypted store fallbacks) and `RUSTYKRAB_GATEWAY_URL` (default
`http://127.0.0.1:3000`).

### MCP servers: credential refs

Any `RUSTYKRAB_MCP_<NAME>_TOKEN`, `_HEADER_<K>`, or `_ENV_<K>` value may be
either a literal or a reference resolved at connect time:

```bash
# Encrypted local SecretStore (namespaced to the server)
export RUSTYKRAB_MCP_GITHUB_TOKEN=ref:store:mcp.github.token
export RUSTYKRAB_MCP_DATADOG_HEADER_DD_API_KEY=ref:store:mcp.datadog.api_key

# macOS Keychain (operator-chosen service / account)
export RUSTYKRAB_MCP_NOTION_TOKEN=ref:keychain:Notion/api-token
```

Store refs are namespaced: server `github` can only resolve store keys
that begin with `mcp.github.`, so a misconfigured env var can't pull
another server's credentials. Keychain refs are not namespaced — the
operator's choice of env var is the gate.

Onboarding a token end-to-end:

```text
$ rustykrab-cli chat
> /set mcp.github.token
  value for mcp.github.token (hidden): ***
  ✓ stored in encrypted local store as `mcp.github.token`
  (MCP servers pick up new credentials at next daemon restart.)
> /quit

$ export RUSTYKRAB_MCP_SERVERS=github
$ export RUSTYKRAB_MCP_GITHUB_URL=https://api.githubcopilot.com/mcp/
$ export RUSTYKRAB_MCP_GITHUB_TOKEN=ref:store:mcp.github.token
$ rustykrab-cli   # restart the daemon
```

The resolver runs entirely inside the connector — the model never sees
the resolved values, and they are not surfaced through any tool.

### Delegating to a peer node

A RustyKrab instance can hand a self-contained task to another RustyKrab
instance running on different hardware — useful for a slower machine with a
stronger local model, or simply more compute you'd like the primary to draw
on. This is delegation, not shared execution: the task runs entirely on the
peer, using *its* tools and filesystem, and returns only a text result.

```bash
export RUSTYKRAB_NODES='[
  {
    "id": "m4max",
    "url": "https://your-node.your-tailnet.ts.net",
    "token": "<the node auth token>",
    "description": "M4 Max 32GB — qwen3.8:27b-mlx. Slower but capable; good for self-contained coding tasks with its own checkout at ~/code/rustycrab."
  }
]'
```

The `nodes` tool (`list`, `discover`, `send`) is hidden from the model until
`RUSTYKRAB_NODES` is set. See `scripts/setup-delegation-node.md` for standing
up a node, exposing it safely (Tailscale Serve, not the raw gateway port),
and measured latency expectations — a delegated task on a local model
typically takes minutes, not seconds.

## Architecture

A Cargo workspace of 10 crates under `crates/`:

```
rustykrab-cli          Binary entrypoint, daemon management, channel loops
  |                    Wires concrete backends to tool adapter traits.
  |
  +-- rustykrab-gateway    Axum HTTP server, REST API, SSE streaming, WebChat static files
  |     +-- auth            Bearer token middleware (constant-time comparison)
  |     +-- rate_limit      Per-IP sliding window + lockout (anti-brute-force)
  |     +-- origin          Origin header validation (blocks cross-origin hijacking)
  |
  +-- rustykrab-agent      Agent loop: model call -> tool exec -> repeat
  |     +-- harness         Profile-driven orchestration (system prompts, tool sets, limits)
  |     +-- compaction      Provider-aware context compaction with re-summarization passes
  |     +-- sandbox         Sandbox trait + process-based isolation with policy
  |
  +-- rustykrab-providers  Model provider implementations
  |     +-- anthropic       Claude Messages API with full tool-use, streaming, and 1M-token beta
  |     +-- ollama          Local models via Ollama (Gemma, Qwen, Llama, Mistral, etc.)
  |
  +-- rustykrab-store      SQLite (rusqlite, WAL mode) persistent storage
  |     +-- conversations   CRUD for conversation history
  |     +-- secrets         Encrypted credential storage (HMAC-SHA256 key derivation)
  |     +-- jobs            Scheduled job store backing the cron tools
  |
  +-- rustykrab-tools      40+ built-in tool implementations
  |     +-- filesystem      read, write, edit, apply_patch
  |     +-- exec            sandboxed_spawn, process, code_execution
  |     +-- web             web_fetch, web_search, x_search, http_request, browser/*
  |     +-- memory          memory_save / search / get / delete (hybrid retrieval)
  |     +-- sessions        spawn / send / yield / list / history (sub-agent orchestration)
  |     +-- integrations    notion, gmail, obsidian, mcp_connector
  |     +-- media           image, canvas, video
  |     +-- scheduling      cron (with persistent JobStore backend)
  |     +-- credentials     credential_read / write (gated by capabilities)
  |
  +-- rustykrab-channels   Communication channel abstractions
  |     +-- telegram        Telegram bot (long-polling + webhook + chat allowlist)
  |     +-- signal          Signal via signal-cli-rest-api (E2E encrypted)
  |     +-- slack           Slack Events API adapter
  |     +-- webchat         In-process mpsc-backed channel for the WebChat UI
  |     +-- video           Live video/audio session channel
  |     +-- mcp / mcp_http  Model Context Protocol server (stdio + HTTP)
  |
  +-- rustykrab-memory     Hybrid retrieval: vector + BM25 + temporal + graph
  |
  +-- rustykrab-skills     SKILL.md loader with ed25519 signature verification
  |
  +-- rustykrab-core       Shared types, traits, error types
        +-- Tool trait, ModelProvider trait
        +-- Capability / CapabilitySet (least-privilege session scoping)
        +-- Session (per-conversation isolation with expiry)
```

## Security Model

This rewrite directly addresses the vulnerability classes found in the original Node.js RustyKrab:

| Original CVE Class | Mitigation |
|---|---|
| CVE-2026-22172 (scope self-declaration) | Bearer token auth, no client-declared scopes |
| CVE-2026-32025 (brute-force / ClawJacked) | Rate limiting with lockout + origin validation |
| CVE-2026-25253 (ClawBleed token exfil) | Loopback-only binding, origin checks |
| CVE-2026-32048 (sandbox escape) | Sandbox trait with policy enforcement on every tool call |
| CVE-2026-24763 (command injection) | Typed Tool trait — no shell string concatenation |
| ClawHavoc (supply chain) | Ed25519 skill signature verification |
| Plaintext credentials | Encrypted SecretStore with master key derivation |
| Cross-session leakage | Per-conversation CapabilitySet with least-privilege defaults |

### Design principles

- **Loopback only** — gateway binds to `127.0.0.1`, never `0.0.0.0`
- **Auth by default** — every API endpoint requires a bearer token
- **Encrypted at rest** — secrets use HMAC-SHA256 derived keystream encryption
- **Least privilege** — sessions start with minimal capabilities, tools must be explicitly granted
- **Signed skills** — external skill packages require ed25519 signatures from trusted publishers
- **Sandboxed execution** — tool calls run within policy-constrained sandbox boundaries
- **No shell execution** — tools are Rust trait implementations, not shell command strings

## Development

```bash
# Check all crates
cargo check

# Run with debug logging
RUST_LOG=debug cargo run -p rustykrab-cli

# Run a specific crate's tests
cargo test -p rustykrab-core
```

## License

MIT
