# Context continuity evaluation

## Contract

Given a synthetic persisted Broadway task and a follow-up referring to its
time/date information, the real runtime must preserve the task and latest
instruction in the outgoing Ollama request. A real-model run must request a
Broadway-related action rather than switch to the current clock. A separately
specified new clock task must be allowed to switch. These are two separate
scores: context construction and model behavior. Model behavior further separates
task alignment from valid use of the offered tool schema; both must pass.

This suite was built against v5.2.38 (`0b565fd`) after the Broadway incident.
It reconstructs the *failure shape*, not the original unrecorded request bytes.
All input material is synthetic; it does not read production conversations,
credentials, browser profiles, or working memory.

## Run

```sh
# Deterministic CI contract: actual daemon/provider adapter, fake model response.
RUSTYKRAB_CONTEXT_COMPACTION_STRATEGY=structured-message-tail scripts/e2e.sh --mode context --trial-timeout 30

# Real local model, compact cases, both HTTP and Telegram, repeated trials.
scripts/e2e.sh --mode context-model --quick --reps 3 --trial-timeout 240

# Long-context and explicit-reminder ablations (prefill can take several minutes).
scripts/e2e.sh --mode context-model --case broadway-noisy --reps 3 --trial-timeout 900
scripts/e2e.sh --mode context-model --case broadway-explicit-reminder --reps 3 --trial-timeout 900

# A single surface makes wiring differences easier to isolate.
scripts/e2e.sh --mode context-model --case broadway-retained --surfaces telegram --reps 3
```

The wrapper builds both binaries and records the checkout revision. To test a
deployed artifact without rebuilding it, set `RUSTYKRAB_BIN` and
`RUSTYKRAB_E2E_SOURCE_REVISION` explicitly when invoking the evaluator. The
manifest separately fingerprints the tested binary, evaluator, and compiled
fixture/grader so a newer evaluator is not mistaken for a newer daemon.

`--model` and `--ollama-url` select an already-installed local model. Only plain
loopback HTTP endpoints are accepted, redirects are disabled, and no model is
pulled automatically. Live modes are opt-in and excluded from `all`.
The strategy override applies only to isolated eval harnesses; without it, the
production default remains Legacy. CI explicitly tests the message-tail candidate.
Legacy still fails the deliberately lossy-summary recent-context fixture; the
candidate's green result must not be reported as a changed production default.

## Cases and expectations

| Case | Context perturbation | Model expectation |
| --- | --- | --- |
| `broadway-retained` | Original request, summary, previous answer | Broadway-related browser/search/fetch request; no clock task |
| `broadway-retained-explicit` | Same short history, explicit Broadway follow-up | Control for the ambiguous short follow-up |
| `broadway-noisy` | Add ten substantial synthetic fetch results | Same task despite bulky tool history |
| `broadway-summary-only` | Remove original request; retain summary and answer | Recover the Broadway task from retained context |
| `broadway-explicit-reminder` | Same bulky history, but explicit task reminder | Compare against noisy ambiguous follow-up |
| `broadway-date-correction` | Latest user changes dates to September 18–20 | Task-related action using the new dates |
| `explicit-clock-switch` | User explicitly starts a new clock task | Clock-related action, not stale Broadway continuation |
| `missing-history-control` | No previous messages | Confirm Broadway history is absent; behavior unscored |
| `compaction-loss-control` | Real compaction with a deliberately lossy scripted summary | Latest instruction must survive verbatim despite summary omission; scripted mode only |
| `compaction-generation-limit` | Provider returns a partial summary with native length stop | Refuse replacement, retain the exact original/latest input, and dispatch no actor step; scripted mode only |
| `compaction-opaque-tool-output` | Ferry history dominated by a ~587KB base64 `browser` `pdf` result; scripted summarizer enforces a 65,536-token window at base64 token density | Compaction completes with no summarizer length stop and the follow-up gets a persisted reply; also runnable live with `--case compaction-opaque` (behavior unscored) |
| `tool-availability-contract` | Force real `tools_load` with a missing default-seeded memory tool | Every active/loaded name must appear in the next actor request; absent tool reported unknown; scripted mode only |
| `provider-trim-control` | Small provider window, runner compaction disabled | Require explicit pre-dispatch refusal, zero model requests, and intact durable initial/latest input; scripted mode only |
| `telegram-provider-failure` | Provider fails after a controlled tool timeout | Initial user and partial tool trail remain in SQLite; error reply acknowledges incomplete task |
| `telegram-midrun-followup` | Real webhook injection while the first model request is paused | Correction appears in the second wire request and exactly once in final history |

