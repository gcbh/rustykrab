#!/usr/bin/env bash
#
# release.sh — Bump the workspace version, update CHANGELOG.md, commit, and tag.
#
# Usage (local):
#   ./scripts/release.sh patch
#   ./scripts/release.sh minor
#   ./scripts/release.sh major
#   ./scripts/release.sh 1.2.3
#
# Usage (CI — called by GitHub Actions on PR merge):
#   ./scripts/release.sh patch --ci --pr-title "Add foo" --pr-number 42
#
# What it does:
#   1. Validates the working tree is clean (skipped in --ci mode)
#   2. Computes the next version (or uses the one you gave)
#   3. Updates workspace version in Cargo.toml
#   4. Moves "Unreleased" entries in CHANGELOG.md under a new version heading
#   5. Runs `cargo check` to regenerate Cargo.lock (skipped in --ci mode)
#   6. Commits and tags as v<version>
#
# After running locally, push with:
#   git push origin main --tags
#
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CARGO_TOML="$REPO_ROOT/Cargo.toml"
CHANGELOG="$REPO_ROOT/CHANGELOG.md"

die() { echo "error: $*" >&2; exit 1; }

# --- Parse arguments ---
BUMP=""
CI_MODE=false
PR_TITLE=""
PR_NUMBER=""

while [[ $# -gt 0 ]]; do
    case "$1" in
        --ci)       CI_MODE=true; shift ;;
        --pr-title) PR_TITLE="$2"; shift 2 ;;
        --pr-number) PR_NUMBER="$2"; shift 2 ;;
        -*)         die "unknown flag: $1" ;;
        *)          BUMP="$1"; shift ;;
    esac
done

[ -n "$BUMP" ] || die "Usage: release.sh <major|minor|patch|X.Y.Z> [--ci --pr-title '...' --pr-number N]"

# --- Ensure clean working tree (local only) ---
if [ "$CI_MODE" = false ] && [ -n "$(git -C "$REPO_ROOT" status --porcelain)" ]; then
    die "working tree is dirty — commit or stash changes first"
fi

# --- Read current version ---
CURRENT=$(sed -n 's/^version = "\(.*\)"/\1/p' "$CARGO_TOML" | head -1)
[ -n "$CURRENT" ] || die "could not read current version from $CARGO_TOML"

IFS='.' read -r MAJOR MINOR PATCH <<< "$CURRENT"

# --- Compute next version ---
case "$BUMP" in
    major) MAJOR=$((MAJOR + 1)); MINOR=0; PATCH=0 ;;
    minor) MINOR=$((MINOR + 1)); PATCH=0 ;;
    patch) PATCH=$((PATCH + 1)) ;;
    [0-9]*.[0-9]*.[0-9]*)
        IFS='.' read -r MAJOR MINOR PATCH <<< "$BUMP" ;;
    *)
        die "unknown bump type: $BUMP (use major, minor, patch, or X.Y.Z)" ;;
esac

NEXT="${MAJOR}.${MINOR}.${PATCH}"
echo "Bumping version: $CURRENT -> $NEXT"

# --- Update Cargo.toml workspace version ---
sed -i "s/^version = \"$CURRENT\"/version = \"$NEXT\"/" "$CARGO_TOML"
echo "  Updated $CARGO_TOML"

# --- Update CHANGELOG.md ---
DATE=$(date -u +%Y-%m-%d)

if [ "$CI_MODE" = true ] && [ -n "$PR_TITLE" ]; then
    # In CI: add PR as a changelog entry under the new version heading.
    # The [Unreleased] section keeps any manually-written entries and gets
    # promoted to the new version heading.
    PR_REF=""
    if [ -n "$PR_NUMBER" ]; then
        PR_REF=" (#$PR_NUMBER)"
    fi

    # Insert the new version heading after [Unreleased], and append the PR entry
    sed -i "s/^## \[Unreleased\]/## [Unreleased]\n\n## [$NEXT] - $DATE\n\n- ${PR_TITLE}${PR_REF}/" "$CHANGELOG"
else
    # Local: just promote the [Unreleased] section to the new version heading
    sed -i "s/^## \[Unreleased\]/## [Unreleased]\n\n## [$NEXT] - $DATE/" "$CHANGELOG"
fi
echo "  Updated $CHANGELOG"

# --- Regenerate Cargo.lock (local only — CI may not have all native deps) ---
if [ "$CI_MODE" = false ]; then
    (cd "$REPO_ROOT" && cargo check --quiet 2>/dev/null) || true
    echo "  Cargo.lock updated"
fi

# --- Commit and tag ---
cd "$REPO_ROOT"
git add Cargo.toml Cargo.lock CHANGELOG.md
git commit -m "release: v$NEXT"
git tag -a "v$NEXT" -m "v$NEXT"

echo ""
echo "Done! Tagged v$NEXT"

if [ "$CI_MODE" = false ]; then
    echo ""
    echo "Next steps:"
    echo "  git push origin main --tags"
fi
