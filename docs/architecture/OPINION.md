# Opinion: Correctness and Sensibility

*Second pass, against `main` at `fd1f1e2`. Static review — I read the code,
the schema, the graph and the tests; I did not run the daemon. Where a claim
is a measurement it says so; where it is taste it says that too. The first
pass's confidence labels were its least reliable part (see
[`05-first-pass-outcome.md`](05-first-pass-outcome.md)), so this one leans
harder on counts.*

## Summary judgement

**The structural findings from the first pass are closed, and the code is
meaningfully better for it.** The application layer is a crate. The agent
loop exists once. The schema declares the constraints it relies on. Locks
recover. The estimator has one definition and an honest description of what
it is for.

What remains is narrower and mostly of one kind: **the same sequence written
out at each call site, and configuration read from ambient process state.**
Neither is a bug today. Both are the conditions under which the last round of
bugs formed — a `PendingLinks` drain that existed in one copy of the turn
sequence and not the others, a compaction fix that had to be applied twice.

The thing that most distinguishes this codebase is unchanged and worth
restating: comments explain *why*, and the reasoning survives contact with
review. Repeatedly during this pass, a thing that looked like a defect turned
out to have a comment explaining why it was deliberate — the Slack `''`
sentinel, the non-back-filled version columns, the deliberately-unenforced
provenance columns, the app-surface/chat-surface asymmetry in link delivery.
That is rare and it is what makes 84k lines reviewable at all.

---

## Correctness

Ranked by how much I want each addressed. No live user-facing bug survives
from the first pass.

### 1. The turn sequence remains duplicated across six entry points — **structural, measured**

