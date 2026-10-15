# One window per local model: verification (2026-10-15)

Base: `main` at `7329d05` (v6.6.0).

## What changed

A local model's settings live in `<data>/models.toml`, read once at startup
(`rustykrab-agent/src/local_model.rs`). `context_window` sizes the Ollama
`num_ctx`, the harness budget and the compaction ceiling together. The chat
runner, the memory distiller, `SubagentRunner`, `LocalWorker`, the resource
slots and the e2e harness all read the model's entry. Before this change,
workers ignored the operator's compaction settings, and the live daemon ran
qwen at a 32K `num_ctx` under a 96,000-token budget.

Retired, with a startup warning if still set: `RUSTYKRAB_NUM_CTX`,
`OLLAMA_NUM_CTX`, `RUSTYKRAB_COMPACTION_CONTEXT_CEILING`,
`RUSTYKRAB_COMPACTION_EXPAND_CTX`, `OLLAMA_KEEP_ALIVE`, and the local-resources
`context_tokens`. For a local model, `RUSTYKRAB_MAX_CONTEXT_TOKENS` and
`harness.toml`'s `max_context_tokens`/`compaction_*` keys are overridden, and
the daemon names each one it overrides.

## Gates

| Check | Result |
|---|---|
| `cargo fmt --all -- --check` | clean |
| `cargo clippy --workspace --all-targets -- -D warnings` | clean |
| `cargo test --workspace --no-default-features --no-fail-fast` | 2,275 passed, 0 failed, 38 ignored |
| `python3 scripts/check_architecture_docs.py --fix` | OK, metrics current |
| `python3 scripts/check_env_docs.py` | OK |
| `scripts/e2e.sh` (scripted, no model) | 66 pass, 7 xfail, 0 fail, 0 xpass |
| `rustykrab-cli models` against a sample file | each entry and the chat model resolved; `RUSTYKRAB_NUM_CTX` named as ignored; a 1,000-token window exits 1 naming the file and table |
| Daemon startup, Ollama provider, a dead Ollama URL | a 1,000-token window stops it naming the file; a 49,152 `[default]` gives `num_ctx`, budget and ceiling of 49,152, with warnings naming `RUSTYKRAB_NUM_CTX` and the overridden `max_context_tokens`, `compaction_strategy`, `RUSTYKRAB_MAX_CONTEXT_TOKENS` |

An earlier workspace run, without `--no-fail-fast`, stopped at
`controller::tests::workers::scenario_02_*` while a model eval and the build
shared the machine. It passed alone 3/3. That is the known drain flake under
load, which #716 fixes further up the stack.

New tests:

- `local_model::tests`: defaults with no file; an entry names only what differs;
  one window sizes budget, ceiling and `num_ctx`; caller sampling survives
  unless the entry names its own; `keep_alive = "server"`; out-of-range and
  unknown keys refused with their table; a malformed file names itself.
- `subagent::tests::a_worker_runs_its_local_model_entry_over_the_definition_preset`:
  a worker on a local model takes its entry's window, strategy and threshold,
  while the loop stays the preset's. A hosted provider keeps the preset.
- `ollama::tests::default_config_pins_one_window_and_reads_no_environment`.
- `models_cmd::tests`: the report names the file, each entry and what the
  chat and worker models resolve to, or the built-in defaults; a file the
  daemon would refuse is an error.
- `local_resources::tests::requires_model_identity_and_still_loads_a_retired_context_tokens`.

## What this does not show

These are configuration and wiring checks. No model ran: this change claims no
behaviour or quality change for any model at a given window. The whole-stack
context evals run separately on top of the restacked PRs.

The memory note in the README was corrected before commit. A 64K run's
33.7 GB resident size had been read as the window's cost, but Ollama's MLX
runner holds about 30 GiB for qwen3.8:27b at 32K too (server.log
`pipeline.go:101`, 29.2 to 29.7 GiB held at 32K on 2026-10-15). The prefix
cache, not the window, sets that plateau. Whether qwen fits 128K needs a
fresh-load measurement.
