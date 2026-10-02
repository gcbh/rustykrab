# Local model benchmark

Portable benchmark for comparing local LLMs served by Ollama on Apple Silicon.
Self-contained — it does not need the RustyKrab repo checked out, only Ollama.

## Why these three metrics

Agent workloads are dominated by two costs that a single "tokens/sec" number hides:

- **Generation speed** — how fast the answer streams once it starts. Bandwidth-bound.
- **Prompt processing** — time to first token on a large prompt. RustyKrab's system
  prompt plus its tool schemas is **~8k tokens before any conversation**, so every
  cold agent turn pays this. Compute-bound, and it varies far more between models
  than generation speed does.
- **Reasoning vs content split** — "thinking" models spend part of their token budget
  on reasoning that never reaches the user. A model can look fast and still take a
  long time to produce a visible answer.

## Prerequisites

```bash
# Ollama must be installed and running
ollama --version

# Pull whichever models you want to compare (~17-18 GB each)
ollama pull gemma4:26b
ollama pull qwen3.8:27b
ollama pull qwen3:30b-a3b

df -h /System/Volumes/Data | tail -1   # check free space first
```

Each model loads fully into RAM while it is benchmarked. On a 32 GB machine run
them one at a time; Ollama unloads the previous model automatically, but a very
large model plus a loaded desktop can still push into swap and distort results.

## Run it

Copy-paste this whole block. It writes the script and runs it.

```bash
cat > /tmp/bench-models.py << 'EOF'
import json, subprocess, sys, time, urllib.request

URL = "http://localhost:11434/v1/chat/completions"
GEN_PROMPT = "Write a detailed paragraph about the ocean."
GEN_MAX_TOKENS = 400
BIG_PROMPT_CHARS = 48000          # ~8k tokens, approximating an agent system prompt
                                  # (this filler runs ~6 chars/token; the script
                                  # reports the server's actual prompt_tokens)


def big_prompt(model):
    """Fixed-size large prompt with a unique prefix.

    The size must not depend on the model name, or a longer name silently makes a
    longer prompt and the numbers stop being comparable. The prefix appears once,
    purely to defeat Ollama's prompt cache.
    """
    prefix = f"Session {model}. "
    filler = "Reference material to ignore. "
    body = filler * (BIG_PROMPT_CHARS // len(filler) + 1)
    return prefix + body[:BIG_PROMPT_CHARS - len(prefix)] + "\nReply with exactly: ok"


def stream(model, prompt, max_tokens):
    """Returns (ttft, elapsed, completion_tokens, n_reasoning, n_content, prompt_tokens)."""
    body = json.dumps({
        "model": model, "messages": [{"role": "user", "content": prompt}],
        "max_tokens": max_tokens, "temperature": 0, "stream": True,
        "stream_options": {"include_usage": True},
    }).encode()
    req = urllib.request.Request(URL, data=body, headers={"Content-Type": "application/json"})
    t0 = time.time(); first = None; toks = 0; n_reason = 0; n_content = 0; ptoks = 0
    with urllib.request.urlopen(req, timeout=3600) as r:
        for raw in r:
            s = raw.decode().strip()
            if not s.startswith("data: ") or s[6:] == "[DONE]":
                continue
            j = json.loads(s[6:])
            if j.get("usage"):
                toks = j["usage"]["completion_tokens"]
                ptoks = j["usage"].get("prompt_tokens", 0)
            ch = j.get("choices") or []
            if not ch:
                continue
            d = ch[0].get("delta") or {}
            # Thinking models stream reasoning in `delta.reasoning` with empty content.
            if (d.get("content") or d.get("reasoning")) and first is None:
                first = time.time() - t0
            if d.get("reasoning"):
                n_reason += 1
            if d.get("content"):
                n_content += 1
    return first, time.time() - t0, toks, n_reason, n_content, ptoks


def hardware():
    try:
        out = subprocess.run(["system_profiler", "SPHardwareDataType"],
                             capture_output=True, text=True, timeout=60).stdout
        chip = next((l.split(":")[1].strip() for l in out.splitlines() if "Chip:" in l), "?")
        mem = next((l.split(":")[1].strip() for l in out.splitlines() if "Memory:" in l), "?")
        return f"{chip}, {mem}"
    except Exception:
        return "unknown"


def models():
    if len(sys.argv) > 1:
        return sys.argv[1:]
    out = subprocess.run(["ollama", "list"], capture_output=True, text=True).stdout
    return [l.split()[0] for l in out.splitlines()[1:] if l.strip()]


print(f"Hardware: {hardware()}\n")
rows = []
for m in models():
    try:
        # Warm up twice: the first call pays model load, which would otherwise be
        # attributed to whichever measurement ran first.
        stream(m, "hi", 8)
        stream(m, "warm up please", 32)

        first, el, toks, n_reason, n_content, _ = stream(m, GEN_PROMPT, GEN_MAX_TOKENS)
        gen = toks / el if el and toks else 0

        big_ttft, _, _, _, _, big_ptoks = stream(m, big_prompt(m), 8)

        print(f"{m:<16} gen {gen:5.1f} tok/s | first-token {first:5.2f}s | "
              f"big prompt ({big_ptoks} tok) {big_ttft:6.2f}s | "
              f"reasoning/content deltas {n_reason}/{n_content}")
        rows.append((m, f"{gen:.1f}", f"{first:.2f}", f"{big_ttft:.2f}", str(big_ptoks),
                     f"{n_reason}/{n_content}"))
    except Exception as e:
        print(f"{m:<16} FAILED: {e}")
        rows.append((m, "FAIL", "-", "-", "-", "-"))

print("\n--- paste-able results ---\n")
print(f"Hardware: {hardware()}\n")
print("| Model | Generation (tok/s) | First token (s) | Big prompt (s) | Prompt tokens | reasoning/content |")
print("|---|---|---|---|---|---|")
for r in rows:
    print(f"| {r[0]} | {r[1]} | {r[2]} | {r[3]} | {r[4]} | {r[5]} |")
EOF

python3 /tmp/bench-models.py
```

