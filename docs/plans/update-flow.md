# Plan: updating a running RustyKrab

**Status:** Slices 1 to 5 built (2026-09-28); slice 6 merged but held until its review's fixes land
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
| 6 | Supervisor: `rustykrab update apply`, swap, restart, verify, roll back | merged (batch 6); the CLI refuses to run it until the fixes below land |

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

## Slice 6: not yet safe

The first build of slice 6 got the swap order, the atomic renames, the
90 s verify window, the `--yes` gate and the commit-keyed bad record
right. Its security review (2026-09-28) found four problems that keep it
from running. Until they are fixed the CLI refuses
`rustykrab update apply` unless `RUSTYKRAB_UPDATE_APPLY_UNREVIEWED=1`.

1. **It trusts `staged.json`, which a worker can write.** The launchd
   refusal reads `kind` and `signature_verified` from the record, `path`
   is never checked, and the version is not compared with the running
   one. The swap must re-check what it is about to install. It copies the
   stage beside the install, then:
   - refuses a symlink;
   - checks the signature requirement (slice 5) on the copy;
   - runs the copy's `--version` and requires the staged version and
     commit;
   - requires the record's directory and path to be the canonical
     `updates/<X.Y.Z>/<binary or app>`;
   - for a release, requires the version to be newer than the running
     one.

   *Fixed (2026-09-28).* `newest_staged` skips a record that does not
   parse and refuses the newest one unless its directory is its plain
   `X.Y.Z` version and its path is `updates/<version>/RustyKrab.app` or
   `updates/<version>/rustykrab-cli` by kind, with no symlink on the way
   (`check_canonical`). A release (a stage with a tag) must be newer than
   the version `/api/version` reports. `DirSwap::swap_in` copies with
   `cp -Rp --` and, before any rename, passes the copy through
   `NextCheck`: a real directory or regular file, the Developer ID
   signature of the configured team under launchd, and its own
   `--version` reporting the staged version and commit. The `Verifier`
   reaches the swap through `Host`, so the tests script it.
2. **An interrupted apply can leave no daemon, and the next run does not
   recover.** A journal (`.<name>.apply-state.json` beside the install,
   the phase and both commits) lets the next run finish or roll back
   first. If the stop step
   fails, the service is started again.
3. **A failure to write the bad record aborts the rollback.** The rollback
   must go on and report the failure with its outcome.
4. **A failed rollback is quiet.** It must write
   `.<name>.apply-failed.json` beside the install, always try to start
   the service last, and
   make every later run print that file and stop until a person clears it.

Progress:

- **Item 1 is done** (batch 7): the record is canonical and its copy is
  re-checked for signature, version and commit. It only updates a healthy
  daemon, requires a loopback or https URL, and stops only the installed
  binary. Its re-review found one blocking hole: the newer-version check
  applied only to a stage with a tag, and the tag is a field a worker
  writes, so an older signed release recorded as a tagless local build
  could be applied. It was fixed at merge. Every stage must be at least
  the running version.
- **Items 2 to 4 are done** (part 2), with one of the re-review's points:
  the copy's checks move before the daemon is stopped, so a bad stage
  never takes it down. `SwapRoot::swap_in` is split into `prepare` (clear
  `.next`, `cp -Rp --`, `NextCheck`), run while the daemon is up, and
  `commit` (the two renames), run after the stop. The journal, then
  `updates/apply-state.json` (part 2b moved it beside the install) and
  written as a temp file then a rename, holds the phase
  (`stopping`, `swapped`, `started`), both commits and the `bad.json`
  entry. Every run first calls `recover`: an install path missing with
  `.prev` beside it gets `.prev` back; a journal at `swapped` or `started`
  is rolled back in full and the new version recorded bad; then the
  service is started if it is not running and the journal cleared. A run
  that recovers applies nothing new. A failed stop starts the service
  again unless it is still up. A failure writing `bad.json` is logged,
  the rollback goes on, and the outcome reports it. Any rollback error
  writes the failure record, then `updates/apply-failed.json` and now
  beside the install (what failed, both commits, the
  time), clears the journal and last starts the service unless it is
  running; while that file exists every run puts it on stderr, exits
  non-zero and changes nothing. The rollback's stop is retried once; if
  it fails again nothing is restored under the running daemon. The
  LaunchAgent plist gets `ExitTimeOut` 45, above the 20 s drain grace.
  The gate stays until this is re-reviewed and part 3 lands.
- **Part 3 follows** with the re-review's smaller points:
  - read the listener's executable with `proc_pidpath` rather than
    `ps -o comm=`, which prints the process's own `argv[0]`;
  - accept only `127.0.0.1` and `[::1]`, and check the default gateway
    URL too;
  - choose a stage by version rather than by the record's `staged_at`.

