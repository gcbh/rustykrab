# Context evaluation: initial verification

## Evidence availability after recovery

The temporary worktree and raw evidence directories were discovered missing
after these runs. The source was recovered from this task's recorded patches
into `/Users/geoff/projects/rustycrab-worktrees/broadway-context-eval`.
The run results below remain recorded in the task transcript, but the referenced
raw JSON bundles are no longer available locally. They must be regenerated;
do not present the old bundle paths as working links or a new verification of
the recovered checkout. The cause of the directory disappearance is unknown.

## Scope and identity

Partially verified: the new suite observes the real daemon's context-production
boundaries; model behavior and browser execution are separate claims.

Tested artifact: RustyKrab 5.2.38, source
`0b565fdbe80095668a402cdc6b5d410efecc867b`, packaged macOS executable SHA-256
`44b39c6ffa5582dd50fab88f5f7e9c760792b1be1d736d22d900bcd6f162fb32`.
The evaluator is an uncommitted worktree based on that revision. Each manifest
records its distinct executable and fixture/grader hashes, so developmental
runs must not be pooled as repetitions of an unchanged experiment.

Real dependencies: deployed daemon executable, SQLite, HTTP ingress, Telegram
webhook parsing/turn assembly, agent runner, compaction, Ollama adapter. Live
trials additionally use local Ollama 0.33.3 and `gemma4:26b` (25.8B, Q4_K_M).
Substitutes: synthetic conversations, empty memory/skills, reduced tool catalog
with production schemas but inert execution, local Telegram Bot API receiver.
Production data, credentials, browser profiles, and configuration were unchanged.
The packaged daemon's isolated Telegram fixture also logged failed embedding
initialization and a failed inbound working-memory write (the model file was
unavailable with fixture egress blocked). Conversation SQLite loading still
worked. This is an observed fixture limitation, not a verified production memory
path; the evaluator now records bounded flags for these failures.

## Boundary evidence

| Boundary | Observation | Result / limitation |
| --- | --- | --- |
| Initial state | Direct SQLite seed readback | Exact synthetic message rows matched |
| Admission | Real HTTP message request or Telegram webhook | Accepted and associated with isolated conversation |
| Context assembly | Native prompt trace with trace ID | Assembled messages and offered tools retained as evidence |
| Provider submission | Proxy's raw request bytes and SHA-256 | Post-conversion/post-trimming body, not inferred from the store |
| Trace-to-wire join | Ordered calls in a single isolated turn | Counts and streaming modes must match; message survival reported per request |
| Model output | Captured visible responses and tool calls | Deterministic responses are explicitly unscored for model quality |
| Persistence / presentation | Final SQLite readback and captured channel reply | New assistant output observed; Telegram delivery is a local substitute |

The final deterministic suite passed **18/18 detector/contract trials** (nine
cases on two surfaces). This includes deliberately unhealthy controls; it does
not mean all 18 contexts satisfied continuity. An independent Ruby audit of the
production-schema run checked 51 wire exchanges: request hashes matched raw
bodies, parsed JSON matched those bodies, and every trace join matched.

Final deterministic evidence bundle:
`context-92667f86-b46b-4693-9057-d265530e02a0` under the local
`/private/tmp/rustykrab-context-evidence` root.

## Live long-history result

On 2026-09-10, one `broadway-noisy` Telegram trial **failed overall** at its
900-second deadline. Evidence bundle:
`context-99a71229-cc2b-4c88-8c59-633a07a370de` under the same local root.
An independent audit verified both captured request hashes, parsed bodies, and
trace joins.

| Measurement | Observed result |
| --- | --- |
| Captured provider requests | Two; 25 and 27 messages, 173,620 and 174,702 bytes |
| Original task, prior answer, latest instruction | Retained in both outgoing bodies; no compaction |
| Completed first response | HTTP 200 after 370,592 ms; 37,542 prompt tokens and 1,143 output tokens reported |
| Emitted action | `web_fetch` of a Google query for Broadway availability September 14–16, 2026 |
| Task alignment | Pass for the observed action; no clock action observed |
| Offered-schema conformance | Fail: only `tools_list` and `tools_load` were offered on that request |
| Second response | Incomplete when the harness deadline expired; no terminal usage available |
| Durable / visible completion | No captured Telegram reply; SQLite still contained only the 23 seeded messages |

