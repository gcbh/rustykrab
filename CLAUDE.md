# RustyKrab

Security-first, modular AI agent gateway written in Rust. Supports Telegram, Signal, and WebChat channels.

## Pre-commit checks

Run these before every commit. CI enforces them on PRs to `main`.

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets
cargo test --workspace
```

Fix formatting automatically with `cargo fmt --all`.

## Build

```sh
cargo build -p rustykrab-cli          # debug
cargo build --release -p rustykrab-cli # release
```

## Project structure

Workspace with 10 crates under `crates/`:

- **rustykrab-cli** — Binary entrypoint, daemon management, channel loops
- **rustykrab-core** — Shared traits (`Tool`, `ModelProvider`), error types
- **rustykrab-store** — SQLite persistence (conversations, secrets, scheduled jobs)
- **rustykrab-gateway** — Axum HTTP server, REST API, SSE streaming, security middleware
- **rustykrab-agent** — Agent loop, harness profiles, orchestration pipeline
- **rustykrab-providers** — Model backends (Anthropic Claude, Ollama)
- **rustykrab-tools** — 30+ tool implementations (filesystem, web, cron, media, etc.)
- **rustykrab-channels** — Telegram, Signal, WebChat, Video, MCP adapters
- **rustykrab-memory** — Hybrid retrieval (vector + BM25 + temporal + graph)
- **rustykrab-skills** — SKILL.md loader and Ed25519 verification

## Key patterns

- **Tool trait** (`rustykrab-core/src/tool.rs`): All agent tools implement `Tool` with `name()`, `description()`, `schema()`, `execute()`.
- **Backend traits** (`rustykrab-tools/src/*_backend.rs`): Abstract interfaces for pluggable backends (memory, cron, message, gateway). Concrete implementations live in the crate that owns the dependency (e.g. `JobStore` in `rustykrab-store`).
- **Adapter structs** (`rustykrab-cli/src/main.rs`): Bridge concrete implementations to tool backend traits (e.g. `MemoryAdapter`, `CronAdapter`).
- **Background tasks**: `tokio::spawn` with handles stored in `infra_handles` for graceful shutdown.
- **Database**: SQLite with WAL mode via `rusqlite`. Schema created idempotently in `Store::run_migrations()`.
- **Config**: Environment variables only (no config files). See README.md for the full list.
