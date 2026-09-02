# RustyKrab

Security-first, modular AI agent gateway written in Rust. Supports Telegram, Signal, and WebChat channels.

## Pre-commit checks

Run these before every commit. CI enforces them on PRs to `main`.

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets
cargo test --workspace
python3 scripts/check_architecture_docs.py
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

Workspace with 14 crates under `crates/`:

- **rustykrab-cli** — Binary entrypoint, daemon management, channel loops
- **rustykrab-core** — Shared traits (`Tool`, `ModelProvider`), error types
- **rustykrab-projects** — Immutable conversational-planning domain and deterministic projections
- **rustykrab-store** — SQLite persistence (conversations, secrets, scheduled jobs)
- **rustykrab-gateway** — Axum HTTP server, REST API, SSE streaming, security middleware
- **rustykrab-agent** — Agent loop, harness profiles, orchestration pipeline
- **rustykrab-runtime** — Assembles and runs one turn, independent of transport
- **rustykrab-providers** — Model backends (Anthropic Claude, Ollama)
- **rustykrab-tools** — 30+ tool implementations (filesystem, web, cron, media, etc.)
- **rustykrab-channels** — Telegram, Signal, WebChat, Video, MCP adapters
- **rustykrab-memory** — Hybrid retrieval (vector + BM25 + temporal + graph)
- **rustykrab-skills** — SKILL.md loader and Ed25519 verification
- **rustykrab-e2e** — Black-box evaluation harness (see above)
- **rustykrab-dream** — Off-cycle self-improvement: read-only outcome analysis (see `DREAMING.md`)

## Architecture docs — keep them current

`docs/architecture/` holds the structural review: system overview, data model
(every table, its keys, and which references are deliberately unenforced),
extension seams, and a dead-code audit. Each crate has its own
`ARCHITECTURE.md`.

Before planning or editing a structural change, read the system overview and
the `ARCHITECTURE.md` files for every affected crate from the exact base commit.
Treat them as versioned evidence: re-check load-bearing claims against the code,
and correct any stale prose you encounter in the same change.
Update the affected write-ups in the same PR; architecture documentation is
part of the implementation, not follow-up cleanup.

**If you change the structure, update these in the same change.** Concretely:

- a new crate needs an `ARCHITECTURE.md` — CI fails without one
- moving a trait between crates, adding or dropping a table, index or foreign
  key, splitting or merging a module, or changing a crate's dependencies all
  belong in the relevant doc
- resolving something recorded in `docs/architecture/OPINION.md` means moving
  it to `docs/architecture/05-first-pass-outcome.md`, not deleting it — the
  record of what was wrong is the part worth keeping
- if a doc says something you have just made false, fix the sentence. Do not
  leave it for a later reviewer to rediscover; that is how CLAUDE.md came to
  claim 11 crates when there were 13

Run this before committing. It regenerates the counts and fails on drift:

```sh
python3 scripts/check_architecture_docs.py --fix
```

For the half a script cannot check — whether the docs' *arguments* are still
true — invoke the `architecture-review` skill. It re-derives each claim from
the code instead of trusting the sentence, and at `full` hunts for structural
problems the docs have not caught up with yet.

Everything between the `generated-metrics` markers is generated — files,
lines, tests, dependencies — so never hand-edit it. The prose is yours to keep
true; nothing can check that for you, which is exactly why it is called out
here. In the PR or handoff evidence, name the architecture documents updated,
or explicitly state why the change does not affect documented architecture.

## Key patterns

- **Tool trait** (`rustykrab-core/src/tool.rs`): All agent tools implement `Tool` with `name()`, `description()`, `schema()`, `execute()`.
- **Backend traits**: Abstract interfaces the tools call. A trait whose implementor sits *below* `rustykrab-tools` in the crate graph lives in `rustykrab-core` (`MemoryBackend`), so the crate owning the capability can implement it directly. The rest stay in `rustykrab-tools/src/*_backend.rs`, because their implementors (`rustykrab-cli`, `rustykrab-agent`) already sit above it and a move would buy nothing.
- **Adapter structs** (`rustykrab-cli/src/main.rs`): Bridge concrete implementations to tool backend traits where the binary genuinely adds something — `CronAdapter` merges the calling conversation's channel context into cron args, `MessageAdapter` routes by channel name. A pure pass-through is a sign the trait is in the wrong crate.
- **Background tasks**: `tokio::spawn` with handles stored in `infra_handles` for graceful shutdown.
- **Database**: SQLite with WAL mode via `rusqlite`. Schema created idempotently in `Store::run_migrations()`.
- **Config**: Environment variables only (no config files). See README.md for the full list.
