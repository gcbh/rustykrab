# RustyKrab (Rust)

A security-first reimplementation of the RustyKrab AI agent gateway in Rust. Built from scratch to address the architectural security flaws in the original Node.js version — no shared-memory single process, no plaintext credentials, no unsandboxed tool execution.

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

## Configuration

All configuration is via environment variables. No plaintext config files.

| Variable | Default | Description |
|---|---|---|
| `RUSTYKRAB_PROVIDER` | `anthropic` | Model backend: `anthropic` or `ollama` |
| `ANTHROPIC_API_KEY` | — | Anthropic API key (required for Claude) |
| `ANTHROPIC_MODEL` | `claude-sonnet-4-20250514` | Claude model to use |
| `ANTHROPIC_CONTEXT_LENGTH` | `200000` | Context window in tokens for the selected Claude model. Anthropic doesn't expose a discovery endpoint, so set this when enabling a non-default window (e.g. the 1M-token beta) so compaction thresholds stay in sync |
| `RUSTYKRAB_COMPACTION_CONTEXT_CEILING` | `65536` | Hard upper bound on the context window used to compute the compaction threshold. Keeps compaction firing at a sane size even when the backing model advertises a much larger window |
| `RUSTYKRAB_COMPACTION_SUMMARY_MAX_TOKENS` | `8192` | Hard upper bound on the final compaction summary. If the summarizer returns a summary larger than this, it is re-summarized (up to 3 passes) and eventually truncated. Prevents oversized summaries from refilling the context window |
| `OLLAMA_MODEL` | `gemma4:26b` | Ollama model name |
| `OLLAMA_BASE_URL` | `http://localhost:11434` | Ollama server address |
| `OLLAMA_NUM_CTX` | — | Explicit client-side `num_ctx` override. Omitted by default so the server's own `OLLAMA_CONTEXT_LENGTH` (or per-model default) wins |
| `OLLAMA_TIMEOUT_SECS` | `900` | HTTP request timeout for Ollama in seconds |
| `CHROME_CDP_URL` | `ws://127.0.0.1:9222` | Chrome DevTools Protocol endpoint |
| `RUSTYKRAB_AUTH_TOKEN` | auto-generated | Bearer token for API auth |
| `RUSTYKRAB_MASTER_KEY` | auto-generated | Encryption key for secrets at rest |
| `TELEGRAM_BOT_TOKEN` | — | Telegram bot token from @BotFather |
| `TELEGRAM_ALLOWED_CHATS` | — | Comma-separated chat IDs allowed to use the bot |
| `TELEGRAM_WEBHOOK_URL` | — | Public webhook URL (omit for long-polling mode) |
| `TELEGRAM_WEBHOOK_SECRET` | — | Secret token for webhook validation |
| `SIGNAL_ACCOUNT` | — | Your Signal phone number (E.164, e.g. `+1234567890`) |
| `SIGNAL_CLI_URL` | `http://localhost:8080` | signal-cli-rest-api URL |
| `SIGNAL_ALLOWED_NUMBERS` | — | Comma-separated E.164 numbers allowed to message |
| `SIGNAL_WEBHOOK_URL` | — | Webhook URL (omit for polling mode) |
| `SIGNAL_WEBHOOK_SECRET` | — | Shared secret for webhook validation |
| `RUST_LOG` | — | Log level (`info`, `debug`, `rustykrab_gateway=debug`) |

### Persisting credentials

To avoid setting env vars every launch:

```bash
# Generate a stable master key (save this somewhere safe)
export RUSTYKRAB_MASTER_KEY=$(openssl rand -hex 32)

# Generate a stable auth token
export RUSTYKRAB_AUTH_TOKEN=$(openssl rand -hex 32)
```

Add these to your shell profile or a secrets manager. The master key encrypts all secrets stored in the database — if you lose it, stored secrets become unreadable.

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

## Architecture

```
rustykrab-cli          Binary entrypoint, wires everything together
  |
  +-- rustykrab-gateway    Axum HTTP server, REST API, WebChat static files
  |     +-- auth            Bearer token middleware (constant-time comparison)
  |     +-- rate_limit      Per-IP sliding window + lockout (anti-brute-force)
  |     +-- origin          Origin header validation (blocks cross-origin hijacking)
  |
  +-- rustykrab-agent      Agent loop: model call -> tool exec -> repeat
  |     +-- sandbox         Sandbox trait + process-based isolation with policy
  |
  +-- rustykrab-providers  Model provider implementations
  |     +-- anthropic       Claude Messages API with full tool-use support
  |     +-- ollama          Local models via Ollama (Qwen, Llama, Mistral, etc.)
  |
  +-- rustykrab-store      Sled-based persistent storage
  |     +-- conversations   CRUD for conversation history
  |     +-- secrets         Encrypted credential storage (HMAC-SHA256 key derivation)
  |
  +-- rustykrab-tools      Built-in tool implementations
  |     +-- http_request    HTTP client tool (GET/POST/PUT/DELETE/PATCH)
  |
  +-- rustykrab-channels   Communication channel abstractions
  |     +-- signal          Signal via signal-cli-rest-api (E2E encrypted)
  |     +-- telegram        Telegram bot (long-polling + webhook + chat allowlist)
  |     +-- webchat         In-process mpsc-backed channel for the WebChat UI
  |
  +-- rustykrab-skills     Skill system with ed25519 signature verification
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
