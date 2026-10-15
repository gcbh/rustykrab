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
It installed nothing and edited no operator configuration.

Two negative controls each fail all three subprocess regressions (exit 101):
omit the RUSTUP_HOME forwarding, or omit environment clearing. The first loses
the installation binding; the second exposes the synthetic credential canaries.
The exact production source was restored, all three regressions passed again,
and the real Cargo version/offline check passed again. The relative-location
fixture initially compared macOS `/var` and `/private/var` spellings; its assertion
now compares canonical locations, with no runtime behavior change for that fix.

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
