#!/usr/bin/env bash
#
# Codesign rustykrab-cli with keychain entitlements.
#
# Uses the Developer ID certificate if available, otherwise falls
# back to ad-hoc signing. The Data Protection Keychain requires a
# real signing identity with the keychain-access-groups entitlement.
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

# Extract team ID from the signing identity (e.g. "Developer ID Application: Name (ABCDEF1234)")
# and resolve the TEAMID placeholder in entitlements.plist so the Data Protection Keychain works.
TEAM_ID=""
if [ "$IDENTITY" != "-" ]; then
    TEAM_ID=$(echo "$IDENTITY" | sed -n 's/.*(\([A-Z0-9]*\))$/\1/p')
fi

if [ -n "$TEAM_ID" ]; then
    RESOLVED_ENTITLEMENTS=$(mktemp "${TMPDIR:-/tmp}/rustykrab-entitlements.XXXXXX.plist")
    sed "s/TEAMID/${TEAM_ID}/g" "$ENTITLEMENTS" > "$RESOLVED_ENTITLEMENTS"
    trap 'rm -f "$RESOLVED_ENTITLEMENTS"' EXIT
    echo "Signing: $BINARY"
    echo "Identity: $IDENTITY"
    echo "Team ID: $TEAM_ID"
    echo "Entitlements: $ENTITLEMENTS (resolved → $RESOLVED_ENTITLEMENTS)"
else
    RESOLVED_ENTITLEMENTS="$ENTITLEMENTS"
    echo "Signing: $BINARY"
    echo "Identity: $IDENTITY"
    echo "Entitlements: $ENTITLEMENTS (no team ID — Data Protection Keychain unavailable)"
fi

codesign \
    --sign "$IDENTITY" \
    --entitlements "$RESOLVED_ENTITLEMENTS" \
    --force \
    "$BINARY"

echo "Done. Verifying..."
codesign --display --entitlements - "$BINARY" 2>&1 | head -20
