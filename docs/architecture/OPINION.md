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

### 1. The turn sequence is written out six times — **structural, measured**

Load conversation → snapshot persisted ids → append user message → run with a
heartbeat → `save_turn` → extract the reply → map failure to a user string.
It appears in `process_telegram_message`, `process_slack_message`,
`send_message`, `send_message_stream`, and twice in `task_queue.rs`.

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

60 distinct `RUSTYKRAB_*` variables; 46 reads inside library crates rather
than the composition root (25 `tools`, 8 `providers`, 5 `gateway`, 4 `agent`,
3 `store`).

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

### 3. `rustykrab-runtime` has no tests — **my own change, flagged**

746 lines holding the application service layer, 0 tests. It is exercised
indirectly through `gateway` (45) and the e2e suite, and the extraction was
behaviour-preserving, so this is a gap rather than a risk. But the crate now
owns prompt assembly and capability derivation, and it is the natural place
for the turn sequence to land — at which point untested becomes untenable.

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

`AppState` went from 26 fields to 10, with 18 in `AgentContext`. The split
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

`runner.rs` is 6,171 lines — larger than at the first pass, despite the
unification removing 375, because the compaction work added ~580. Removing
the duplicate loop was necessary and not sufficient. Compaction, response
classification and tool execution are three coherent modules sharing little
but `&self`, and splitting them is now easier than it was.

`main.rs` at 3,180 lines still holds a ~1,200-line `main()` whose ordering
constraints are enforced by comments rather than types.

`ollama.rs` at 3,229 is *not* a problem — it manages the model server, and
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
