#!/usr/bin/env bash
#
# Ad-hoc codesign rustykrab-cli with keychain entitlements.
#
# The Data Protection Keychain requires the keychain-access-groups
# entitlement. This script signs the cargo-built binary so it can
# access the keychain without an Apple Developer certificate.
#
# Usage:
#   ./scripts/codesign.sh                  # sign debug build
#   ./scripts/codesign.sh --release        # sign release build
#   ./scripts/codesign.sh path/to/binary   # sign a specific binary

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

echo "Signing: $BINARY"
echo "Entitlements: $ENTITLEMENTS"

codesign \
    --sign - \
    --entitlements "$ENTITLEMENTS" \
    --force \
    "$BINARY"

echo "Done. Verifying..."
codesign --display --entitlements - "$BINARY" 2>&1 | head -20
