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
cargo build -p rustykrab-cli --no-default-features  # no ONNX download (sandboxes/CI)
```

The `embeddings` feature (on by default) pulls fastembed/ONNX, which
downloads a runtime from `cdn.pyke.io` at build time. Disable it in
network-restricted environments — a deterministic `HashEmbedder` stands in.

## End-to-end evaluation harness

```sh
scripts/e2e.sh          # build daemon, boot it, run the scenario suite
```

Boots the daemon on a throwaway data dir and an ephemeral port with a
scripted (no-model, no-network) agent, then drives the scenarios in
`crates/rustykrab-e2e`. Prints a JSON report to stdout and
`e2e-report.json`; exit 0 means green.

Scenarios encoding not-yet-built behaviour are marked **xfail** — the
suite is green while they fail, and a phase ships by flipping its
scenarios to must-pass. An unexpected pass (**xpass**) fails the suite so
the scenario gets promoted. See
`docs/plans/apollo-ios-and-credential-guard.md` §11.

## Project structure

Workspace with 11 crates under `crates/`:

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
- **rustykrab-dream** — Off-cycle self-improvement: read-only outcome analysis (see `DREAMING.md`)

## Key patterns

- **Tool trait** (`rustykrab-core/src/tool.rs`): All agent tools implement `Tool` with `name()`, `description()`, `schema()`, `execute()`.
- **Backend traits**: Abstract interfaces the tools call. A trait whose implementor sits *below* `rustykrab-tools` in the crate graph lives in `rustykrab-core` (`MemoryBackend`), so the crate owning the capability can implement it directly. The rest stay in `rustykrab-tools/src/*_backend.rs`, because their implementors (`rustykrab-cli`, `rustykrab-agent`) already sit above it and a move would buy nothing.
- **Adapter structs** (`rustykrab-cli/src/main.rs`): Bridge concrete implementations to tool backend traits where the binary genuinely adds something — `CronAdapter` merges the calling conversation's channel context into cron args, `MessageAdapter` routes by channel name. A pure pass-through is a sign the trait is in the wrong crate.
- **Background tasks**: `tokio::spawn` with handles stored in `infra_handles` for graceful shutdown.
- **Database**: SQLite with WAL mode via `rusqlite`. Schema created idempotently in `Store::run_migrations()`.
- **Config**: Environment variables only (no config files). See README.md for the full list.
