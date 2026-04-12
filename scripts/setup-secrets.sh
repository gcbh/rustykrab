#!/usr/bin/env bash
# -----------------------------------------------------------------------
# setup-secrets.sh — Store required RustyKrab secrets in the OS credential
# store so the application can find them at startup.
#
# This script uses `rustykrab-cli keychain set` under the hood, which
# persists each value in both the OS credential store (macOS Keychain /
# Linux Secret Service) and the encrypted local SQLite store.
#
# Usage:
#   ./scripts/setup-secrets.sh                  # interactive prompts
#   ./scripts/setup-secrets.sh --env            # read from env vars
#
# The canonical credential names come from the central registry in
# crates/rustykrab-store/src/registry.rs. If you add secrets there,
# add the corresponding prompt here.
# -----------------------------------------------------------------------

set -euo pipefail

# Detect the CLI binary.
CLI="${RUSTYKRAB_CLI:-rustykrab-cli}"
if ! command -v "$CLI" &>/dev/null; then
    # Try the workspace debug build.
    WORKSPACE_BIN="$(dirname "$0")/../target/debug/rustykrab-cli"
    if [[ -x "$WORKSPACE_BIN" ]]; then
        CLI="$WORKSPACE_BIN"
    else
        echo "ERROR: rustykrab-cli not found."
        echo "  Build it first:  cargo build -p rustykrab-cli"
        echo "  Or set RUSTYKRAB_CLI=/path/to/rustykrab-cli"
        exit 1
    fi
fi

echo "RustyKrab Secret Setup"
echo "======================"
echo
echo "This will store your credentials in the OS credential store"
echo "(macOS Keychain / Linux Secret Service) and the encrypted local store."
echo

# -----------------------------------------------------------------------
# Registry of secrets to prompt for.
#
# Format: keychain_account | env_var | description | required (yes/no)
#
# Keep this in sync with REGISTRY in crates/rustykrab-store/src/registry.rs.
# -----------------------------------------------------------------------
SECRETS=(
    "notion-api-token|NOTION_API_TOKEN|Notion integration API token (ntn_...)|yes"
    "obsidian-api-key|OBSIDIAN_API_KEY|Obsidian Local REST API key|yes"
    "anthropic-api-key|ANTHROPIC_API_KEY|Anthropic Claude API key|no"
    "auth-token|RUSTYKRAB_AUTH_TOKEN|Gateway bearer auth token (auto-generated if empty)|no"
)

ENV_MODE=false
if [[ "${1:-}" == "--env" ]]; then
    ENV_MODE=true
    echo "Reading values from environment variables."
    echo
fi

stored=0
skipped=0

for entry in "${SECRETS[@]}"; do
    IFS='|' read -r account env_var description required <<< "$entry"

    value=""

    if $ENV_MODE; then
        value="${!env_var:-}"
    else
        req_label=""
        if [[ "$required" == "yes" ]]; then
            req_label=" [REQUIRED]"
        fi

        # Show current status.
        if "$CLI" keychain status 2>/dev/null | grep -q "$account.*present"; then
            echo "  $description: already set (skip with Enter)"
        fi

        read -rsp "$description${req_label}: " value
        echo
    fi

    # Skip empty values.
    if [[ -z "$value" ]]; then
        if [[ "$required" == "yes" ]] && ! $ENV_MODE; then
            # Check if it's already stored — allow skipping.
            if "$CLI" keychain status 2>/dev/null | grep -q "$account.*present"; then
                ((skipped++))
                continue
            fi
            echo "  WARNING: $description is required but was left empty."
        fi
        ((skipped++))
        continue
    fi

    "$CLI" keychain set "$account" "$value"
    ((stored++))
done

echo
echo "Done. $stored credential(s) stored, $skipped skipped."
echo
echo "Verify with:  $CLI keychain status"
