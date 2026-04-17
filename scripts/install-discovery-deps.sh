#!/usr/bin/env bash
# -----------------------------------------------------------------------
# install-discovery-deps.sh — Install system dependencies used by the
# `network-recon` skill (driven through the `exec` tool).
#
# The skill shells out to standard network CLIs for discovery, admin,
# and audit tasks. This script detects the package manager and installs
# the expected binaries.
#
# Usage:
#   sudo ./scripts/install-discovery-deps.sh          # install all
#   sudo ./scripts/install-discovery-deps.sh --check   # dry-run check only
#
# Dependencies installed:
#   Required:
#     nmap           — live-host and port scanning
#     avahi-utils    — mDNS/DNS-SD service discovery (avahi-browse)
#     openssh-client — SSH for remote admin and DHCP lease queries
#     dnsutils       — DNS lookups (dig)
#     traceroute     — network path tracing
#     iproute2       — interface listing, ARP cache (ip command)
#
#   Recommended:
#     arp-scan       — fast, reliable ARP scanning with OUI vendor data
#     ieee-data      — IEEE OUI database for MAC vendor lookups
#     openssl        — TLS/SSL certificate checking
# -----------------------------------------------------------------------

set -euo pipefail

CHECK_ONLY=false
if [[ "${1:-}" == "--check" ]]; then
    CHECK_ONLY=true
fi

# ANSI colors (disabled if not a terminal).
if [[ -t 1 ]]; then
    GREEN='\033[0;32m'
    YELLOW='\033[1;33m'
    RED='\033[0;31m'
    NC='\033[0m'
else
    GREEN='' YELLOW='' RED='' NC=''
fi

ok()   { echo -e "  ${GREEN}[ok]${NC}   $1"; }
miss() { echo -e "  ${RED}[miss]${NC} $1"; }
skip() { echo -e "  ${YELLOW}[skip]${NC} $1"; }

# -----------------------------------------------------------------------
# Detect package manager
# -----------------------------------------------------------------------
detect_pkg_manager() {
    if command -v apt-get &>/dev/null; then
        echo "apt"
    elif command -v dnf &>/dev/null; then
        echo "dnf"
    elif command -v yum &>/dev/null; then
        echo "yum"
    elif command -v pacman &>/dev/null; then
        echo "pacman"
    elif command -v apk &>/dev/null; then
        echo "apk"
    elif command -v brew &>/dev/null; then
        echo "brew"
    else
        echo "unknown"
    fi
}

PKG_MGR=$(detect_pkg_manager)

# -----------------------------------------------------------------------
# Package name mapping per distro
# -----------------------------------------------------------------------
# Each entry: binary_name -> package_name for the detected package manager.

declare -A APT_PKGS=(
    [nmap]="nmap"
    [avahi-browse]="avahi-utils"
    [ssh]="openssh-client"
    [dig]="dnsutils"
    [traceroute]="traceroute"
    [ip]="iproute2"
    [arp-scan]="arp-scan"
    [openssl]="openssl"
)

declare -A DNF_PKGS=(
    [nmap]="nmap"
    [avahi-browse]="avahi-tools"
    [ssh]="openssh-clients"
    [dig]="bind-utils"
    [traceroute]="traceroute"
    [ip]="iproute"
    [arp-scan]="arp-scan"
    [openssl]="openssl"
)

declare -A PACMAN_PKGS=(
    [nmap]="nmap"
    [avahi-browse]="avahi"
    [ssh]="openssh"
    [dig]="bind"
    [traceroute]="traceroute"
    [ip]="iproute2"
    [arp-scan]="arp-scan"
    [openssl]="openssl"
)

declare -A APK_PKGS=(
    [nmap]="nmap"
    [avahi-browse]="avahi-tools"
    [ssh]="openssh-client"
    [dig]="bind-tools"
    [traceroute]="traceroute"
    [ip]="iproute2"
    [arp-scan]="arp-scan"
    [openssl]="openssl"
)

declare -A BREW_PKGS=(
    [nmap]="nmap"
    [avahi-browse]=""
    [ssh]=""
    [dig]=""
    [traceroute]=""
    [ip]="iproute2mac"
    [arp-scan]="arp-scan"
    [openssl]="openssl"
)

# IEEE data package (for OUI lookups) — only available on some distros.
IEEE_DATA_PKG=""
case "$PKG_MGR" in
    apt)  IEEE_DATA_PKG="ieee-data" ;;
    dnf|yum) IEEE_DATA_PKG="hwdata" ;;
    pacman) IEEE_DATA_PKG="" ;;
    apk) IEEE_DATA_PKG="" ;;
    brew) IEEE_DATA_PKG="" ;;
