# Plan: updating a running RustyKrab

**Status:** Slices 1 to 5 built; slice 6 built in four reviewed parts and gated (2026-09-28)
**Builds on:** `control-layer-and-worker-fleet.md`, `self-build-loop.md`

A new version of the daemon has to replace the old one without losing
work: runs in flight end cleanly, the items they held run again, two
daemons never both run the controller, and a release that does not come
up healthy is rolled back. The daemon cannot do this to itself, so the
swap belongs to a separate supervisor. The new version comes from the
release pipeline for the live daemon, or from a local build for the
builder.

## The pieces

| # | Piece | Status |
|---|---|---|
| 1 | SIGTERM runs graceful shutdown; each external run is in its own process group, which shutdown ends | built (batch 3) |
| 2 | `GET /api/version`: version, commit, build date, and the controller's last tick and live runs | built (batch 3) |
| 3 | Drain: on shutdown the controller leases nothing new, gives runs `RUSTYKRAB_DRAIN_SECS` (20) to finish, then ends them as interrupted, returned to `ready` with no rung | built (batch 4) |
| 4 | Controller lock: an exclusive `flock` on `<data>/controller.lock`; only the holder ticks, and `/api/version` reports `held` or `waiting`; failed ticks reported beside it | built (batch 4) |
| 5 | Release source and verifier: `rustykrab update check` and `rustykrab update stage` | built (batch 5), with the review's fixes |
| 6 | Supervisor: `rustykrab update apply`, swap, restart, verify, roll back | built in four reviewed parts; gated, see "Slice 6: status" |

Pieces 3 and 4 were exercised by hand on the builder on 2026-09-28. A
second daemon on the same data directory reported `waiting` and never
ticked. When the first got SIGTERM, it drained and exited, and the second
took the lock within one tick. The e2e scenario
`control/two-daemons-one-data-dir-only-one-ticks` now checks the lock half
of that on every `scripts/e2e.sh` run, as must-pass.

## Slice 5: where a new version comes from, and how it is checked

`rustykrab update check` reports whether a newer release exists.
`rustykrab update stage` downloads it, verifies it and stages it. Neither
touches the running daemon.

- **Source.** GitHub Releases of `RUSTYKRAB_UPDATE_REPO` (default
  `gcbh/rustykrab`), the latest release. It is newer when its tag
  `vX.Y.Z` is above the running `VERSION`, compared as numbers. The asset
  is `rustykrab-<target>.tar.gz` for the target the binary was built for
  (`aarch64-apple-darwin`, `x86_64-unknown-linux-gnu`). A token is
  optional (`RUSTYKRAB_GITHUB_TOKEN`), for rate limits only. It is sent
  only to the API host, never to whatever host the asset URL names, and
  `Config`'s `Debug` output redacts it. A tag that is not a plain
  `vX.Y.Z`, such as the pre-release `v5.4.0-rc.1`, is nothing newer to
  stage, and `check` says so. With
  `--from <path>` the source is a local build instead, either a
  `RustyKrab.app` or a bare binary, which is the builder's case.
- **Size before the download.** The release API's `size` for the asset
  must be present and at most 256 MiB, and the asset URL must be `https`.
  The body is read in chunks and refused as soon as it passes the
  declared size or the cap, or if it ends short of the declared size.
- **Digest first.** The release API's `digest` for the asset
  (`sha256:<hex>`) must be present and must equal the SHA-256 of the bytes
  downloaded. A missing or different digest refuses the release. Nothing
  from the download is extracted before this passes.
- **Signature before execution.** The archive is extracted with the
  system `tar` into `<data>/updates/<version>/`, and nothing in it may be a
  symbolic link. On macOS the staged `RustyKrab.app` must satisfy, under
  `codesign --verify --deep --strict -R`, the requirement
  `anchor apple generic and identifier "com.gcbh.rustykrab" and
  certificate leaf[subject.OU] = "3RRX845C4X"`, plus the Developer ID
  markers on the intermediate and leaf certificates. `codesign` checks
  that against the certificate chain. The `Identifier=` and
  `TeamIdentifier=` lines of `codesign -dv` are not enough on their own:
  they are fields of the signature, and an ad-hoc signature
  (`codesign -s - --team-id 3RRX845C4X`) prints both. A review caught this
  flaw in the first version of this plan. The team is pinned, so a
  correctly signed bundle from anyone else is refused, and
  `RUSTYKRAB_UPDATE_TEAM_ID` overrides the pin for a fork. The same team
  and identifier are what keep the Data Protection Keychain readable after
  the swap. On Linux there is no signature to check yet, so the digest is
  the check.
- **Then `--version`.** Only after the signature passes does the staged
  binary run, once, with `--version`, and it is killed after 10 s. It must
  print the release's version, and its commit is recorded. A version names
  a directory only when it is exactly `X.Y.Z` in digits, so a local build
  printing `..` cannot reach outside `updates/`.
- **The record.** `<data>/updates/<version>/staged.json` holds the
  version, tag, commit, source, digest, path and time. It also holds the
  `kind` (`app` or `binary`) and whether the signature was checked. It is
  written before the stage is moved into place, and a scratch directory
  left by a killed stage is removed by the next one. A version recorded
  as bad by a rollback (slice 6) is never staged again without
  `--force`.
- **Tests.** A local HTTP stand-in for the GitHub API serves a release
  and an archive built in the test. Tests cover: a digest mismatch is
  refused, a missing digest is refused, a release that is not newer
  stages nothing, and a good one is staged with its record. A security
  review added: an asset declared over the cap is refused without a
  download, a body longer or shorter than declared is refused, the token
  does not reach an asset on another host, and a pre-release tag stages
  nothing. The
  signature check sits behind a seam so the tests can script it. One
  macOS test runs the real `codesign` against an unsigned bundle and
  expects a refusal.

