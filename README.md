# OpenClaw (Rust)

A security-first reimplementation of the OpenClaw AI agent gateway in Rust. Built from scratch to address the architectural security flaws in the original Node.js version — no shared-memory single process, no plaintext credentials, no unsandboxed tool execution.

## Prerequisites

- **Rust 1.75+** (install via [rustup](https://rustup.rs))
- **One of:**
  - An [Anthropic API key](https://console.anthropic.com/) for Claude
  - [Ollama](https://ollama.com) installed locally for open-source models

## Installation

```bash
git clone https://github.com/gcbh/rustycrab.git
cd rustycrab
cargo build --release
```

The binary will be at `target/release/openclaw-cli`.

## Quick Start

### Option A: Run with Claude (recommended)

```bash
export ANTHROPIC_API_KEY=sk-ant-your-key-here
cargo run --release -p openclaw-cli
```

On first launch, the CLI generates and prints an auth token. Save it:

```
  OPENCLAW_AUTH_TOKEN=64-char-hex-token
```

### Option B: Run with a local model via Ollama

```bash
# Pull a model (Qwen 3 32B recommended for tool-use)
ollama pull qwen3:32b

# Start OpenClaw with Ollama
export OPENCLAW_PROVIDER=ollama
cargo run --release -p openclaw-cli
```

## Configuration

All configuration is via environment variables. No plaintext config files.

| Variable | Default | Description |
|---|---|---|
| `OPENCLAW_PROVIDER` | `anthropic` | Model backend: `anthropic` or `ollama` |
| `ANTHROPIC_API_KEY` | — | Anthropic API key (required for Claude) |
| `ANTHROPIC_MODEL` | `claude-sonnet-4-20250514` | Claude model to use |
| `OLLAMA_MODEL` | `qwen3:32b` | Ollama model name |
| `OLLAMA_BASE_URL` | `http://localhost:11434` | Ollama server address |
| `OPENCLAW_AUTH_TOKEN` | auto-generated | Bearer token for API auth |
| `OPENCLAW_MASTER_KEY` | auto-generated | Encryption key for secrets at rest |
| `RUST_LOG` | — | Log level (`info`, `debug`, `openclaw_gateway=debug`) |

### Persisting credentials

To avoid setting env vars every launch:

```bash
# Generate a stable master key (save this somewhere safe)
export OPENCLAW_MASTER_KEY=$(openssl rand -hex 32)

# Generate a stable auth token
export OPENCLAW_AUTH_TOKEN=$(openssl rand -hex 32)
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
  -H "Authorization: Bearer $OPENCLAW_AUTH_TOKEN"

# List conversations
curl http://127.0.0.1:3000/api/conversations \
  -H "Authorization: Bearer $OPENCLAW_AUTH_TOKEN"

# Get a conversation
curl http://127.0.0.1:3000/api/conversations/{id} \
  -H "Authorization: Bearer $OPENCLAW_AUTH_TOKEN"

# Delete a conversation
curl -X DELETE http://127.0.0.1:3000/api/conversations/{id} \
  -H "Authorization: Bearer $OPENCLAW_AUTH_TOKEN"
```

### WebChat UI

Open `http://127.0.0.1:3000` in a browser for the embedded WebChat interface.

## Architecture

```
openclaw-cli          Binary entrypoint, wires everything together
  |
  +-- openclaw-gateway    Axum HTTP server, REST API, WebChat static files
  |     +-- auth            Bearer token middleware (constant-time comparison)
  |     +-- rate_limit      Per-IP sliding window + lockout (anti-brute-force)
  |     +-- origin          Origin header validation (blocks cross-origin hijacking)
  |
  +-- openclaw-agent      Agent loop: model call -> tool exec -> repeat
  |     +-- sandbox         Sandbox trait + process-based isolation with policy
  |
  +-- openclaw-providers  Model provider implementations
  |     +-- anthropic       Claude Messages API with full tool-use support
  |     +-- ollama          Local models via Ollama (Qwen, Llama, Mistral, etc.)
  |
  +-- openclaw-store      Sled-based persistent storage
  |     +-- conversations   CRUD for conversation history
  |     +-- secrets         Encrypted credential storage (HMAC-SHA256 key derivation)
  |
  +-- openclaw-tools      Built-in tool implementations
  |     +-- http_request    HTTP client tool (GET/POST/PUT/DELETE/PATCH)
  |
  +-- openclaw-channels   Communication channel abstractions
  |     +-- webchat         In-process mpsc-backed channel for the WebChat UI
  |
  +-- openclaw-skills     Skill system with ed25519 signature verification
  |
  +-- openclaw-core       Shared types, traits, error types
        +-- Tool trait, ModelProvider trait
        +-- Capability / CapabilitySet (least-privilege session scoping)
        +-- Session (per-conversation isolation with expiry)
```

## Security Model

This rewrite directly addresses the vulnerability classes found in the original Node.js OpenClaw:

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
RUST_LOG=debug cargo run -p openclaw-cli

# Run a specific crate's tests
cargo test -p openclaw-core
```

## License

MIT
