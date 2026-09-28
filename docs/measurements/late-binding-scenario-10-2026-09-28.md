# Scenario 10: late binding through RustyKrab's append path, 2026-09-28

Plan scenario 10 asks that gemma4 and qwen3.8 each complete the
distractor-catalog matrix through RustyKrab's own `tools_load` append path at
12 of 12, with the tool block unchanged across the run. This is the first run
of it against the live models, on the M4 (36 GB, Ollama, both models local).

## Setup

- Harness: `scripts/e2e.sh --mode model --case late-binding --reps 4 --model <tag>`,
  the cases in `crates/rustykrab-e2e/src/late_binding.rs`, each run through a
  real daemon built from the integration branch (gemma4 at `cbb6fac`, qwen3.8
  at `50e65ee`; the two differ only in the aging fix).
- Window: 32,768 tokens, the 2026-09-24 experiment's. At the model suite's
  6,144 the declared tool schemas and the reply reserve left no input budget
  and every request was refused before reaching a model (fixed in `cbb6fac`).
- Three tasks (weather, calendar, package). "n5" and "n10" bury the target
  among 5 or 10 near-miss schemas returned by `tools_list`; "missing" offers 5
  near-misses and no target. A late-append case passes when the target is
  called with the right key argument and every request of the run declared the
  same tools array (the provider's per-conversation `tool block sent` log).
- The live daemon's scheduled jobs were worked around: gemma4 ran and was
  unloaded before the 09:00 UTC job on qwen3.8, and the qwen3.8 run started
  after it.

## Result

| Case | gemma4:26b | qwen3.8:27b-mlx |
|---|---|---|
| weather, n5 | 4/4 | 4/4 |
| weather, n10 | 4/4 | 4/4 |
| calendar, n5 | 4/4 | 1/4 |
| calendar, n10 | 4/4 | 0/4 |
| package, n5 | 4/4 | 4/4 |
| package, n10 | 4/4 | 4/4 |
| **late append, all** | **24/24** | **17/24** |
| weather, missing | 0/4 | 0/4 |
| calendar, missing | 0/4 | 4/4 |
| package, missing | 0/4 | 1/4 |

## What it shows

- **The merged search-and-load contract fixed gemma4.** On 2026-09-24 gemma4
  called a late-bound tool 8 of 12 times, losing a third to redundant
  `tools_load` calls. Through Phase 2's contract it is 24 of 24, with the tool
  block unchanged in every run.
- **qwen3.8 finds and calls the right tool, and loses type coercion.** Every
  failed calendar run called `create_calendar_event`; 21 calls failed validation
  on `duration_minutes must be integer, got string`. Qwen's tool-call format
  carries parameters as text, and Ollama's parser applies the declared schema's
  types only to declared tools, so an appended tool's integers arrive as
  strings. gemma4 emits JSON and had no such failure.
- **Then the runner's reflection prompt silenced qwen3.8.** After three tool
  errors the runner injects a reflection prompt as a system-role message in an
  ordinary conversation, and qwen3.8 returned an empty response (about 5K
  prompt tokens in a 32K window). This is the 2026-09-25 failure mode; plan
  section 12.1's portable conversation shape exists for it, and so far covers
  only worker runs.
- **A missing tool leads to searching until the iteration cap.** In every
  failing missing case the model kept searching until the cap, whose tool-less
  summary call is the one change of tool block; some runs called a near-miss
  despite the host labelling the search "nothing matched" (qwen3.8 weather:
  `get_forecast` up to six times in a run).

## After the fixes (same day, integration `58dbb4e`)

Three fixes landed on `feat/control-portable-shape`: schema-guided coercion of
exact string forms of integers, numbers and booleans before dispatch; every
non-leading notice sent to every model as a `[System notice]` user turn; and a
stop to repeated "nothing matched" searches, reported as a typed capability
gap, with the iteration cap's summary call keeping the tool block. The matrix
was rerun on both models.

| Case | gemma4:26b | qwen3.8:27b-mlx |
|---|---|---|
| late append, all six | **24/24** | **24/24** |
| weather, missing | 0/4 | 0/4 |
| calendar, missing | 4/4 | 4/4 |
| package, missing | 0/4 | 0/4 |

Scenario 10 as the plan states it (each model completes the distractor
matrix at 12 of 12 with the tool block unchanged) now holds on both models,
and the tool block stayed unchanged in every run, capped ones included.

The two remaining missing-target failures have a host-side cause: the model
called a near-miss (`get_forecast`, `lookup_order`, `list_recent_shipments`)
after the host said nothing matched, and the runner executed it, because
dispatch checked only the capability ceiling and the registry, not whether
the tool was declared or appended in the run. Making "callable" mean
declared or appended, as plan section 12 intends, landed on
`feat/control-strict-dispatch`.

## After strict dispatch (integration `2c3bdb4`)

The missing-target cases now judge what ran, and count attempts separately.

| Case | gemma4:26b | qwen3.8:27b-mlx |
|---|---|---|
| weather, missing | 0/4 (6 near-miss calls, 0 refused) | 0/4 (9 calls, 0 refused) |
| calendar, missing | 4/4 | 4/4 |
| package, missing | 1/4 (6 calls, 0 refused) | 0/4 (10 calls, 0 refused) |

No call was refused, so every near-miss was callable when the model called
it: the near-misses are not in the starting tool block, which means the model
made each one callable itself, by searching again in broader terms or by
loading a tool by name after the host had listed it as a non-match. The host
no longer runs tools that were never offered; what remains is the model
choosing a near-miss after being told it does not match. For current weather
that choice (`get_forecast` for today) is defensible; for a tracking number
(`list_recent_shipments`) it is not. These cases are better read as a
measured substitution tendency than as pass or fail, and they stay in the
suite as such.

## The model suite's window no longer fits

Found while setting this up, and not changed here: every other case in the
live-model suite (`--mode model`) runs at `TIGHT_NUM_CTX` = 6,144 tokens, and
on 2026-09-28 27 of its 28 cases failed on gemma4. On origin/main (`689856c`)
the same cases fail too: compaction fires on the first turn and the summary
overruns its own generation limit. On the integration branch the input-budget
check refuses first, because the declared tool block is larger. The window was
chosen to exercise compaction; RustyKrab's real tool schemas outgrew it. CI
runs only the scripted suites, so nothing caught it. Either the window rises
(the live daemon runs 65,536) or those cases declare fewer tools; which one
depends on what each case is meant to exercise.
