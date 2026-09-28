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
- **Interleaving is the dominant cause.** 31 of 46 changes came from runs
  sharing one model, each switch paying for the other run's prefix. The
  single-slot rule of plan section 12.1, and the Phase 1 close-out gate that
  holds local leases while an interactive turn runs, address it directly.
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