- **Part 2 is merged** (batch 8), behind the same gate. The copy's checks
  now run before the stop, and a journal lets the next run recover.
  A rollback survives a bad-record failure, and a failed rollback writes
  a file that blocks later runs. Its review found three more problems:
  1. The journal and the failure file lived in the data directory, where
     a worker could write. A forged journal could make the next run, even
     one without `--yes`, roll a healthy daemon back to `.prev`, and
     deleting the failure file would hide a failed rollback.
  2. A crash between the renames and the journal's `swapped` write started
     the new version unchecked, and later runs then reported nothing to
     do.
  3. Under launchd, a `bootout` that returns while the job is still
     draining made the stop step give up without restarting it.

  Part 2b moves both files beside the install. Before any rollback it
  checks the installed and `.prev` binaries against the journal's
  commits, and it fixes the crash window, the launchd stop and the
  smaller findings.

- **Part 2b is done**, behind the same gate:
  - The journal is `.<name>.apply-state.json` and the failure record
    `.<name>.apply-failed.json`, both beside the install
    (`SwapRoot::state_path`; for the launchd default,
    `~/Applications/.RustyKrab.app.apply-state.json`). Files of those
    names in the data dir are ignored.
  - Before `recover` rolls anything back, it runs the `Verifier`'s
    `--version` on the installed and `.prev` binaries. They must report
    the journal's `to_commit` and `from_commit`. Otherwise it writes the
    failure record and stops nothing.
  - A journal at `stopping` whose `.next` is gone and whose installed
    binary reports `to_commit` is the crash between the renames and the
    `swapped` write, and is rolled back like `swapped`.
  - `apply` checks the running daemon's health before it compares
    commits, so a daemon of the staged commit stuck waiting on the lock
    is not reported as already running.
  - `Launchd::stop` waits for the job to unload even when `bootout`
    errors, and fails only if it is still loaded after 60 s. After a
    failed stop, `apply` clears the journal only once `/api/version`
    answers with the old commit.
  - A journal that does not parse (an unknown phase, say) writes the
    failure record and starts the service.
  - A rollback finding no `.prev` and the installed binary already on
    `from_commit` (one that crashed late) skips the stop and the restore.
    It only starts the service if need be and verifies it.
  - A failed `commit` that leaves nothing at the install path writes the
    failure record. A failure to clear the journal after a failed stop
    or commit is logged, not returned over the original error.

  Moving the files beside the install makes them as hard to write as the
  install itself, and no harder. The same-user limit below still holds.
  The gate stays until part 2b is re-reviewed and part 3 lands.

**The limit of all this.** Workers run as the same macOS user as the
daemon, so "a worker cannot write it" holds only as far as the worker's
tool rules go. Claude Code's edit tools stay inside the worktree, but a
`cargo` build script it may run can write anywhere the user can. These
checks stop a confused worker and make a determined one leave traces.
Isolation that holds against a hostile worker needs a separate user or
sandbox for workers. That is a decision for the owner, and the updater
should not be installed on the live daemon without it being made
knowingly.

Also required:
- The script service stops only a loopback listener whose executable is
  the installed one.
- A daemon that is already failing ticks is not updated.
- The verify step counts a tick advance only when no ticks are failing.
- `--url` must be loopback or https.
- The LaunchAgent plist gets an `ExitTimeOut` above the drain grace.

*Fixed (2026-09-28)*, all but the plist: the script service stops only
the one process listening on the URL's port, and only when it listens on
loopback alone and runs the installed executable (`ps -o comm=`, or
`/proc/<pid>/exe` on Linux); no listener, several, or any other process
is refused. `apply` refuses before any change unless the running daemon
reports `controller.lock` `held` and `consecutive_failed_ticks` 0.
`verify` counts a `last_tick` advance only while no ticks are failing,
and a failing tick starts the count again. `--url` must be https or http
to `127.0.0.1`, `::1` or `localhost`. Items 2 to 4 and the plist
followed in part 2 (above); the `RUSTYKRAB_UPDATE_APPLY_UNREVIEWED` gate
stays until part 2 is re-reviewed and part 3 lands.

## Not yet

- **Build provenance attestation** in the release workflow, so that a
  Linux update has more than a digest to check.
- **Notarization.** The secrets are unset. A bundle that `tar` extracts
  carries no quarantine attribute, so the updater does not need it.
- **What reaches `main`.** Releases are cut from `main`, so the live
  daemon only ever updates to what its owner merged there. The integration
  branch reaches `main` only through the owner.