## Slice 6: the supervisor

`rustykrab update apply` swaps in the newest staged version. It runs as
its own LaunchAgent (`com.gcbh.rustykrab.updater`, on an interval),
installed by `scripts/install.sh`. It runs from a copy of the binary kept
outside the bundle it replaces, so it is never the process it stops.

1. Read the newest `staged.json`, and record the running version from
   `/api/version`.
2. `launchctl bootout gui/<uid>/com.gcbh.rustykrab`. That sends SIGTERM,
   the daemon drains (piece 3), and the supervisor waits for it to exit.
3. Rename `RustyKrab.app` to `RustyKrab.app.prev`, replacing any older
   one, and move the staged bundle into place. Both renames are in one
   directory.
4. `launchctl bootstrap gui/<uid> <plist>`.
5. Verify within 90 s through `/api/version`:
   - the commit is the staged one;
   - `controller.lock` is `held`;
   - `last_tick` advances twice;
   - `consecutive_failed_ticks` is 0.
6. On any failure, roll back: boot the new version out, restore `.prev`,
   bootstrap, verify the old version the same way, and record the new
   version as bad.

`apply` changes nothing without `--yes` or `RUSTYKRAB_UPDATE_AUTO=1`.
Otherwise it reports what it would do and leaves a notice. For the
builder the steps are the same with a plain binary path and `start.sh`
in place of launchd.

## Slice 6: status (2026-09-28)

Slice 6 was built in four parts, each reviewed for security before it
merged. The CLI still refuses `rustykrab update apply` unless
`RUSTYKRAB_UPDATE_APPLY_UNREVIEWED=1`.

**What `apply` does now:**

- **It trusts nothing in `staged.json`.**
  - The record must be canonical: `updates/<X.Y.Z>/<RustyKrab.app or
    rustykrab-cli>`, with no symlink on the way.
  - Among canonical records, the highest version wins, then the latest
    `staged_at`. A record that is not canonical is skipped.
  - Every stage must be at least the running version, and a release
    must be newer. The downgrade check once depended on the record's
    `tag`, and a worker writes that field.
- **It checks the copy before stopping anything.** The stage is copied
  beside the install (`cp -Rp --`). That copy must not be a symlink, must
  carry the Developer ID signature under launchd, and its own
  `--version` must report the staged version and commit. Only then is
  the daemon stopped and the two renames made.
- **It only updates a healthy daemon.**
  - Before any change, the running daemon must hold `controller.lock`
    with no failing ticks.
  - After the start, verify counts a `last_tick` advance only while no
    ticks fail.
  - URLs must be https, or http to `127.0.0.1` or `[::1]`, and that
    includes the default gateway URL.
  - The script service stops only the one loopback listener whose
    executable (`proc_pidpath`, or `/proc/<pid>/exe`) is the installed
    binary.
- **It recovers.** A journal beside the install records the phase
  (`stopping`, `swapped`, `started`) and both commits. Every run first
  calls `recover`:
  - a missing install gets `.prev` back;
  - an interrupted swap is rolled back, but only after the installed and
    `.prev` binaries report the journal's commits;
  - a crash between the renames and the journal write is recognised;
  - an unparseable journal writes the failure record;
  - a late-crashed rollback is not redone.
  A launchd stop that errors mid-drain waits for the job to unload.
- **It fails loudly.** A failure to write `bad.json` never aborts a
  rollback. A failed rollback writes a failure record beside the install,
  always tries to start the service last, and blocks every later run
  until a person deletes the record. The plist's `ExitTimeOut` (45 s) is
  above the drain grace.

**Before the gate lifts for the builder's script service** (part 4):

- `recover` verifies every service it starts, not just that `start()`
  returned, and writes the failure record if verify fails. The
  commit-failure branch waits for the old commit before clearing the
  journal.
- The installed binary must report the running commit before a swap.
- A journal at `swapped` or `started` whose new version verifies is
  cleared, not rolled back.
- A stage at `stopping` counts as swapped whenever `.next` is gone and the
  installed binary does not report `from_commit`.
- `.prev` passes the symlink and signature checks before a restore.
- An unreadable journal is renamed, not deleted.
- `apply` refuses to run inside the daemon's own launchd job.

**Before launchd, on the owner's machine, it must also:**

- **Stop sending the token to the probe.** Every probe carries the
  daemon's token, which on the live install lives in the Keychain. While
  the port is free during a stop or start, any same-user process that
  binds it gets the token and can answer "healthy", leaving no daemon and
  no failure record. The fix is to serve `/api/version` without
  authentication (it carries only version, commit and controller state),
  and to check that the listener is the launchd job's own process.
- **Have its own job built:** `com.gcbh.rustykrab.updater` in
  `scripts/install.sh`, on an interval, running from a copy of the binary
  kept outside the bundle.
- **Have the owner's decision on worker isolation.** Workers run as the
  same macOS user as the daemon, so "a worker cannot write it" holds only
  as far as the worker's tool rules go. Claude Code's edit tools stay
  inside the worktree, but a `cargo` build script can write anywhere the
  user can. The journal beside `~/Applications` is exactly as writable as
  the install itself. These checks stop a confused worker and make a
  determined one leave traces; they do not stop a hostile one. That needs
  a separate user or a sandbox for workers, and it is the owner's call.

## Not yet

- **Build provenance attestation** in the release workflow, so that a
  Linux update has more than a digest to check.
- **Notarization.** The secrets are unset. A bundle that `tar` extracts
  carries no quarantine attribute, so the updater does not need it.
- **What reaches `main`.** Releases are cut from `main`, so the live
  daemon only ever updates to what its owner merged there. The integration
  branch reaches `main` only through the owner.
