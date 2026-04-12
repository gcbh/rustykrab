#!/usr/bin/env bash
#
# Codesign rustykrab-cli for macOS.
#
# Uses the Developer ID certificate if available, otherwise falls
# back to ad-hoc signing. For Developer ID, enables hardened runtime
# and sets the team identifier (required for notarization).
#
# Usage:
#   ./scripts/codesign.sh                  # sign debug build
#   ./scripts/codesign.sh --release        # sign release build
#   ./scripts/codesign.sh path/to/binary   # sign a specific binary
#
# Override the signing identity via CODESIGN_IDENTITY env var:
#   CODESIGN_IDENTITY="Developer ID Application: ..." ./scripts/codesign.sh --release

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
ENTITLEMENTS="$PROJECT_ROOT/entitlements.plist"

if [ ! -f "$ENTITLEMENTS" ]; then
    echo "error: entitlements.plist not found at $ENTITLEMENTS" >&2
    exit 1
fi

# Determine which binary to sign.
if [ $# -eq 0 ]; then
    BINARY="$PROJECT_ROOT/target/debug/rustykrab-cli"
elif [ "$1" = "--release" ]; then
    BINARY="$PROJECT_ROOT/target/release/rustykrab-cli"
else
    BINARY="$1"
fi

if [ ! -f "$BINARY" ]; then
    echo "error: binary not found at $BINARY" >&2
    echo "hint: run 'cargo build' first" >&2
    exit 1
fi

# Find signing identity: env var > auto-detect Developer ID > ad-hoc fallback.
if [ -n "${CODESIGN_IDENTITY:-}" ]; then
    IDENTITY="$CODESIGN_IDENTITY"
else
    IDENTITY=$(security find-identity -v -p codesigning 2>/dev/null \
        | grep "Developer ID Application" \
        | head -1 \
        | sed 's/.*"\(.*\)".*/\1/' || true)
    if [ -z "$IDENTITY" ]; then
        echo "warning: no Developer ID found, using ad-hoc signing (keychain entitlements may not work)" >&2
        IDENTITY="-"
    fi
fi

echo "Signing: $BINARY"
echo "Identity: $IDENTITY"

# Build codesign flags. For Developer ID, enable hardened runtime and set
# the team ID with an explicit designated requirement (codesign generates a
# broken one for bare Mach-O files).
EXTRA_FLAGS=()
if [ "$IDENTITY" != "-" ]; then
    EXTRA_FLAGS+=(--options runtime)
    TEAM_ID=$(echo "$IDENTITY" | sed -n 's/.*(\([A-Z0-9]*\)).*/\1/p')
    if [ -n "$TEAM_ID" ]; then
        EXTRA_FLAGS+=(--team-id "$TEAM_ID")
        EXTRA_FLAGS+=(-r="designated => anchor apple generic and certificate leaf[subject.OU] = \"${TEAM_ID}\"")
        echo "Team ID: $TEAM_ID"
    fi
fi

echo "Entitlements: $ENTITLEMENTS"

codesign \
    --sign "$IDENTITY" \
    --entitlements "$ENTITLEMENTS" \
    --force \
    "${EXTRA_FLAGS[@]}" \
    "$BINARY"

echo "Done. Verifying..."
codesign -vvv "$BINARY" 2>&1
codesign --display --entitlements - "$BINARY" 2>&1 | head -20
