# Release pipeline

## Responsibility

`.github/workflows/release.yml` turns a merged pull request or an explicit
manual dispatch into one versioned release. `scripts/release.sh` owns the
deterministic repository mutation: workspace version, changelog entry,
`Cargo.lock`, commit, and annotated tag. The workflow owns GitHub credentials,
pushes, cross-platform builds, signing, notarization, artifacts, and the GitHub
Release object.

The merge-triggered path amends the merge commit and force-pushes with a lease,
so the version and changelog describe the exact released tree. A manual release
uses a separate release commit because it does not own an existing pull-request
merge commit.

## Atomic-stack ownership

An atomic GitHub stack merge closes every member pull request and therefore
starts one workflow per member. Only the top member represents the final tree.
Before any repository mutation, each run fetches `origin/main` and compares its
event's `merge_commit_sha` with the current main tip. The matching run becomes
the release owner; non-tip members report a notice and skip bump, build, and
publication. A manual dispatch is always its own release owner.

This gate is deliberately before the version bump. Relying on concurrent
`--force-with-lease` pushes would make one run win accidentally while every
other run failed, and serializing those runs would incorrectly publish one
version per stack layer.

## Reproducibility invariant

Workspace package versions are materialized in both `Cargo.toml` and
`Cargo.lock`. After changing `Cargo.toml`, the release script runs full
`cargo metadata --format-version 1`; this resolves the lockfile without
compiling dependencies or requiring their native build libraries. The workflow
then runs the same resolution after the amended commit and fails if
`git diff --exit-code -- Cargo.lock` observes any drift.

The script uses `awk` plus same-directory temporary files for version and
changelog edits. That keeps the documented local path portable between GNU and
BSD/macOS userlands; GNU-only `sed -i` semantics are not part of the contract.

Workflow repository access uses the Node 24-based `actions/checkout@v7`; the
release writer obtains its short-lived installation credential through the
Node 24-based `actions/create-github-app-token@v3`. GitHub-hosted runners are
the supported execution environment, so the action majors and runner runtime
advance together instead of relying on GitHub's temporary Node 20 shim.
Evidence and release bundles use `actions/upload-artifact@v7`, and publication
uses `actions/download-artifact@v8` with digest mismatches failing by default.
Release builds restore Rust caches but set `save-if: false`: the workflow's
least-privilege token cannot write Actions caches, and a successful release
must not end with a knowingly unauthorized post-job write attempt.

## Failure boundaries

- A dependency-resolution failure stops before commit or tag creation.
- A dirty lockfile after the commit stops before pushing `main`.
- The leased push stops rather than overwriting an unexpected newer main tip.
- Release builds use the emitted tag, not a moving branch.
- The macOS leg verifies its code signature before publishing and notarizes
  when the Apple credentials are configured.
- The GitHub Release is created only after both target builds complete.
