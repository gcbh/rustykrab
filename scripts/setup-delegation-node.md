# Setting up a delegation node

Stand up a second RustyKrab instance on another Mac and let the primary hand it
tasks via the `nodes` tool.

## Read this first: what delegation actually is

A delegated task runs **entirely on the remote node**, using *that machine's*
tools, filesystem, and model. It does not share the primary's conversation.

Consequences worth planning around:

- **The node needs its own checkout** of any repo it will work on. It cannot see
  files on the primary.
- **Results come back as text**, not as edits on the primary. To move actual code,
  have the node commit and push a branch, then pull it here.
- **The message must be self-contained** — restate the goal, paths, and
  constraints. The node does not share the primary's conversation.

If what you want is "one agent, more compute", this is not that. It is "a second,
independent agent you can hand a well-specified job to."

## How a delegation runs

`send` does not wait. It submits the task to the node's queue and returns a
`task_id` in about a second; the model then does other work and collects the
result with `check` on a later turn.

```
nodes send   -> {"task_id": "...", "status": "queued"}
nodes check  -> {"status": "running", "elapsed_secs": 94}
nodes check  -> {"status": "done", "response": "...", "conversation_id": "..."}
nodes cancel -> {"status": "cancelled"}      # aborts it mid-run if started
```

Three things follow from that shape:

- **A long task no longer dies on the caller.** The agent loop caps a network
  tool call at 120 seconds. A synchronous delegation exceeded that on anything
  real — the caller timed out while the node kept running, and the result was
  unreachable.
- **The node runs one task at a time.** Its model has a single KV-cache slot
  (`OLLAMA_NUM_PARALLEL=1`), so interleaving two delegated conversations evicts
  both prompt prefixes; serialising is faster than sharing. Queued work is
  drained oldest-first, except that a task continuing the conversation the node
  just ran is preferred — that thread's prefix is still warm.
- **Follow-ups should continue the thread.** Pass the `conversation_id` from
  `check` back into the next `send`. On this hardware a fresh thread spent 79.5s
  evaluating its own prompt before answering; the second turn of the same thread
  spent 3.3s.

Tasks survive a restart of either machine: the queue is persisted, and a task
the node was mid-way through when it died is reported as failed rather than
left pending forever.

## Recursion

`hop_budget` on a node entry says how many further delegations the work may
make, and defaults to `0`. At zero the node is denied the `nodes` tool for that
run, so it cannot hand any part of the task onward.

This matters specifically because the node is another copy of the same program.
If it also lists the primary in its own `RUSTYKRAB_NODES`, a task can bounce
between the two machines indefinitely, costing minutes of local inference per
hop. The `subagents` tool's depth counter does not help — it is process-local
and does not cross the network.

If a node genuinely needs to break work into parts, it should queue those parts
for itself rather than delegate them outward.

## 1. On the node machine

Build and bundle (the `.app` bundle is required for keychain access — see
`scripts/bundle.sh`):

```bash
git clone <repo> rustycrab && cd rustycrab
make bundle          # release build + codesign into a .app
```

Set its model. **Use the MLX build, not the plain GGUF tag:**

```bash
ollama pull qwen3.8:27b-mlx

export RUSTYKRAB_PROVIDER=ollama
export OLLAMA_MODEL=qwen3.8:27b-mlx
```

The plain `qwen3.8:27b` tag ships a multi-token-prediction (MTP) head, and Ollama
launches it with `--spec-type draft-mtp` by default. On M1/M4-class Metal that
speculative-decoding path is a measured **regression**, not a win — see the
tuning table below. The MLX engine avoids it.

Or point at any OpenAI-compatible server (llama.cpp, mistral.rs, LM Studio):

```bash
export RUSTYKRAB_PROVIDER=llama-server
export OPENAI_BASE_URL=http://localhost:8080
export OPENAI_MODEL=qwen3.8:27b
export OPENAI_MAX_TOKENS=4096      # thinking models need headroom; too low and
                                   # the agent loops on "hit max tokens"
```

Start it and note the auth token it prints on first run (or set
`RUSTYKRAB_AUTH_TOKEN` yourself):

```bash
./target/release/RustyKrab.app/Contents/MacOS/rustykrab-cli
```

## 2. Make it reachable

The gateway binds to **loopback only, by design** — that is not configurable, and
should stay that way. Expose it deliberately instead. Two good options:

**Tailscale Serve** (recommended for always-on). Terminates on the tailnet and
proxies to loopback, so nothing is exposed to the local network or internet:

```bash
tailscale serve --bg 3000
tailscale serve status        # note the https://<machine>.<tailnet>.ts.net URL
```

**SSH tunnel** (fine for ad-hoc use). Run on the *primary*:

```bash
ssh -N -L 3100:localhost:3000 <node-host>
```

