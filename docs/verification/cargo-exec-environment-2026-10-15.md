# Cargo execution environment

Base: #701 `5d53613a0d12d89bdee88a30cca1aaaa02a63682`.
Branch: `codex/rustup-exec-environment`.

The local host executor inherited PATH but replaced HOME and cleared every
other variable. On this Mac, `cargo --version` consequently exited 1: Rustup
could not select a toolchain because the temporary HOME had no default.
Read-only preflight adding only the Rustup installation location returned
`cargo 1.98.0 (797e8a9bc 2026-08-05)`, exit 0. This was an execution-environment
failure, not evidence that Cargo was absent from the host.

ExecTool now forwards explicit RUSTUP_HOME, or derives the host HOME's `.rustup`.
It resolves relative locations before applying the worker's bound directory.
The child retains its fixed `/tmp/rustykrab-home` HOME. CARGO_HOME, registry
tokens, API keys and arbitrary parent variables remain excluded. No toolchain
installation, user configuration edit, native inference or live daemon change
is part of the repair.

The command environment also forces `RUSTUP_AUTO_INSTALL=0`: project toolchain
overrides and `cargo +<toolchain>` must use an installed toolchain instead of
implicitly downloading into the host Rustup store. This flag was introduced in
[Rustup 1.28.1](https://blog.rust-lang.org/2025/03/04/Rustup-1.28.1/) and is
documented in the [Rustup environment contract](https://rust-lang.github.io/rustup/environment-variables.html).
The execution fixture used Rustup 1.29.0. Older releases may ignore the flag;
this default does not replace filesystem/network permissions or prevent an
explicit environment override in a permitted command.

The existing registered `binary.available/cargo` probe still means version
discovery at the actual host boundary. It does not establish that any requested
repository builds, dependencies are cached or private registries are authorized.
The fixed temporary HOME shares Cargo's default cache across exec calls until
temporary storage is purged. A persistent per-worker Cargo cache under daemon
data is a separate follow-up; the operator's credential-bearing Cargo home is
not inherited to solve cache performance.

Portable subprocess regressions pass for explicit, default and relative
installation paths, isolated HOME, bound working directory and credential-canary
exclusion: three parent cases, each executing the private child fixture. No
process-global environment mutation is used. The child fixture is ignored during
ordinary workspace tests because its parents supply the private environment.

Actual host verification also passed through the same ExecTool API: Cargo
1.98.0 version discovery and `cargo check --offline` on a dependency-free Rust
library, each exit 0. The fixture used synthetic parent credential canaries;
CARGO_HOME, registry tokens and API keys did not reach the command environment.
The host Rustup settings hash and toolchain-entry listing hash were unchanged
before/after that execution. It installed nothing and edited no operator
configuration.

Three negative controls each fail all three subprocess regressions (exit 101):
omit the RUSTUP_HOME forwarding, omit environment clearing, or enable automatic
installation. They catch missing installation binding, exposed synthetic
credential canaries, and an overridden no-auto-install default respectively.
The exact production source was restored, all three regressions passed again,
and the real Cargo version/offline check passed again. The relative-location
fixture initially compared macOS `/var` and `/private/var` spellings; its assertion
now compares canonical locations, with no runtime behavior change for that fix.

Actual missing-toolchain checks use an empty private RUSTUP_HOME and a Cargo
wrapper that confines Rustup's distribution/update URLs to a loopback-only 404
mirror. Through ExecTool, both a project `rust-toolchain.toml` and explicit
`cargo +stable` fail with "not installed": zero mirror requests and no installed
toolchain directories. Controls using the same real Rustup with automatic
installation enabled each attempt two requests to that private mirror. Nothing
is downloaded from the public service or written to the operator's store.
Receipt: `cargo-env-missing-toolchain.json` and its log.

Task receipts: `cargo-env-preflight.json`, `cargo-env-real-exec.json`, and
`cargo-env-negative-controls.json`, with their associated logs. Required local checks passed: **2,333 workspace tests, 0 failed, 32 ignored**
(one ignored test is the private child fixture executed by the three parents),
formatting, warnings-denied workspace/all-target lint, architecture docs and
environment docs. Final source/log hashes are retained in
`cargo-env-verification.json` in the Codex collaboration task. Hosted checks
run separately on the published PR head; no deployment is asserted.

Architecture updated: `crates/rustykrab-tools/ARCHITECTURE.md`, execution
environment responsibility and the historical environment-read count marker.
No new module, dependency, worker, scheduler, resource grant or store schema.
