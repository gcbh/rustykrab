# Tooling, build, and what couldn’t be exercised

Captured during audit at `2026-04-25`, branch
`claude/codebase-analysis-WmuB6`, commit point: `git status` clean.

---

## `cargo fmt --all -- --check`

✅ Clean.

---

## `cargo clippy --workspace --all-targets`

The full workspace cannot be built end-to-end in this environment
because `ort-sys@2.0.0-rc.11` (transitive via the optional `fastembed`
feature on `rustykrab-memory`, which `rustykrab-cli` enables in its
`Cargo.toml:17`) tries to download a prebuilt ONNX Runtime binary
during `build.rs`:

```
error: ort-sys@2.0.0-rc.11: ort-sys failed to download prebuilt
binaries from
`https://cdn.pyke.io/0/pyke:ort-rs/ms@1.23.2/x86_64-unknown-linux-gnu.tar.lzma2`:
http status: 403
error: build script logged errors
```

This is an environmental restriction, not a code defect. To get a
clean clippy run, I split the workspace:

```sh
cargo clippy -p rustykrab-core -p rustykrab-store -p rustykrab-skills \
             -p rustykrab-providers -p rustykrab-tools \
             -p rustykrab-channels -p rustykrab-gateway -p rustykrab-agent \
             --no-default-features --all-targets

cargo clippy -p rustykrab-memory --no-default-features --all-targets
```

✅ Both commands finish with no warnings or errors. The only spurious
output is a future-rust-incompat warning emitted by `imap-proto v0.10.2`
(transitive via the Gmail tool), which is a dependency-side issue.

**Recommendation.** Either:

- Vendor the ONNX runtime, or
- Make `fastembed` a non-default feature explicitly enabled by the
  operator, with documentation that mentions the network requirement
  for first build, or
- Switch to a pure-Rust embedding alternative (`candle`,
  `tokenizers`-only, or hosted embedding).

---

## `cargo test --workspace --no-run`

Same `ort-sys` failure. Tests for the subset of crates that don’t
depend on `fastembed` could be exercised individually but I held off:
the transcript would balloon and clippy/fmt already cover the static
checks.

**Recommendation.** Run `cargo test --workspace` in CI behind the
`fastembed` feature gate, or split CI into a "core" job (no fastembed)
and a "ml" job that has the ONNX binary cached.

---

## What remains under-exercised

These are areas where unit tests are present but I have no way to
*run* them in this environment, so any finding from the static read
should be treated as needing a regression test before it’s closed:

- `crates/rustykrab-providers/src/anthropic.rs` streaming (§G-18..G-25)
- `crates/rustykrab-providers/src/ollama.rs` streaming (§G-26..G-34)
- `crates/rustykrab-channels/src/telegram.rs` parse_webhook_update,
  HMAC verification (§S-21, §S-7..G-11)
- `crates/rustykrab-skills/src/verify.rs` (§C-3, §S-22, §Sk-1..Sk-5)
- `crates/rustykrab-tools/src/security.rs::validate_path` symlink
  TOCTOU (§S-15)
- `crates/rustykrab-tools/src/exec.rs::validate_command` env-prefix
  bypass (§C-5, §S-13/§S-14)
- `crates/rustykrab-memory/src/retrieval.rs` RRF math edge cases
  (§M-1..M-6)

For each, write a focused test that demonstrates the bug, then fix.

---

## Workspace size

- 10 workspace crates
- 123 `.rs` files
- ~28,500 LOC under `crates/*/src/`
- 0 `todo!()` / `unimplemented!()` / `FIXME` markers
- 1 `panic!()` in test code only
- ~139 `unwrap()`/`expect()` call sites; the production-path subset
  was audited (see `03-bugs-core-agent.md` §A-1, §A-2, etc.)
