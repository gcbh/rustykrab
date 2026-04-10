#!/usr/bin/env bash
#
# deploy-fastembed.sh — Idempotent deployment of the fastembed embedding model.
#
# Downloads the Nomic-embed-text-v1.5 ONNX model (~275 MB) into the
# application's model cache directory so the first startup doesn't block
# on a download.  Safe to run multiple times (skips if model already cached).
#
# Can be run remotely via: ssh host 'bash -s' < scripts/deploy-fastembed.sh
#
# Environment variables (all optional):
#   RUSTYKRAB_DATA_DIR  — override data directory (default: ~/.local/share/rustykrab)
#   FASTEMBED_CACHE_DIR — override model cache directory (default: $DATA_DIR/models)
#   FASTEMBED_MODEL     — HuggingFace model repo (default: nomic-ai/nomic-embed-text-v1.5)
#
set -euo pipefail

# ── Configuration ────────────────────────────────────────────────

DATA_DIR="${RUSTYKRAB_DATA_DIR:-${XDG_DATA_HOME:-$HOME/.local/share}/rustykrab}"
CACHE_DIR="${FASTEMBED_CACHE_DIR:-$DATA_DIR/models}"
MODEL_REPO="${FASTEMBED_MODEL:-nomic-ai/nomic-embed-text-v1.5}"

# fastembed stores models in a directory derived from the repo name.
# The convention is: <cache_dir>/fast-embed-models/<repo>
MODEL_DIR="$CACHE_DIR/fast-embed-models/$MODEL_REPO"

# Files that constitute a complete model download.
REQUIRED_FILES=(
    "model.onnx"
    "tokenizer.json"
)

# ── Helpers ──────────────────────────────────────────────────────

log()  { printf '[deploy-fastembed] %s\n' "$*"; }
die()  { log "ERROR: $*" >&2; exit 1; }

check_tool() {
    command -v "$1" >/dev/null 2>&1 || die "'$1' is required but not found in PATH"
}

# ── Preflight ────────────────────────────────────────────────────

check_tool curl
check_tool mkdir

log "data dir:  $DATA_DIR"
log "cache dir: $CACHE_DIR"
log "model:     $MODEL_REPO"

mkdir -p "$CACHE_DIR"
mkdir -p "$DATA_DIR"

# ── Idempotency check ───────────────────────────────────────────

model_complete() {
    for f in "${REQUIRED_FILES[@]}"; do
        if [ ! -f "$MODEL_DIR/$f" ]; then
            return 1
        fi
    done
    return 0
}

if model_complete; then
    log "model already cached at $MODEL_DIR — nothing to do"
    exit 0
fi

# ── Download model files from HuggingFace ────────────────────────

HF_BASE="https://huggingface.co/$MODEL_REPO/resolve/main"
mkdir -p "$MODEL_DIR"

download_file() {
    local filename="$1"
    local url="$HF_BASE/$filename"
    local dest="$MODEL_DIR/$filename"

    if [ -f "$dest" ]; then
        log "  $filename — already present, skipping"
        return 0
    fi

    log "  $filename — downloading..."
    local tmp="$dest.tmp.$$"
    if curl -fSL --retry 3 --retry-delay 2 -o "$tmp" "$url"; then
        mv "$tmp" "$dest"
        log "  $filename — done"
    else
        rm -f "$tmp"
        die "failed to download $url"
    fi
}

log "downloading model files..."

# Core ONNX model
download_file "model.onnx"

# Tokenizer
download_file "tokenizer.json"

# Config files (fastembed may look for these)
download_file "config.json" 2>/dev/null || true
download_file "special_tokens_map.json" 2>/dev/null || true
download_file "tokenizer_config.json" 2>/dev/null || true

# ── Verify ───────────────────────────────────────────────────────

if model_complete; then
    log "model deployed successfully"
    log "model path: $MODEL_DIR"
    # Print model size for verification
    du -sh "$MODEL_DIR" 2>/dev/null || true
else
    die "model deployment incomplete — required files missing"
fi

# ── Ensure data directory structure ──────────────────────────────

# Create directories the application expects on startup.
mkdir -p "$DATA_DIR/logs"
mkdir -p "$DATA_DIR/skills"
mkdir -p "$DATA_DIR/db"

log "data directory structure verified"
log "deployment complete"
