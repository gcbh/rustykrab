# Using a Spare Intel Mac as a RustyKrab Helper Node

This runbook explains how to offload parts of a RustyKrab deployment onto a
secondary **pre-M1 (Intel) MacBook Pro with a non-NVIDIA (AMD Radeon) GPU**. It
covers a media/browser worker, a service host, and an optional speech (STT/TTS)
service.

RustyKrab is a single-instance monolith — the gateway, SQLite store, and memory
all live on one machine. You can't cluster the core, but the parts that talk over
the network or shell out to external binaries *can* run on another box. That's
what this guide does.

---

## TL;DR — what to put where

| Workload | Runs on | Why |
|---|---|---|
| Gateway, store, memory, agent loop | **Primary** | Loopback-only gateway; local SQLite. Do not move. |
| Browser automation / screenshots (Chrome) | **Spare Mac** | Chrome's GPU rendering uses the AMD card; offloads CPU. |
| Video rendering (FFmpeg / hyperframes) | **Spare Mac** | FFmpeg uses **VideoToolbox** hardware encode on the AMD GPU. |
| `signal-cli-rest-api`, MCP servers, Telegram/Signal polling | **Spare Mac** | Pure network services; no need to sit next to the agent. |
| Speech-to-text (`whisper.cpp`) / text-to-speech (Piper) | **Spare Mac** | Small models; `whisper.cpp` Metal backend uses the AMD GPU. |
| LLM inference (Ollama) | **Not here** | On Intel macOS, Ollama is **CPU-only** — the AMD GPU is unused. |

### What the AMD (non-NVIDIA) GPU does and doesn't accelerate

- ✅ **VideoToolbox** H.264/HEVC video encode (FFmpeg `h264_videotoolbox` / `hevc_videotoolbox`)
- ✅ **Chrome** GPU-accelerated page rendering / WebGL
- ✅ **`whisper.cpp`** speech-to-text via its **Metal** backend (works on any
  Metal-capable GPU, including AMD on Intel Macs)
- ❌ **LLM inference.** Ollama's GPU path is Apple-Silicon Metal or Linux
  CUDA/ROCm. On an Intel Mac it falls back to **CPU**, so the AMD GPU gives you
  nothing for large models. Keep LLMs on the primary (Claude API) or a
  GPU-capable host.

---

## 1. Topology & networking

```
            ┌──────────────────────────────┐         ┌──────────────────────────────┐
            │           PRIMARY            │         │       SPARE INTEL MAC        │
            │  (keep these local)          │   LAN   │  (offload these)             │
            │                              │ ⟷ ⟷ ⟷ ⟷ │                              │
            │  • gateway (127.0.0.1:3000)  │         │  • Chrome (CDP :9222)        │
            │  • SQLite store + memory     │         │  • FFmpeg / hyperframes      │
            │  • agent loop                │         │  • signal-cli-rest-api :8080 │
            │  • Claude (Anthropic API)    │         │  • MCP servers               │
            │                              │         │  • whisper.cpp / Piper (MCP) │
            └──────────────────────────────┘         └──────────────────────────────┘
```

**Do not** move the gateway, store, or memory. The gateway binds to loopback
only, and the store/memory are local SQLite databases with no network/sync
backend — each instance owns its own.

**Connectivity options (most private first):**

1. **Tailscale / WireGuard** — both Macs join a private mesh; use the tailnet IPs.
   Recommended if the machines aren't always on the same LAN.
2. **SSH tunnel** — e.g. `ssh -N -L 9222:localhost:9222 user@spare-mac` to forward
   a single service port over an encrypted channel.
3. **Plain LAN** — same trusted network only. Bind helper services to the private
   interface, not `0.0.0.0` on an untrusted network.

**Security notes:**
- Browser navigation is governed by an SSRF policy (`SsrfPolicy` in
  `crates/rustykrab-tools/src/browser/config.rs`). If your helper Chrome needs to
  reach private addresses, set `allow_private_network` / `hostname_allowlist`
  deliberately rather than disabling protection wholesale.
