#!/usr/bin/env bash
# Install a signed RustyKrab.app and register it as a per-user LaunchAgent.
#
# Usage:
#   scripts/install.sh [path/to/RustyKrab.app] [--allow-adhoc]
#
# Runtime credentials should be placed in the macOS Keychain before install.
# Values supplied through the environment are written to the user-only plist
# for compatibility with existing deployments; prefer the Keychain for new
# installs.

set -euo pipefail

LABEL="com.gcbh.rustykrab"
PLIST="$HOME/Library/LaunchAgents/$LABEL.plist"
APP_DEST="$HOME/Applications/RustyKrab.app"
LOG_DIR="$HOME/.local/share/rustykrab/logs"
ALLOW_ADHOC=false
APP_SRC=""

die() { echo "error: $*" >&2; exit 1; }

while (($#)); do
    case "$1" in
        --allow-adhoc) ALLOW_ADHOC=true ;;
        -*) die "unknown option: $1" ;;
        *) [[ -z "$APP_SRC" ]] || die "only one app path may be supplied"; APP_SRC="$1" ;;
    esac
    shift
done

if [[ -z "$APP_SRC" ]]; then
    SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
    APP_SRC="$SCRIPT_DIR/../RustyKrab.app"
fi
APP_SRC="$(cd "$APP_SRC" 2>/dev/null && pwd)" || die "app bundle not found: $APP_SRC"
BINARY="$APP_SRC/Contents/MacOS/rustykrab-cli"
[[ -x "$BINARY" ]] || die "app executable not found: $BINARY"

if ! codesign --verify --deep --strict "$APP_SRC" 2>/dev/null; then
    if ! $ALLOW_ADHOC; then
        die "app signature is invalid; pass --allow-adhoc only for local development"
    fi
    echo "warning: installing an unverified/ad-hoc app for local development" >&2
fi

VERSION="$($BINARY --version 2>/dev/null | head -1 || true)"
echo "Installing ${VERSION:-RustyKrab}"

mkdir -p "$HOME/Applications" "$HOME/Library/LaunchAgents" "$LOG_DIR"
launchctl bootout "gui/$(id -u)/$LABEL" 2>/dev/null || true

# Keep the previous bundle available for rollback and install atomically.
if [[ -d "$APP_DEST" ]]; then
    BACKUP="$APP_DEST.previous-$(date -u +%Y%m%dT%H%M%SZ)"
    mv "$APP_DEST" "$BACKUP"
    echo "Previous app preserved at $BACKUP"
fi
ditto "$APP_SRC" "$APP_DEST"

# PlistBuddy avoids interpolation errors from XML-sensitive configuration.
rm -f "$PLIST"
plutil -create xml1 "$PLIST"
PB=(/usr/libexec/PlistBuddy -c)
"${PB[@]}" 'Add :Label string com.gcbh.rustykrab' "$PLIST"
"${PB[@]}" 'Add :ProgramArguments array' "$PLIST"
"${PB[@]}" "Add :ProgramArguments:0 string $APP_DEST/Contents/MacOS/rustykrab-cli" "$PLIST"
"${PB[@]}" 'Add :EnvironmentVariables dict' "$PLIST"
"${PB[@]}" "Add :EnvironmentVariables:HOME string $HOME" "$PLIST"
"${PB[@]}" 'Add :EnvironmentVariables:PATH string /opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin' "$PLIST"

# Anything not listed here never reaches the installed daemon, however it was
# set at install time. The credential-page settings are on the list because
# without RUSTYKRAB_PUBLIC_URL the agent cannot mint a link at all, and the
# failure is silent — it just falls back to telling the user to open the app.
#
# RUSTYKRAB_NODES carries a peer's bearer token, so it lands in a plist that is
# chmod 600 below — the same posture as the systemd EnvironmentFile documented
# in the README. Without forwarding it, a daemon installed as a LaunchAgent can
# never be configured to delegate, whatever the installing shell had exported.
for key in RUSTYKRAB_PROVIDER RUSTYKRAB_PORT RUSTYKRAB_WEB_UI RUST_LOG \
    OLLAMA_BASE_URL OLLAMA_MODEL OLLAMA_NUM_CTX RUSTYKRAB_NUM_CTX \
    RUSTYKRAB_NODES RUSTYKRAB_NODE_TIMEOUT_SECS \
    TELEGRAM_ALLOWED_CHATS TELEGRAM_BOT_TOKEN \
    RUSTYKRAB_PUBLIC_URL RUSTYKRAB_TAILNET_USERS RUSTYKRAB_ALLOWED_ORIGINS; do
    if [[ -n "${!key:-}" ]]; then
        "${PB[@]}" "Add :EnvironmentVariables:$key string ${!key}" "$PLIST"
    fi
done

"${PB[@]}" 'Add :RunAtLoad bool true' "$PLIST"
"${PB[@]}" 'Add :KeepAlive bool true' "$PLIST"
"${PB[@]}" 'Add :ThrottleInterval integer 10' "$PLIST"
"${PB[@]}" 'Add :ProcessType string Background' "$PLIST"
"${PB[@]}" "Add :StandardOutPath string $LOG_DIR/launchagent.log" "$PLIST"
"${PB[@]}" "Add :StandardErrorPath string $LOG_DIR/launchagent.log" "$PLIST"
chmod 600 "$PLIST"
plutil -lint "$PLIST"

launchctl bootstrap "gui/$(id -u)" "$PLIST"
echo "RustyKrab is installed and started."
echo "  Status: launchctl print gui/$(id -u)/$LABEL"
echo "  Logs:   tail -f $LOG_DIR/launchagent.log"