Follow-up against `0b565fd`: shared interactive setup, Telegram/Slack admission
journaling and reset generations, and partial HTTP/SSE persistence now address
specific lifecycle defects; see [outcome history](05-first-pass-outcome.md#interactive-continuity-follow-up).
The broader duplication finding remains open: the channel transaction is not unified.
Correction against v6 base `3e85f48`: the earlier five-entry-point measurement omitted
`rustykrab-cli/src/task_queue.rs::process_task`, which still persists a turn.
There are six entry points; the task queue was not removed.

Load conversation → snapshot persisted ids → append user message → run with a
heartbeat → `save_turn` → extract the reply → map failure to a user string.
It appears in `process_telegram_message`, `process_slack_message`,
`send_message`, `send_message_stream`, the delegated-task worker in `gateway/src/tasks.rs`,
and the CLI task-queue worker.

This is the highest-value remaining item, and unlike most duplication
findings it has already produced a defect rather than merely threatening to:
the `PendingLinks` drain existed in the Telegram copy and not the Slack one,
so a scheduled job minted a credential link, told the user one was coming,
and dropped it. That was found and fixed by adding the drain to a *second*
copy, which is the failure mode repeating rather than resolving.

`rustykrab-runtime` now exists and is the obvious home. When it moves, one
invariant must move with it explicitly: **chat surfaces drain, app surfaces
do not.** Apollo and WebChat render the credential form from
`GET /api/credential-requests`, so the filed request *is* the delivery
mechanism there; pushing a live capture URL into an SSE stream and a
persisted transcript is exactly what `pending_links` exists to prevent. The
asymmetry looks like a bug if you do not know why, which makes it the most
likely thing to be "fixed" by someone tidying up.

### 2. Ambient configuration — **structural, measured, unchanged**

The 2026-10-02 measurement finds 56 literal-key environment reads in library
src files: 30 tools, 8 providers, 5 gateway, 5 agent, 3 store, 3 dream, and
one each in channels and skills. This includes test code (the three dream
reads are test fixtures) and excludes dynamic keys. The original 48-site
snapshot was against fd1f1e2; these counts are source sites, not call frequency.

Each read is individually defensible. Collectively they mean a library's
behaviour depends on process state its caller did not pass, two tests cannot
configure one component differently, and a test that sets one races every
other test in the binary — which has already happened once.

`OllamaProvider` is the sharpest case: it has a full `OllamaConfig` and a
builder, and the env reads bypass both, so a caller who constructs a provider
explicitly can still be overridden by ambient state. That is the wrong
precedence order regardless of the wider refactor.

`memory` and `runtime` read zero and are the two most portable crates here.
That is not a coincidence.

### 3. Runtime turn assembly still needs direct coverage

The original zero-test finding is retired in the [outcome history](05-first-pass-outcome.md#interactive-continuity-follow-up):
base `0b565fd` already had 14 distillation tests, and this follow-up adds three
channel lifecycle tests. The work-manager follow-up adds a direct `prepare_agent` test of capability
derivation, including denial of dynamically disclosed execution tools. Other
prompt-assembly behaviour still relies on indirect gateway/evaluation coverage.
That narrower gap remains open.

### 4. `memory_links` has no foreign keys — **minor, unchanged**

`chunks` and `extracted_facts` reference `memories`; `memory_links` does not.
Soft deletion bounds the exposure. The asymmetry looks accidental rather than
reasoned, which is the actual complaint — everywhere else in this schema the
deliberate omissions are now commented.

### 5. Semantic search remains a linear scan — **design, unchanged (#328)**

`get_all_chunk_embeddings` loads every embedding for the agent and
cosine-scores in Rust. Cached per agent, invalidated on write, and correct.
Right at thousands of memories, wrong at hundreds of thousands. The lifecycle
machinery exists precisely to bound the working set, so the honest answer is
probably "bounded by design" — and that should be written down, because
otherwise it reads as an oversight.

### 6. Dead code — **minor, unchanged**

`GatewayBackend` (0 implementors), `GatewayTool` and `automation_tools` are
unreachable; `ConsistencyVoter` is exported and referenced nowhere.
`chat_with_choice` became unreachable from the agent when the loop merged
onto the streaming path. Treated in
[`03-dead-code-audit.md`](03-dead-code-audit.md); the recommendation there
stands, and the reason to act is that a trait with no implementors reads as
an extension point when it is an unfinished thought.

---

## Sensibility

### The layering is now right

`core → capability providers → behaviour → runtime → transport → composition`
is a sound spine and it is followed. The exception that motivated the first
pass — the application layer living inside the HTTP crate — is gone, and the
proof is mechanical rather than rhetorical: `cargo tree -p rustykrab-runtime
-e normal | grep -c axum` returns `0`, and the CLI's channel loops call the
runtime directly.

The extraction originally reduced AppState from 26 fields to 10. The
2026-10-02 checkout has 15 transport/composition fields in AppState and 20
in AgentContext after control, delegation and continuity wiring. The split
followed a seam that was already there — `orchestrate` used 16 fields, the
HTTP handlers used 5, and only 3 overlapped.

### Compaction is better than what I reviewed

`predicted_prompt_tokens` anchoring on actual usage, with the heuristic
applied only to the delta, is a real improvement and the right shape. It also
retires an argument the first pass made: the chars-per-token constant is no
longer load-bearing for the compaction threshold, so unifying the five copies
mattered for consistency rather than for accuracy. Worth being explicit that
the finding was right for a weaker reason than stated.

### Abstractions that still do not earn their keep

**`Channel` has one implementor** and it is the in-process one. The four real
channels are concrete types with per-channel loops, per-channel `AppState`
fields and string-matched dispatch. Widen it or delete it; a trait that
advertises pluggability the code does not have costs a reader time to
discover that.

**`HarnessProfile` / `HarnessRouter`** — `research()` is still identical to
`default()` except its name, and the router still holds an
`Arc<dyn ModelProvider>` it never reads. Now that `think` can be controlled
per-call, there is an obvious way to make profiles carry real variance rather
than ±3 integers; that is the version of this abstraction worth keeping.

### Size, where it matters and where it does not

The original second pass measured runner.rs at 6,171 lines. On 2026-10-02
it is 9,214 lines including tests; the unification remains intact. Removing
the duplicate loop was necessary and not sufficient. Compaction, response
classification and tool execution are three coherent modules sharing little
but `&self`, and splitting them is now easier than it was.

main.rs is now 4,769 lines including tests (3,180 in the original review);
the composition root still relies on initialization order.

ollama.rs at 3,210 lines (2026-10-02) is *not* a problem — it manages the model server, and
that capability is what makes per-call window resizing and thinking control
possible at all.

---

## What I would do next, in order

1. **Move the turn sequence into `rustykrab-runtime`**, carrying the
   chat-vs-app drain invariant as an explicit comment. Highest value; already
   has a defect to its name.
2. **`Config::from_env()` per crate**, called once at the composition root.
   Start with `providers`, where the env reads currently beat an explicit
   builder.
3. **Test `rustykrab-runtime`** — it will hold the turn sequence after (1).
4. **Split `runner.rs`** along compaction / classification / execution.
5. **Decide `Channel`** — widen or delete.
6. **Resolve the dead code** per the audit.

Items 1–3 are one arc: the runtime crate becomes the thing it was extracted
to be. 4–6 are hygiene.

## Caveat

Static review again. The measurements are real; the severity ordering is
judgement, and the first pass got severity wrong three times out of eleven.
If any item here matters enough to act on, the cheapest validation is to
implement it — that is what caught the errors last time.

## Context and monitor integration review (2026-10-02)

The integrated planner requires an explicit planning-only worker role;
advertising work_plan on a peer does not reserve the peer for planning.
The real peer-restart scenario exposed that distinction. Phase 4 question
answers now feed the store-backed evaluator. Registry observation does not
refresh last_seen: reads cannot conceal worker health-check staleness.
Monitoring projects existing durable rows; it adds no parallel state ledger.
Authentication/origin, bounded counts, lease faults, normal waits, verification
evidence and event replay are checked against the real router and store.
The scripted daemon suite proves lifecycle wiring; it does not establish
model reasoning quality or implement the separate PR delivery system.


The project handoff follow-up derives from `24dc038`: `plan_workspace` selected
HEAD for every native run, and `Brief` carried no planning revision. The verified
branch history therefore did not supply the next agent's files or project state.
The existing worker boundary now carries durable project context and verified
code ancestry; real git/SQLite tests exercise the loss path. The 128 KiB fail-closed
context bound is an explicit scale limitation. Automatic integration of divergent
verified branches and promotion of unverified mid-run changes remain separate.