- On Linux/Docker hosts, `RUSTYKRAB_MASTER_KEY` is required to encrypt secrets at
  rest. This guide doesn't move the store, so the key stays on the primary.

---

## 2. Role 1 — Browser / screenshot worker

RustyKrab can attach to a Chrome running on another machine over the Chrome
DevTools Protocol (CDP). Remote profiles are first-class: see `BrowserProfile`
(`cdp_url`, `driver: remote`) in `crates/rustykrab-tools/src/browser/config.rs`.

**On the spare Mac** — launch Chrome with CDP listening on the network:

```bash
/Applications/Google\ Chrome.app/Contents/MacOS/Google\ Chrome \
  --remote-debugging-port=9222 \
  --remote-debugging-address=0.0.0.0 \
  --user-data-dir="$HOME/.rustykrab-chrome"
```

> Chrome restricts `--remote-debugging-address=0.0.0.0` for security. The safest
> setup is to bind to localhost on the spare Mac and reach it through an **SSH
> tunnel** or **Tailscale** instead of exposing 9222 on the LAN.

**On the primary** — point RustyKrab at it. Easiest (single endpoint):

```bash
export CHROME_CDP_URL=http://<spare-mac-ip>:9222
```

This sets the default profile's `cdp_url` and switches its driver to attach-mode
(`config.rs` env override). For multiple named browsers, use
`~/.rustykrab/browser.json` instead:

```json
{
  "defaultProfile": "remote",
  "profiles": {
    "remote": {
      "driver": "remote",
      "cdpUrl": "http://<spare-mac-ip>:9222"
    }
  }
}
```

