#!/usr/bin/env bash
#
# Assemble and codesign a macOS .app bundle for rustykrab-cli.
#
# The Data Protection Keychain requires the `keychain-access-groups`
# entitlement, which is restricted: macOS only honours it when the binary
# ships inside an .app bundle with an embedded provisioning profile. A bare
# `cargo build` binary therefore falls back to the legacy keychain, which
# prompts for per-app ACL approval on every credential access.
#
# Use this instead of scripts/codesign.sh when the build needs keychain
# access — i.e. any build you intend to actually run.
#
# Usage:
#   ./scripts/bundle.sh                    # debug build  -> target/debug/RustyKrab.app
#   ./scripts/bundle.sh --release          # release build -> target/release/RustyKrab.app
#   ./scripts/bundle.sh --release -o /path/to/Out.app
#
# Environment:
#   CODESIGN_IDENTITY    signing identity (default: auto-detect Developer ID)
#   PROVISION_PROFILE    path to .provisionprofile (default: auto-detect)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
ENTITLEMENTS="$PROJECT_ROOT/entitlements.plist"

PROFILE_MODE="debug"
OUT_APP=""
while [ $# -gt 0 ]; do
    case "$1" in
        --release) PROFILE_MODE="release"; shift ;;
        --debug)   PROFILE_MODE="debug";   shift ;;
        -o|--output) OUT_APP="$2"; shift 2 ;;
        *) echo "error: unknown argument: $1" >&2; exit 1 ;;
    esac
done

BINARY="$PROJECT_ROOT/target/$PROFILE_MODE/rustykrab-cli"
[ -n "$OUT_APP" ] || OUT_APP="$PROJECT_ROOT/target/$PROFILE_MODE/RustyKrab.app"

if [ ! -f "$BINARY" ]; then
    echo "error: binary not found at $BINARY" >&2
    echo "hint: cargo build${PROFILE_MODE:+ --$PROFILE_MODE} -p rustykrab-cli" >&2
    exit 1
fi
if [ ! -f "$ENTITLEMENTS" ]; then
    echo "error: entitlements.plist not found at $ENTITLEMENTS" >&2
    exit 1
fi

# --- Signing identity -------------------------------------------------------
if [ -n "${CODESIGN_IDENTITY:-}" ]; then
    IDENTITY="$CODESIGN_IDENTITY"
else
    IDENTITY=$(security find-identity -v -p codesigning 2>/dev/null \
        | grep "Developer ID Application" | head -1 | sed 's/.*"\(.*\)".*/\1/' || true)
fi
if [ -z "$IDENTITY" ]; then
    echo "error: no Developer ID Application identity found." >&2
    echo "       Ad-hoc signing cannot carry keychain-access-groups, so the" >&2
    echo "       bundle would still fall back to the legacy keychain." >&2
    exit 1
fi

TEAM_ID=$(echo "$IDENTITY" | sed -n 's/.*(\([A-Z0-9]*\)).*/\1/p')
if [ -z "$TEAM_ID" ]; then
    echo "error: could not extract team ID from identity: $IDENTITY" >&2
    exit 1
fi

# --- Provisioning profile ---------------------------------------------------
# Required: without it macOS rejects (and may SIGKILL) a binary claiming the
# restricted keychain-access-groups entitlement.
if [ -z "${PROVISION_PROFILE:-}" ]; then
    for candidate in \
        "$PROJECT_ROOT/RustyKrab.app/Contents/embedded.provisionprofile" \
        "$HOME/Library/MobileDevice/Provisioning Profiles"/*.provisionprofile
    do
        if [ -f "$candidate" ]; then PROVISION_PROFILE="$candidate"; break; fi
    done
fi
if [ -z "${PROVISION_PROFILE:-}" ] || [ ! -f "$PROVISION_PROFILE" ]; then
    echo "error: no provisioning profile found; set PROVISION_PROFILE=/path/to.provisionprofile" >&2
    exit 1
fi

VERSION=$(grep -m1 '^version' "$PROJECT_ROOT/Cargo.toml" | sed 's/.*"\(.*\)".*/\1/')

echo "Binary:   $BINARY"
echo "Bundle:   $OUT_APP"
echo "Identity: $IDENTITY"
echo "Team ID:  $TEAM_ID"
echo "Profile:  $PROVISION_PROFILE"

# --- Assemble ---------------------------------------------------------------
rm -rf "$OUT_APP"
mkdir -p "$OUT_APP/Contents/MacOS"
cp "$BINARY" "$OUT_APP/Contents/MacOS/rustykrab-cli"
cp "$PROVISION_PROFILE" "$OUT_APP/Contents/embedded.provisionprofile"

# CFBundleIdentifier must match the application-identifier in entitlements.plist.
cat > "$OUT_APP/Contents/Info.plist" << PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleIdentifier</key>
    <string>com.gcbh.rustykrab</string>
    <key>CFBundleExecutable</key>
    <string>rustykrab-cli</string>
    <key>CFBundleName</key>
    <string>RustyKrab</string>
    <key>CFBundleVersion</key>
    <string>${VERSION}</string>
    <key>LSMinimumSystemVersion</key>
    <string>10.15</string>
</dict>
</plist>
PLIST

# --- Sign -------------------------------------------------------------------
# The explicit designated requirement matters: codesign generates a broken one
# for bare Mach-O files, and a mismatched DR is what makes the legacy keychain
# re-prompt after every rebuild.
codesign \
    --sign "$IDENTITY" \
    --entitlements "$ENTITLEMENTS" \
    --options runtime \
    --team-id "$TEAM_ID" \
    -r="designated => anchor apple generic and certificate leaf[subject.OU] = \"${TEAM_ID}\"" \
    --force \
    "$OUT_APP"

echo "Done. Verifying..."
codesign --verify --strict --verbose=2 "$OUT_APP" 2>&1
codesign -dv --verbose=2 "$OUT_APP" 2>&1 | grep -E "Identifier=|flags=|TeamIdentifier="
echo
echo "Run it with:"
echo "  $OUT_APP/Contents/MacOS/rustykrab-cli"
