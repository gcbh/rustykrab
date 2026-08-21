#!/usr/bin/env bash
# Remove RustyKrab's per-user LaunchAgent. Keep the app and data by default.

set -euo pipefail

LABEL="com.gcbh.rustykrab"
PLIST="$HOME/Library/LaunchAgents/$LABEL.plist"
APP_DEST="$HOME/Applications/RustyKrab.app"

if launchctl print "gui/$(id -u)/$LABEL" >/dev/null 2>&1; then
    launchctl bootout "gui/$(id -u)/$LABEL" || true
fi
rm -f "$PLIST"

if [[ "${1:-}" == "--purge" ]]; then
    rm -rf "$APP_DEST"
    echo "Removed app bundle: $APP_DEST"
else
    echo "LaunchAgent removed; app retained at $APP_DEST"
    echo "Use --purge to remove the app bundle too."
fi