**Verify:** from the primary, run a browser/screenshot tool and confirm the page
renders (the spare Mac's Chrome window should navigate). `remoteCdpTimeoutMs`
(default 5000) governs the connection timeout if the link is slow.

---

## 3. Role 2 — Video render worker

The video channel shells out to `npx hyperframes` → FFmpeg + Chrome
(`crates/rustykrab-channels/src/video.rs`, `crates/rustykrab-tools/src/video.rs`).
Running it on the spare Mac lets FFmpeg use the AMD GPU's VideoToolbox encoder.

**On the spare Mac**, install the dependencies:

```bash
brew install ffmpeg node          # Node >= 22 required by hyperframes
# Chrome must also be installed (used by hyperframes for HTML rendering)
```

Confirm VideoToolbox hardware encoders are present (this is the GPU win):

```bash
ffmpeg -hide_banner -encoders | grep videotoolbox
# expect: h264_videotoolbox, hevc_videotoolbox, prores_videotoolbox
```

Enable the video channel on whichever RustyKrab process owns rendering:

```bash
export RUSTYKRAB_VIDEO=true
export RUSTYKRAB_NPX_PATH=/opt/homebrew/bin/npx   # only if npx isn't on PATH
```

> Because video rendering is invoked in-process by the agent, the cleanest way to
> truly run renders on the spare Mac is to host the agent process there, or to
> expose hyperframes/FFmpeg as a small service the primary calls. If you just want
> the GPU encode locally on the helper, run the RustyKrab instance that handles
> video on the spare Mac with `RUSTYKRAB_VIDEO=true`.

**Verify:** trigger a render and confirm an MP4 is produced; check Activity
Monitor's GPU history to see VideoToolbox engaged during encode.

---

## 4. Role 3 — Service host

Move always-on network services off the primary.

### signal-cli-rest-api (Docker)

```bash
# On the spare Mac
docker run -d --name signal-api -p 8080:8080 \
  -v "$HOME/.local/share/signal-api:/home/.local/share/signal-cli" \
  bbernhard/signal-cli-rest-api
```

Point RustyKrab at it (`SIGNAL_CLI_URL`, default `http://localhost:8080`, read in
`crates/rustykrab-cli/src/main.rs`):

```bash
export SIGNAL_ACCOUNT=+15551234567
export SIGNAL_CLI_URL=http://<spare-mac-ip>:8080
export SIGNAL_ALLOWED_NUMBERS=+15557654321
```

**Verify:** `curl http://<spare-mac-ip>:8080/v1/health` returns OK; send a Signal
message end-to-end.

### MCP servers

Host MCP servers on the spare Mac and register them from the primary
(`RUSTYKRAB_MCP_*`, `crates/rustykrab-tools/src/mcp_connector.rs`):

```bash
export RUSTYKRAB_MCP_SERVERS=myserver
export RUSTYKRAB_MCP_MYSERVER_TRANSPORT=http
export RUSTYKRAB_MCP_MYSERVER_URL=http://<spare-mac-ip>:9000/mcp
export RUSTYKRAB_MCP_MYSERVER_TOKEN=ref:store:mcp.myserver.token   # optional
```

### Telegram / Signal polling

Telegram and Signal both work in long-poll mode (no public IP needed), so a
RustyKrab instance running these channels can live entirely on the spare Mac.

---

## 5. Bonus — Speech (STT / TTS) on the AMD GPU

**Can the GPU run a small speech model? Yes.** STT and TTS models are far smaller
than LLMs, and `whisper.cpp` can actually use the AMD GPU here.

> **Heads-up:** RustyKrab does not transcribe audio yet. Voice messages are
> received but dropped with `"[User sent an audio message. Audio transcription is
> not yet supported.]"` (`crates/rustykrab-core/src/types.rs`). So you consume a
> speech service through the **agent's tools/MCP**, or via a future code change
> that wires transcription into the audio content path — not an existing switch.

### STT — whisper.cpp (GPU via Metal)

```bash
brew install whisper-cpp                       # or build from source with Metal
whisper-cli -m models/ggml-base.en.bin -f clip.wav   # base ≈ 75 MB
```

- `base`/`small` (~75–500 MB) transcribe faster-than-realtime with Metal offload.
- Prefer `whisper.cpp` over `faster-whisper` here — the latter needs CUDA and
  would run CPU-only on an Intel Mac.

### TTS — Piper / Kokoro (CPU is fine)

Piper voices are tens of MB and synthesize in real time on CPU; the GPU isn't
even required.

### Wiring it in

Wrap whichever you need in a small HTTP or MCP server on the spare Mac and expose
it as an MCP tool (see Role 3). The agent can then call `transcribe` / `speak`
explicitly as part of a conversation.

---

## 6. Environment variable quick reference

| Variable | Where | Purpose |
|---|---|---|
| `CHROME_CDP_URL` | primary | Attach the default browser profile to a remote Chrome CDP endpoint |
| `CHROME_CDP_PORT` | primary | CDP port for the default profile (alternative to a full URL) |
| `RUSTYKRAB_VIDEO` | video host | Enable the video rendering channel (`true`/`false`) |
| `RUSTYKRAB_NPX_PATH` | video host | Override the `npx` binary path used by hyperframes |
| `SIGNAL_ACCOUNT` | Signal host | Bot phone number (E.164) |
| `SIGNAL_CLI_URL` | Signal host | URL of `signal-cli-rest-api` (default `http://localhost:8080`) |
| `SIGNAL_ALLOWED_NUMBERS` | Signal host | Comma-separated allowed E.164 numbers |
| `RUSTYKRAB_MCP_SERVERS` | primary | Comma-separated MCP server names to load |
| `RUSTYKRAB_MCP_<NAME>_TRANSPORT` | primary | `http` or `stdio` |
| `RUSTYKRAB_MCP_<NAME>_URL` | primary | HTTP MCP endpoint |
| `RUSTYKRAB_MCP_<NAME>_TOKEN` | primary | Bearer token (supports `ref:store:` / `ref:keychain:`) |

For the full configuration list, see the **Configuration** section of `README.md`.