To benchmark only specific models, pass them as arguments:

```bash
python3 /tmp/bench-models.py gemma4:26b qwen3.8:27b
```

## Baseline to compare against — M1 Max, 64 GB

Measured with this exact script and method. Higher generation is better; **lower**
first-token and 8k-prompt times are better.

| Model | Generation (tok/s) | First token (s) | 8k prompt (s) | Prompt tokens | reasoning/content |
|---|---|---|---|---|---|
| qwen3:30b-a3b | **51.7** | **0.27** | 16.52 | 8025 | 377/21 |
| gemma4:26b | 45.9 | 0.63 | **15.09** | 8028 | 352/0 |
| qwen3.8:27b | 8.4 | 0.94 | 85.51 | 8023 | 99/294 |

Reading it:

- **qwen3:30b-a3b** and **gemma4:26b** are close — within ~12% on generation, and
  gemma4 is marginally *faster* on prompt processing. Either is viable.
  qwen3:30b-a3b (`qwen3moe`) has no vision; gemma4:26b does.
- **qwen3.8:27b is ~5.5x slower on generation and ~5.7x slower on prompt
  processing.** At 85s to ingest an 8k prompt, every cold agent turn stalls well
  over a minute before the first token.
- **`reasoning/content` shows how much of the budget is invisible thinking.**
  gemma4 spent all 400 tokens reasoning and emitted *zero* content on this prompt —
  it needs a higher `max_tokens` to produce a visible answer. qwen3.8 thinks
  briefly then answers.

All three pass tool-calling correctness — this table is purely about speed.

Note gemma4:26b sustains ~46 tok/s from a ~17 GB model. On a 400 GB/s M1 Max a
*dense* model that size caps out near 400/17 ≈ 23 tok/s, since every token must
read every weight. Exceeding that ceiling means it is activating only a subset —
evidence that gemma4:26b is sparse/MoE, though Ollama does not expose an expert
count to confirm it directly.

## What to look for on the M4 Max

1. **Does qwen3.8:27b stay slow?** On the M1 Max it is ~6x slower than gemma4 on
   *both* axes, which looks like unoptimized kernels for a very new architecture
   rather than a bandwidth limit. If the M4 Max shows the same ratio, that
   confirms it. If the gap narrows sharply, it was hardware-specific.

2. **Scaling vs bandwidth.** The M4 Max has higher memory bandwidth than the M1
   Max, so generation should improve roughly in proportion. Prompt processing is
   compute-bound and should improve *more* — that is the M4 Max's bigger advantage
   and the number that matters most for agent turns.

3. **The 32 GB ceiling.** These models are ~17-18 GB. Watch for swap: if a result
   is wildly worse than the baseline, check memory pressure before believing it.

```bash
memory_pressure | tail -3
```

## Optional: provider correctness tests

Only if the RustyKrab repo is checked out on that machine. These confirm the
OpenAI-compatible provider handles the model's tool-call format, not speed:

```bash
LIVE_OPENAI_BASE_URL=http://localhost:11434/v1 LIVE_OPENAI_MODEL=gemma4:26b \
  cargo test -p rustykrab-providers --test openai_live -- --ignored --nocapture --test-threads=1
```

## Caveats

- **Warm-up matters.** Without it the first measurement absorbs model load time and
  can read several times worse than reality. The script warms each model twice.
- **Prompt caching.** Repeating an identical large prompt hits Ollama's cache and
  reports an unrealistically fast time. The script varies the filler per model.
- **Speed is not quality.** Nothing here measures reasoning ability or answer
  quality. A slower model may still be the right one.
- **One sample per measurement.** Treat differences under ~15% as noise; the
  differences worth acting on here are multiples, not percentages.