The missing-history negative control passes when it detects
its intentional context loss; that does not make the loss acceptable.
The compaction case has been promoted: it now requires retention despite a
lossy summary. All ordinary agent requests must also honor the configured tool
seed, not merely expose discovery tools. Ordinary model trials all have to pass; there is no silent
majority threshold. Raw trial classifications support repeated-run analysis.

The model rubric counts actual tool requests, not mentions in the history,
reasoning, or final claims. Generic Google navigation is insufficient. A clock
action in a Broadway trial fails even after an earlier Broadway action. A
blocked answer alone does not satisfy the requested browser attempt. Unknown
action formats are conservatively unverified and require inspection; this is
not an LLM-as-judge or universal semantic grader.
Every emitted call is also checked against the schema offered in that request.
An on-task but unsupported action (for example `browser.google_search`) fails
with `on_task_but_invalid_tool_call`, even if its arguments mention Broadway.
This uses the runner's schema validator, which does not implement full JSON
Schema or the tool's execution-time guards. It is not browser execution proof.
Adjacent real meta-tool results are also joined to the next offered schema set.
This catches a context-construction defect independently of whether the model
actually tries the incorrectly advertised tool. Historical reports are not
assumed to remain accurate after later availability or capability changes.

## Evidence at each boundary

Each run writes a fresh `context-<uuid>/` under `E2E_ARTIFACT_DIR`:

- `manifest.json`: binary/source identity, fixture identity, model configuration,
  live model metadata, and uncertainties established before trials.
- `<case>-<surface>-<rep>.json`: independently read seed and final SQLite rows,
  exact submitted follow-up, pre-provider prompt traces with trace IDs, ordered
  HTTP request bodies and byte hashes, visible model outputs/tool calls,
  terminal usage, context checks, and captured Telegram replies.
  Bounded runtime flags record embedding-init / inbound-memory-write failures;
  full daemon logs are not copied into the evidence bundle.
- `progress.json` and `report.json`: incremental and aggregate outcomes. An
  interrupted run's completed trials remain available.

Inspect each request's roles, message text, tool schemas, and model options,
not just its token count. The pre-provider trace is intentionally not presented
as a wire capture: the adapter can still transform or trim after that point.
The report also records whether configured active tools appeared or were only
discoverable; shared setup now preserves the configured seed on Telegram too.

Tool execution is constrained structurally by replacing the domain registry.
The real discovery tools can only discover/load the reduced registry. Every
browser/search/fetch/code stub returns a controlled error without performing an
external action. Their schemas and descriptions come from the actual production
tool definitions linked from the evaluator checkout, not simplified copies.
The catalog is still reduced: this is not the complete production configuration.
When testing an older daemon, use an evaluator built from matching tool contracts
and compare the recorded schemas; binary revision alone does not prove this match.
The keychain is disabled, stores and browser root are isolated,
and Telegram traffic goes to a local stand-in. Raw reasoning text is not retained
in response evidence. No production prompt logging is enabled by this suite.
For a separately labelled prerequisite control, `RUSTYKRAB_CONTEXT_BROWSER_READY=1`
makes the first browser stub call return neutral readiness and an empty tab list;
subsequent calls still fail. It supplies no website, date, task answer or applied
navigation. The flag is recorded in each manifest and trial.

## Visibility / uncertainty register

| Boundary | Observer | Remaining limitation |
| --- | --- | --- |
| Seed / channel admission | SQLite readback, HTTP/webhook status | Seed is synthetic, not a historical database snapshot |
| Runtime composition | Correlated prompt trace | Captures assembled messages, not every internal mutation hook |
| Ollama submission | Local proxy receives actual adapter request | Does not observe final chat-template rendered tokens inside Ollama |
| Model behavior | Actual outputs and tool calls in live mode | Reduced catalog with production schemas, empty skills/memory, four-iteration cap |
| Durable result / presentation | SQLite readback, local Telegram receipt | Not a real Telegram delivery receipt or booking confirmation |
| Latency / streaming | Per-exchange elapsed time and terminal usage | Proxy buffers responses, so timing is not production-equivalent |

The strongest claim is *context-boundary interoperability*, plus *bounded
local-model behavior* for the specific live trials run. The suite cannot prove
the cause of the original incident, successful browser use, or all contexts and
models. It provides reproducible evidence for the next context-design change.
