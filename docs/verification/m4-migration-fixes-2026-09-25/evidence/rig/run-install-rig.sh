#!/usr/bin/env bash
# PR #659 rig: run a real scripts/install.sh into a throwaway HOME.
#   run-install-rig.sh <install.sh under test> <signed RustyKrab.app> <out-dir> <label> <yes|no>
# launchctl is replaced by a PATH shim that only records its arguments, so
# nothing is loaded into launchd. Everything else is real: codesign --verify,
# ditto, PlistBuddy, plutil, chmod. Env values are dummies, never real secrets.
set -uo pipefail
SCRIPT=$1 APP=$2 OUT=$3 LABEL=$4 WITH=$5
H="$OUT/$LABEL"; rm -rf "$H"; mkdir -p "$H/shim"
cat > "$H/shim/launchctl" <<'SH'
#!/bin/sh
echo "launchctl $*" >> "$LAUNCHCTL_LOG"
SH
chmod +x "$H/shim/launchctl"
extra=()
[ "$WITH" = yes ] && extra=(OLLAMA_TIMEOUT_SECS=901 RUSTYKRAB_MAX_CONTEXT_TOKENS=40001 RUSTYKRAB_OUTCOME_CAPTURE=1)
env -i PATH="$H/shim:/usr/bin:/bin:/usr/sbin" HOME="$H/home" LAUNCHCTL_LOG="$H/launchctl.log" \
  RUSTYKRAB_PROVIDER=ollama OLLAMA_MODEL=verify-model RUSTYKRAB_NUM_CTX=65536 ${extra[@]+"${extra[@]}"} \
  bash "$SCRIPT" "$APP" > "$H/install.out" 2>&1
echo "== $LABEL (install.sh blob $(git hash-object "$SCRIPT"), keys exported: $WITH) exit=$?"
python3 - "$H/home/Library/LaunchAgents/com.gcbh.rustykrab.plist" <<'PY'
import plistlib, sys, os
p = sys.argv[1]; e = plistlib.load(open(p, "rb"))["EnvironmentVariables"]
print("  plist env keys:", " ".join(sorted(e)))
for k in ("OLLAMA_TIMEOUT_SECS", "RUSTYKRAB_MAX_CONTEXT_TOKENS", "RUSTYKRAB_OUTCOME_CAPTURE"):
    print(f"  {k} = {e.get(k, '<absent>')}")
print("  plist mode", oct(os.stat(p).st_mode & 0o777))
PY
sed "s#$H#<rig>#g; s/^/  /" "$H/launchctl.log"
