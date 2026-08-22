# rustykrab-e2e

End-to-end evaluation harness. Boots a throwaway daemon on its own data
directory and an ephemeral port, drives scenarios over HTTP, and asserts on
the responses **and** on what the daemon persisted. Prints a JSON report
and exits 0 when green.

```sh
make e2e            # deterministic plumbing scenarios — no model, seconds
make eval           # gemma4:26b behaviour scenarios, 3 repetitions each
make eval-quick     # one repetition, skipping the slow scenarios
make eval-list      # list every scenario
```

Flags pass through: `make eval ARGS="--case compaction"`, or call
`./scripts/e2e.sh --mode all --release` directly.

## Two modes, deliberately not comparable

**scripted** — `RUSTYKRAB_PROVIDER=scripted` replays a fixed JSON script of
tool calls and text turns. No model, no network, no sampling. It answers
"does the server still do what it says": auth, the origin allowlist,
conversation CRUD, SSE framing, secrets, the credential guard, device
pairing. Every scenario must pass every run — determinism is the point, so
two out of three is a broken scenario, not an acceptable rate. This is the
mode for CI.

**model** — a real model with the tool registry replaced by scripted stubs.
It answers "how does the framework behave with a model in the loop": tool
selection, argument fidelity, recovery from a broken tool, honest reporting
when a tool never recovers, what survives compaction, whether memory comes
back. Slow, sampled, and scored as a pass rate over repetitions, because
with a 26B model "passed once" and "passed every time" are both misleading.
A scenario passes on a majority of its repetitions and is flagged `~flaky`
when it passes some and fails others.

The report never merges the two. A scripted pass says the plumbing works; a
model pass says the plumbing works *and* gemma4 could use it.

## Scripting the model, scripting the tools

`ScriptedProvider` scripts the **model** so the plumbing can be tested
without one. `RUSTYKRAB_TOOL_STUBS` is the mirror image: it scripts the
**tools** so a real model can be tested against situations a live tool
would only reach by luck.

```json
{
  "mode": "replace",
  "keep": ["memory_search", "memory_save"],
  "tools": [{
    "name": "weather_lookup",
    "description": "Get the current weather for a city.",
    "parameters": { "type": "object",
                    "properties": { "city": { "type": "string" } },
                    "required": ["city"] },
    "script": { "responses": [
      { "type": "err", "message": "upstream timed out", "kind": "timeout" },
      { "type": "ok",  "value": { "temperature_c": 17, "conditions": "sleet" } }
    ]}
  }]
}
```

`responses` is indexed by call number. `repeat_last` (default true) makes
the final entry answer every later call; set it false and further calls
fail with "script exhausted", which is how a scenario checks that an agent
stops retrying. Response types are `ok`, `err` (with a real
`ToolErrorKind`, so the agent loop's retry and reflection paths actually
fire), `filler` (an oversized payload, for pushing a window past the
compaction trigger), and `delay`.

`mode: "replace"` leaves the model with only the stubs plus anything in
`keep`. That is usually what you want: thirty real tools give a small model
thirty ways to wander off, and the scenario stops measuring what it meant
to. The memory scenarios keep the genuine `memory_*` tools, because those
are the thing under test.

## How assertions see the run

Everything comes from `GET /api/conversations/{id}` — the record the daemon
persisted. Tool calls, tool arguments, tool *results*, the compaction
bookmark, the summary, and the full message history are all in there, so
the assertions read what the system stored rather than the harness's own
bookkeeping. `ToolOutputContainsAny` is the useful consequence: a memory
scenario can assert that retrieval actually returned the fact, separately
from whether the model then used it well.

An LLM judge covers only what substring matching cannot — whether an answer
that admits a tool failed is an honest report or a hedge around an invented
number. `claude-sonnet-5` grades when `ANTHROPIC_API_KEY` is set, the model
under test grades itself otherwise, and the report always names the judge.
The judge only runs on repetitions whose hard assertions already passed.

## Compaction without an hour of inference

Compaction fires on estimated tokens, so the compaction scenarios write a
`harness.toml` into the throwaway data dir with `max_context_tokens =
6000`. That reaches `AgentRunner::maybe_compact` — the same code path, the
same bookmark logic — after a handful of turns instead of hundreds. Facts
that must survive are stated in the first turn, so they land in the region
that gets folded away; a fact sitting in the verbatim tail proves nothing
about the summary.

## Prerequisites

The scripted mode needs nothing but a build. The model mode needs Ollama
running with the model pulled (`ollama pull gemma4:26b`).

Preflight checks all of it up front, including one trivial generation:
Ollama serves one request at a time per model, so another client holding
the slot — a running RustyKrab daemon is the usual culprit — does not slow
the suite down, it stops it. Better to fail in one line than to time out
every scenario twenty minutes in.

## The xfail convention

A scenario encoding behaviour that is not built yet is marked `XFail`. The
suite stays green while it fails, and an **unexpected pass turns the suite
red** so the scenario gets promoted to `Pass`. Shipping a phase is then a
matter of flipping its scenarios over.

## Known limits

- Scenarios run sequentially. One 26B model behind one GPU means
  concurrency would only make each scenario slower and the timings
  meaningless.
- The model mode boots a fresh daemon per repetition. That costs a few
  seconds each and buys the only thing that makes repetitions meaningful:
  no scenario can pass on state another one left behind.
- Skills are not installed into the throwaway data dir, so the system
  prompt carries no skill catalog. Nothing in the current scenarios
  depends on one.
