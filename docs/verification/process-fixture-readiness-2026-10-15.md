# Executable-identity fixture readiness

Base: #704 `e9f31388275de2a563828a165bda12096493c1f6`.

The updater regression `the_system_executable_is_the_real_one_not_argv0`
immediately read the executable of its newly spawned sleep child. Two retained
hosted failures (#695 and #704's single unchanged retry) observed the CLI test
binary instead of `/usr/bin/sleep`. The production check correctly reads kernel
identity, but the fixture had no independent signal that its program had run.

## Executed Linux preflight

A disposable pinned `rust:1` image, network disabled and no host credentials,
ran a minimal std-only harness with the production Linux executable lookup
copied verbatim. Only the anyhow error-context adapter was replaced; the
`/proc/<pid>/exe` lookup and returned path were unchanged.

- Immediate sleep-child lookup: **6 mismatches in 10,000 starts**. Every wrong
  initial path was the harness parent, and all six settled to the expected
  sleep executable within the bounded follow-up observation.
- Shell child that independently writes a readiness marker after execution:
  **zero mismatches/errors in 500 starts**.
- The container was removed. Inputs, source hashes and results remain in the
  Codex task's process-readiness-preflight.json and related logs.

This reproduces the failure pattern and transient image observation. It does
not establish which kernel/libc/Rust implementation detail caused either
historical CI occurrence, or explain the separate Claude quota-test failure.

## Test-only change

The fixture starts `/bin/bash` on macOS and `/bin/sh` on Linux, with a forged
argv-zero installed path and a cleared fixture environment. The
shell writes an independent marker and blocks on its open input pipe. Only
after the marker does the test call the real SystemProcesses lookup, checking
that the path is absolute and canonicalizes to the shell rather than argv-zero.
The readiness wait has a five-second deadline and detects early child exit.
A guard kills and reaps the child even if readiness or assertions fail.

The initial Mac check caught `/bin/sh` launching `/bin/bash` on this machine;
its initial failure is retained alongside the first Linux test-body results.
The fixture now selects bash directly on macOS so its expected path names the
actual image. The production identity function is unchanged.

The test does not retry until the lookup matches its expected value. Readiness
comes from the executed program. Production process identity and updater
authorization are unchanged; no new dependency or runtime path is introduced.

## Verification status

- Final verbatim repository test body and Linux lookup: **500/500 passing**
  executions in std-only tempfile/error-context/Processes scaffolding. This
  exercises the exact test and lookup, not the full Linux Cargo workspace.
- Copied-body argv-zero control fails the unchanged canonical-identity
  assertion. Never-ready control fails after the five-second deadline; its
  child is independently confirmed absent from /proc after unwinding.
- The actual macOS repository regression passes. Full workspace: **2,333
  passed, zero failed, 32 ignored**, one test thread. Warnings-denied workspace/
  all-target lint, formatting and architecture/environment checks pass.
- Production apply.rs is byte-identical to the pinned base. Both disposable
  Linux experiments were network-free and cleaned up; source and log hashes
  are retained in the Codex task process-readiness-verification.json.

No source change to provider startup, production process inspection, service
ownership or updater authorization. No main merge or live deployment.
