# Plan: updating a running RustyKrab

**Status:** Slices 1 to 4 built (2026-09-28); slices 5 and 6 to build
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
| 5 | Release source and verifier: `rustykrab update check` and `rustykrab update stage` | to build |
| 6 | Supervisor: `rustykrab update apply`, swap, restart, verify, roll back | to build, after 5 |

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
  optional (`RUSTYKRAB_GITHUB_TOKEN`), for rate limits only. With
  `--from <path>` the source is a local build instead, either a
  `RustyKrab.app` or a bare binary, which is the builder's case.
- **Digest first.** The release API's `digest` for the asset
  (`sha256:<hex>`) must be present and must equal the SHA-256 of the bytes
  downloaded. A missing or different digest refuses the release. Nothing
  from the download is extracted before this passes.
- **Signature before execution.** The archive is extracted with the
  system `tar` into `<data>/updates/<version>/`. On macOS the staged
  `RustyKrab.app` must pass `codesign --verify --deep --strict`, and
  `codesign -dv` must show `Identifier=com.gcbh.rustykrab` and
  `TeamIdentifier=3RRX845C4X`. The team is pinned, so a correctly signed
  bundle from anyone else is refused, and `RUSTYKRAB_UPDATE_TEAM_ID`
  overrides the pin for a fork. The same team and identifier are what
  keep the Data Protection Keychain readable after the swap. On Linux
  there is no signature to check yet, so the digest is the check.
- **Then `--version`.** Only after the signature passes does the staged
  binary run, once, with `--version`. It must print the release's
  version, and its commit is recorded.
- **The record.** `<data>/updates/<version>/staged.json` holds the
  version, tag, commit, source, digest, path and time. A version recorded
  as bad by a rollback (slice 6) is never staged again without
  `--force`.
- **Tests.** A local HTTP stand-in for the GitHub API serves a release
  and an archive built in the test. Tests cover: a digest mismatch is
  refused, a missing digest is refused, a release that is not newer
  stages nothing, and a good one is staged with its record. The
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

## Not yet

- **Build provenance attestation** in the release workflow, so that a
  Linux update has more than a digest to check.
- **Notarization.** The secrets are unset. A bundle that `tar` extracts
  carries no quarantine attribute, so the updater does not need it.
- **What reaches `main`.** Releases are cut from `main`, so the live
  daemon only ever updates to what its owner merged there. The integration
  branch reaches `main` only through the owner.
