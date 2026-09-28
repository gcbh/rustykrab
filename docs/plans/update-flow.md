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
   recover.** A journal (`updates/apply-state.json`, the phase and both
   commits) lets the next run finish or roll back first. If the stop step
   fails, the service is started again.
3. **A failure to write the bad record aborts the rollback.** The rollback
   must go on and report the failure with its outcome.
4. **A failed rollback is quiet.** It must write
   `updates/apply-failed.json`, always try to start the service last, and
   make every later run print that file and stop until a person clears it.

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
to `127.0.0.1`, `::1` or `localhost`. Items 2 to 4 and the plist are
still open, so the `RUSTYKRAB_UPDATE_APPLY_UNREVIEWED` gate stays.

## Not yet

- **Build provenance attestation** in the release workflow, so that a
  Linux update has more than a digest to check.
- **Notarization.** The secrets are unset. A bundle that `tar` extracts
  carries no quarantine attribute, so the updater does not need it.
- **What reaches `main`.** Releases are cut from `main`, so the live
  daemon only ever updates to what its owner merged there. The integration
  branch reaches `main` only through the owner.
