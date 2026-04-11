# CLAUDE.md

## Project overview

RustyKrab is a multi-crate Rust workspace — an AI agent runtime with HTTP gateway, model providers, tools, communication channels, and a memory system.

## Workspace layout

```
crates/
  rustykrab-core        # Shared types, error types, traits
  rustykrab-store       # SQLite persistent storage + encrypted credentials
  rustykrab-gateway     # Axum HTTP gateway
  rustykrab-agent       # Agent orchestration loop
  rustykrab-providers   # Model providers (Anthropic, Ollama)
  rustykrab-tools       # Built-in tools (browser, email, Notion, etc.)
  rustykrab-channels    # Communication channels (WebChat, Telegram, Signal, Video)
  rustykrab-skills      # Composable skill definitions
  rustykrab-cli         # CLI entrypoint and daemon (the binary crate)
  rustykrab-memory      # Hybrid memory system (vector + BM25 + temporal + graph)
```

All crates inherit version from `[workspace.package]` in the root `Cargo.toml`.

## Build commands

```bash
make build          # release build (+ codesign on macOS)
make debug          # debug build
make clean          # cargo clean
make version        # print current version
cargo test --workspace   # run all tests
cargo clippy --workspace --all-targets   # lint
cargo fmt --all -- --check               # format check
```

## Versioning and releases

This project uses **semantic versioning**. The single source of truth for the version is `version` in the root `Cargo.toml` `[workspace.package]`.

### Automated releases (on every PR merge)

Every PR merged to `main` automatically triggers a release via `.github/workflows/release.yml`:

1. **Version bump** is determined by PR labels:
   - `semver:major` — breaking change (1.0.0 → 2.0.0)
   - `semver:minor` — new feature (0.1.0 → 0.2.0)
   - No label — patch (default) (0.1.0 → 0.1.1)
2. The workflow runs `scripts/release.sh --ci` which updates `Cargo.toml` and `CHANGELOG.md`
3. A `release: vX.Y.Z` commit and `vX.Y.Z` tag are pushed to `main`
4. Release binaries are built for linux-x86_64, macOS-arm64, macOS-x86_64
5. A GitHub Release is created with changelog notes and attached tarballs

### PR titles become changelog entries

The merged PR title is written into `CHANGELOG.md` under the new version heading. Write clear, descriptive PR titles.

For richer changelogs, manually add entries under `## [Unreleased]` in `CHANGELOG.md` before merging — they will be promoted to the new version section automatically.

### Manual / local releases

```bash
./scripts/release.sh patch    # 0.1.0 → 0.1.1
./scripts/release.sh minor    # 0.1.0 → 0.2.0
./scripts/release.sh major    # 0.1.0 → 1.0.0
./scripts/release.sh 2.0.0    # explicit version
git push origin main --tags
```

### Version at runtime

- `rustykrab-cli --version` / `-V` prints version with git hash and build date
- Version banner is logged at startup via `tracing::info!`
- `build.rs` in `rustykrab-cli` embeds git hash, dirty flag, and build date at compile time

## CI

`.github/workflows/ci.yml` runs on every push/PR to `main`:
- `cargo check` + `cargo clippy` (warnings are errors via `-Dwarnings`)
- `cargo test --workspace`
- `cargo fmt --all -- --check`
- Security audit via `rustsec/audit-check`

### Pre-push: run CI checks locally

Before pushing a PR, run the same checks that CI enforces to avoid round-tripping on failures:

```bash
cargo fmt --all -- --check    # formatting
cargo clippy --workspace --all-targets -- -D warnings   # lint (warnings = errors)
cargo test --workspace        # tests
```

All three must pass — CI will block the merge otherwise.

## Code conventions

- Rust edition 2021, MSRV 1.88
- All dependencies go in `[workspace.dependencies]` and crates use `.workspace = true`
- Error handling: `thiserror` for library crates, `anyhow` for the CLI
- Async runtime: `tokio`; HTTP: `axum`; TLS: `rustls` (no OpenSSL)
- Tracing for logging (not `println!` or `log`)