Calling a non-offered tool is a failure of this eval's interface contract, not
proof that registry dispatch could not find that tool: it was registered as an
inert stub, and the next prompt contained its controlled error. Historical tool
calls can therefore refer to tools absent from the current offered schema set.
That configuration/model interaction deserves investigation rather than an
automatic model-only attribution.

The timeout cancelled the isolated run before normal completion. Its unchanged
SQLite rows are an observation at cancellation, not proof about the daemon's
ordinary error-persistence path. The fixture daemon and evaluator exited;
production remained running. The long trial predates only the final aggregate
invariant correction and bounded environment-warning fields; its individual
request checks already show continuity on both requests. Its manifest identifies
that earlier grader exactly. Do not pool it with the earlier simplified-schema
smokes or describe it as a reliability estimate.

## Findings the evaluation makes visible

- **Latest-instruction loss is possible during compaction.** With a deliberately
  lossy summarizer, both real channel paths emitted subsequent requests without
  the latest date correction. This demonstrates the missing retention safeguard,
  not that the historical Broadway turn used this particular summary.
- **Provider trimming is another loss boundary.** Under the forced 4,096-token
  control, original history disappeared after the runner's prompt trace. Loading
  the larger tool schemas could also remove the remaining objective on a later
  request while the latest, ambiguous follow-up survived.
- **HTTP and Telegram have different initial tool context.** HTTP honored the
  configured domain-tool seed. Telegram initially offered only discovery tools;
  loading the browser required extra model steps. The suite records this wiring
  difference; it does not repair it.
- **Fixture fidelity matters.** Early simplified schemas allowed unsupported
  browser action names. Those smoke results are not final model-quality evidence.
  The shipped fixture now imports actual tool schemas/descriptions and separately
  grades task alignment and argument validity against each offered schema.

## Build and regression checks

Workspace tests and workspace/all-target Clippy passed with
`--no-default-features --offline`. Embeddings/ONNX and ignored live-provider
tests were not exercised. Formatting, architecture metrics, and diff whitespace
checks passed. The evaluator's 53 unit tests include negative controls for clock
drift, unsupported browser actions, missing instructions, and later-request loss.
CI is configured to run the deterministic suite and upload its artifacts;
hosted CI has not run for this uncommitted change.

Architecture documents updated: `crates/rustykrab-e2e/ARCHITECTURE.md` and
`docs/architecture/00-system-overview.md`. No production execution path changed.

## Remaining uncertainty and shortest next checks

| Uncertainty | Alternate explanation / impact | Evidence needed |
| --- | --- | --- |
| Original incident request unavailable | Historical failure may differ from this reconstruction | Exact retained original request, if available; otherwise do not claim replay |
| Ollama's final rendered tokens unobserved | Chat-template/tokenizer behavior can differ from submitted JSON | Provider-side rendered-input/token instrumentation |
| Empty memory, reduced catalog, bounded run | Recall, skills, tool competition, and longer recovery could change behavior | Controlled full-configuration ablations with synthetic memory |
| Packaged embedding initialization failed in isolation | Inbound working-memory persistence was unavailable in that fixture | Pre-provision the isolated embedding cache and assert memory write/read separately |
| Inert domain tools | Valid calls need not execute successfully | Separate real-browser journey suite |
| Small live sample | One success/failure is not a reliability estimate | Repeated trials across noisy history, explicit reminders, summary-only, corrected dates, and true task switches |
| Seeded history rather than complete earlier runs | Previous failure persistence and queued-message admission remain untested | Multi-turn failure/restart/queued-follow-up scenarios |
| Buffered proxy | Observed latency is not production streaming latency | Streaming-preserving observation before performance conclusions |

See [the run guide](context-continuity.md) for commands, grading rules, and the
machine-readable evidence layout. These are regression instruments, not fixes
for the production defects they expose.