The node is then `http://127.0.0.1:3100` from the primary's perspective.

## 3. On the primary

Register the node. `description` is shown to the model, so say what the node is
good for — it uses this to choose:

```bash
export RUSTYKRAB_NODES='[
  {
    "id": "m4max",
    "url": "https://your-machine.your-tailnet.ts.net",
    "token": "<the node's auth token>",
    "description": "M4 Max 32GB — qwen3.8:27b. Slow but capable; good for self-contained coding tasks. Has its own checkout at ~/code/rustycrab."
  }
]'

# Local models are slow; give delegated tasks room to finish.
export RUSTYKRAB_NODE_TIMEOUT_SECS=1800
```

Restart the primary so it picks up the config. The `nodes` tool stays hidden from
the model until `RUSTYKRAB_NODES` is set.

If the primary runs as an installed LaunchAgent rather than from a shell,
re-run `scripts/install.sh` after exporting these: the installer copies a fixed
list of variables into the plist, and a daemon started by launchd sees only
what is in there.

## 4. Verify

```bash
RUSTYKRAB_NODES='...' cargo test -p rustykrab-tools --test nodes_live -- --ignored --nocapture
```

That checks `list`, `discover` (reachability + latency), a real submit-and-poll
round trip, and a cancel.

## Measured tuning findings

All on an M1 Max 64GB with the **GGUF** `qwen3.8:27b` build, 8k-token prompts.
Ratios should transfer to the M4 Max even though absolute numbers differ.

| Config | Generation | 8k prefill |
|---|---|---|
| Ollama default (MTP on) | 7.9 tok/s | 88.7 s |
| llama-server, flash attention **off** | 8.4 tok/s | 91.7 s |
| llama-server, flash attention **on** | **9.9 tok/s** | **88.2 s** |
| ...plus `-ub 2048` | 9.3 tok/s | 110.3 s |
| ...plus MTP speculative decoding | 8.5 tok/s | 118.4 s |
| ...plus `-ctk/-ctv q8_0` | 8.1 tok/s | 131.8 s |

Four things worth knowing, three of which contradict commonly repeated advice:

- **Flash attention on is the one clear win** — +18% generation. Use `-fa on`.
- **Do not raise the prefill micro-batch.** `-ub 2048` is widely recommended for
  Apple Silicon and it cost ~20% prefill here. Leave the default.
- **Do not enable MTP speculative decoding on Metal.** Draft acceptance measured
  only 38-40%, so it loses more to verification than it gains: -14% generation
  and -25% prefill. It is a large win on CUDA, which is where most of the
  positive reports come from.
- **Do not quantize the KV cache.** This architecture uses 256-dim heads with
  sparse Metal flash-attention kernel coverage; `q8_0` made both axes worse.
  Advice to the contrary is generally measured on NVIDIA hardware.

**Multi-turn is much cheaper than cold turns.** In a two-turn conversation with a
~4.9k-token system prompt, turn 1 paid 79.5 s of prefill and turn 2 paid only
**3.3 s** — the slot cache reuses the prefix. Long-running conversations amortize
well; it is *fresh* delegated tasks that pay full price. `send` accepts a
`conversation_id` precisely so a follow-up need not be one.

## Performance expectations

Measured on an M1 Max, delegating a *trivial* task ("reply with exactly OK") to a
peer running gemma4:26b — the fast model at ~46 tok/s:

**~43 seconds.**

That is the floor, not the ceiling. It is dominated by the node's ~8k-token system
prompt plus tool schemas, and by thinking-model reasoning tokens that never reach
the caller. On qwen3.8:27b — roughly 5x slower on both generation and prompt
processing — the same trivial round trip is closer to **3-4 minutes**, and a real
coding task considerably longer.

Plan accordingly:

- Delegate **batch or background work**, not anything interactive.
- Prefer **few large tasks** over many small ones — per-task overhead dominates.
- Continue a thread rather than opening a new one, which is what turns that
  per-task overhead from ~80s into ~3s.
- Benchmark the node first with `scripts/bench-local-models.md`. If prompt
  processing there is slow, every delegated task inherits it.
- A faster model on the node often beats a smarter one, because the overhead is
  paid per task regardless. Worth A/B-ing gemma4:26b against qwen3.8:27b on your
  actual workload before committing.

## Security notes

- Auth is the node's **bearer token**, over a private network. Treat the token as
  a credential: it grants full agent access, including shell execution on that
  machine.
- Keep the node bound to loopback and reach it via tailnet or SSH. Do not expose
  the gateway port directly.
- `nodes list` deliberately never returns tokens — tool output goes into the
  model's context.
- The peer's Origin check is CSRF protection against browsers, not authentication;
  node-to-node calls set their own loopback origin. The real boundary is the token
  plus network reachability.
