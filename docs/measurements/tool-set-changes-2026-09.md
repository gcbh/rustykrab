# Tool-set changes on the live daemon, 2026-09-25 to 2026-09-28

Phase 0 of the control-layer plan asks for "a base rate for mid-run loads"
before the append path is tuned. This is that count, taken from the daemon
that runs RustyKrab on the M4 (v5.3.x, qwen3.8:27b-mlx, num_ctx 65536).

## Method

Read-only counts over `~/Library/Application Support/rustykrab/logs`
(`rustykrab.log.2026-09-25` to `.2026-09-28`, UTC days). No message bodies
were read. Three log lines carry the signal:

- `rustykrab_providers::ollama: tool set changed since the last request`,
  with `num_tools` and `tool_tokens`. The provider compares each request with
  the previous request to Ollama, whatever conversation it came from.
- `rustykrab_agent::runner: agent loop iteration` with `iteration=N`; a run
  starts at iteration 1.
- `rustykrab_agent::runner: LLM call completed`, one per model call.

Each tool-set change was classed by the iteration it followed and by whether a
`tools_load` or `tools_list` call had succeeded since the run began.

## Result

| Measure | Count |
|---|---|
| Model calls | 174 |
| Runs | 24 |
| Tool-set changes | 46 |
| At the start of a run | 11 |
| Runs interleaving on one model (2026-09-25 15:00 to 15:19 UTC) | 31 |
| Mid-run, after the run loaded a tool | 2 |
| Tool-less calls (compaction summaries) | 2 |

The 31 interleaving changes alternate between three fixed tool blocks (15,
21 and 23 tools) while three runs' iteration counters advance side by side:
the morning after the M1 to M4 cutover, when overdue scheduled jobs and an
interactive turn shared the single local model. The tool blocks never grow
in that window, so these are not loads.

Tool block sizes: median 5,925 tokens (21 tools), smallest 3,823 (15 tools),
largest 8,711 (23 tools), about 280 tokens per tool.

## What it means

- **Mid-run loads are rare.** 2 in 24 runs. The append path (plan section 12)
  is still right, because each such load costs a full re-prefill today, but it
  is not where most prefill time went.
- **Interleaving is the dominant cause of changes, not of prefill.** 31 of 46
  changes came from runs sharing one model. The interleaving measurement
  below shows the MLX engine keeps each conversation's prefix cached, so
  those switches cost milliseconds, not re-prefills; what hurt that morning
  was memory (three concurrent generations and their caches, into swap).
  The single-slot rule of plan section 12.1, and the Phase 1 close-out gate
  that holds local leases while an interactive turn runs, address the
  memory, which is where the cost is.
- **The tool block itself is large.** At qwen3.8's measured ~210 tokens a
  second, the median block costs about 28 seconds of prefill whenever it is
  not cached. That supports section 12's advice to keep the visible set small
  and leave possible needs to the append path.

## Limits

- The change line compares with the previous request globally, so it cannot
  attribute a change to a conversation. Adding the conversation or trace id to
  that line (Phase 2's fingerprint work) removes the inference above.
- Three and a half days, one user, one model, including an unusual morning.
  The count should be retaken after Phase 1's serialisation and Phase 2's
  append path are deployed.
- The ~210 tokens a second figure is from the 2026-09-24 late binding
  experiment at 6.9K tokens, not from these logs.

## Interleaving cost on qwen3.8 (2026-09-28)

Phase 0 also asks for the cost of switching between conversations on one
local model. Measured through Ollama's `/api/chat` on qwen3.8:27b-mlx at the
daemon's `num_ctx` 65,536, thinking off, `num_predict` 8: four distinct
conversations of about 6,400 tokens each, prefilled once, then taken in turns.
`prompt_eval_duration` is the cache signal.

| Step | Prefill |
|---|---|
| Conversation A, cold | 32,546 ms |
| A again | 66 ms |
| B, cold | 33,666 ms |
| A after B, B after A | 63 to 69 ms |
| A or B with one new turn appended | 212 to 216 ms |
| C and D, cold | 32,416 and 33,550 ms |
| A, B, C, D in turn after all four were prefilled | 62 to 148 ms |

- **Switching is nearly free while the prefixes fit the engine's cache.** Four
  6.4K-token conversations stayed warm; a cold 6.4K prefill costs about 33
  seconds (about 195 tokens a second).
- **Memory is the constraint.** With the four conversations cached the model
  was resident at 28.1 GB of the machine's 36 GB, and swap stood at 9.6 of
  10 GB; unloading the model returned free memory to 91%. This matches plan
  section 12.1's "memory, not tokens".
- For the scheduler: serialising local runs protects memory, not prefill.
  Interleaving a few conversations on the MLX engine does not need to be
  avoided for cache reasons.