esac

get_pkg_name() {
    local binary="$1"
    case "$PKG_MGR" in
        apt)    echo "${APT_PKGS[$binary]:-}" ;;
        dnf|yum) echo "${DNF_PKGS[$binary]:-}" ;;
        pacman) echo "${PACMAN_PKGS[$binary]:-}" ;;
        apk)    echo "${APK_PKGS[$binary]:-}" ;;
        brew)   echo "${BREW_PKGS[$binary]:-}" ;;
        *)      echo "" ;;
    esac
}

# -----------------------------------------------------------------------
# Check which binaries are present
# -----------------------------------------------------------------------

REQUIRED_BINS=(nmap avahi-browse ssh dig traceroute ip)
RECOMMENDED_BINS=(arp-scan openssl)

MISSING_REQUIRED=()
MISSING_RECOMMENDED=()

echo ""
echo "Checking network discovery dependencies..."
echo ""
echo "Required:"
for bin in "${REQUIRED_BINS[@]}"; do
    if command -v "$bin" &>/dev/null; then
        ok "$bin ($(command -v "$bin"))"
    else
        miss "$bin"
        MISSING_REQUIRED+=("$bin")
    fi
done

echo ""
echo "Recommended:"
for bin in "${RECOMMENDED_BINS[@]}"; do
    if command -v "$bin" &>/dev/null; then
        ok "$bin ($(command -v "$bin"))"
    else
        miss "$bin"
        MISSING_RECOMMENDED+=("$bin")
    fi
done

# Check IEEE data.
echo ""
echo "Data files:"
OUI_FOUND=false
for path in /usr/share/ieee-data/oui.csv /usr/share/misc/oui.txt /usr/share/nmap/nmap-mac-prefixes /var/lib/ieee-data/oui.csv; do
    if [[ -f "$path" ]]; then
        ok "OUI database ($path)"
        OUI_FOUND=true
        break
    fi
done
if ! $OUI_FOUND; then
    miss "OUI database (IEEE MAC vendor data) — oui_lookup will use built-in table only"
fi

# -----------------------------------------------------------------------
# Summary
# -----------------------------------------------------------------------
echo ""

if [[ ${#MISSING_REQUIRED[@]} -eq 0 && ${#MISSING_RECOMMENDED[@]} -eq 0 ]]; then
    echo -e "${GREEN}All dependencies are installed.${NC}"
    exit 0
fi

if $CHECK_ONLY; then
    echo "Run without --check to install missing packages."
    exit 1
fi

if [[ "$PKG_MGR" == "unknown" ]]; then
    echo -e "${RED}Could not detect package manager.${NC}"
    echo "Please install manually: ${MISSING_REQUIRED[*]} ${MISSING_RECOMMENDED[*]}"
    exit 1
fi

# -----------------------------------------------------------------------
# Install missing packages
# -----------------------------------------------------------------------

PKGS_TO_INSTALL=()
for bin in "${MISSING_REQUIRED[@]}" "${MISSING_RECOMMENDED[@]}"; do
    pkg=$(get_pkg_name "$bin")
    if [[ -n "$pkg" ]]; then
        PKGS_TO_INSTALL+=("$pkg")
    else
        skip "$bin — no package available for $PKG_MGR"
    fi
done

# Add IEEE data if not present.
if ! $OUI_FOUND && [[ -n "$IEEE_DATA_PKG" ]]; then
    PKGS_TO_INSTALL+=("$IEEE_DATA_PKG")
fi

# De-duplicate.
PKGS_TO_INSTALL=($(printf '%s\n' "${PKGS_TO_INSTALL[@]}" | sort -u))

if [[ ${#PKGS_TO_INSTALL[@]} -eq 0 ]]; then
    echo "No packages to install."
    exit 0
fi

echo "Installing: ${PKGS_TO_INSTALL[*]}"
echo ""

case "$PKG_MGR" in
    apt)
        apt-get update -qq
        apt-get install -y --no-install-recommends "${PKGS_TO_INSTALL[@]}"
        ;;
    dnf)
        dnf install -y "${PKGS_TO_INSTALL[@]}"
        ;;
    yum)
        yum install -y "${PKGS_TO_INSTALL[@]}"
        ;;
    pacman)
        pacman -Sy --noconfirm "${PKGS_TO_INSTALL[@]}"
        ;;
    apk)
        apk add --no-cache "${PKGS_TO_INSTALL[@]}"
        ;;
    brew)
        brew install "${PKGS_TO_INSTALL[@]}"
        ;;
esac

echo ""
echo -e "${GREEN}Done.${NC} Re-run with --check to verify."
