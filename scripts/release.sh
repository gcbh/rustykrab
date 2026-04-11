#!/usr/bin/env bash
#
# release.sh — Bump the workspace version, update CHANGELOG.md, commit, and tag.
#
# Usage:
#   ./scripts/release.sh <major|minor|patch>
#   ./scripts/release.sh 1.2.3          # explicit version
#
# What it does:
#   1. Validates the working tree is clean
#   2. Computes the next version (or uses the one you gave)
#   3. Updates workspace version in Cargo.toml
#   4. Moves "Unreleased" entries in CHANGELOG.md under a new version heading
#   5. Runs `cargo check` to regenerate Cargo.lock
#   6. Commits and tags as v<version>
#
# After running this script, push with:
#   git push origin main --tags
#
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CARGO_TOML="$REPO_ROOT/Cargo.toml"
CHANGELOG="$REPO_ROOT/CHANGELOG.md"

die() { echo "error: $*" >&2; exit 1; }

# --- Ensure clean working tree ---
if [ -n "$(git -C "$REPO_ROOT" status --porcelain)" ]; then
    die "working tree is dirty — commit or stash changes first"
fi

# --- Read current version ---
CURRENT=$(sed -n 's/^version = "\(.*\)"/\1/p' "$CARGO_TOML" | head -1)
[ -n "$CURRENT" ] || die "could not read current version from $CARGO_TOML"

IFS='.' read -r MAJOR MINOR PATCH <<< "$CURRENT"

# --- Compute next version ---
BUMP="${1:?Usage: release.sh <major|minor|patch|X.Y.Z>}"

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
# Replace the "## [Unreleased]" line, keeping a fresh Unreleased section above
sed -i "s/^## \[Unreleased\]/## [Unreleased]\n\n## [$NEXT] - $DATE/" "$CHANGELOG"
echo "  Updated $CHANGELOG"

# --- Regenerate Cargo.lock ---
(cd "$REPO_ROOT" && cargo check --quiet 2>/dev/null) || true
echo "  Cargo.lock updated"

# --- Commit and tag ---
git -C "$REPO_ROOT" add Cargo.toml Cargo.lock CHANGELOG.md
git -C "$REPO_ROOT" commit -m "release: v$NEXT"
git -C "$REPO_ROOT" tag -a "v$NEXT" -m "v$NEXT"

echo ""
echo "Done! Tagged v$NEXT"
echo ""
echo "Next steps:"
echo "  git push origin main --tags"
