# Dead Code Audit

Five things the review flagged as unreferenced. Each was checked against
`origin/main` rather than a working tree, because two of them turned out not
to be what they looked like.

**Second pass:** re-measured against `fd1f1e2`. Nothing here has changed —
`GatewayBackend` still has zero implementors, `ConsistencyVoter` is still
exported and referenced nowhere, and `HarnessRouter::_classifier` is still
held and never read. `TurnRecorder` is still absent from the repository.

**Verdict summary**

| Item | Lines | Actually dead? | Recommendation |
|---|---|---|---|
| `TurnRecorder` (`core/src/memory.rs`) | 35 | **No — not in the repo at all** | Nothing to do |
| `GatewayBackend` + `GatewayTool` + `automation_tools` | ~145 | Yes | Delete, or wire — it does something useful |
| `ConsistencyVoter` (`agent/src/voting.rs`) | 242 | Yes | Keep the file, stop exporting it, fix the doc |
| `HarnessRouter::_classifier` | 1 field | Yes | Delete the field |
| `ModelProvider::chat_with_choice` | ~10 | **Newly** unreferenced | Keep — public trait API |

---

## 1. `TurnRecorder` — not dead, and not in the repository

`crates/rustykrab-core/src/memory.rs` defines a `TurnRecorder` trait with no
implementors and no callers. It is also **untracked**, and `core/src/lib.rs`
on `main` has no `pub mod memory;` declaration — so it is not compiled, not
part of the crate, and not part of any commit.

It is uncommitted work in progress in a working tree, not repository dead
code. The review listed it as dead; that was wrong, and it is the reason
this audit checks `origin/main` rather than whatever is on disk.

**Recommendation:** nothing. It belongs to whoever is writing it.

---

## 2. `GatewayBackend` / `GatewayTool` / `automation_tools` — dead, but the tool is not pointless

- `tools/src/gateway_backend.rs` — the trait. **Zero implementors.**
- `tools/src/gateway.rs` — `GatewayTool`, which implements `Tool` over it.
- `tools/src/lib.rs` — `automation_tools(cron_backend, gateway_backend)`, the
  only constructor of `GatewayTool`. **Zero callers**; the CLI builds
  `CronTool::new(...)` directly and never calls this factory.

So the whole chain is unreachable: nothing implements the trait, and the only
function that would construct the tool is never called.

Worth noting before deleting: `GatewayTool` gives the agent a way to act on
its own gateway. That is a coherent capability — the kind of thing a
self-managing daemon wants — and the reason it is dead looks like drift
rather than a decision. `automation_tools` bundles cron and gateway together,
the CLI wanted only cron, and it reached past the factory instead of
extending it.

**Recommendation:** decide the capability question, then act. If the agent
should be able to operate its own gateway, implement the backend in the CLI
next to `CronAdapter` and call `automation_tools`. If not, delete all three.
Leaving a trait with no implementors is the one option that helps nobody: it
reads as an extension point when it is an unfinished thought.

---

## 3. `ConsistencyVoter` — dead, and the technique is worth more than the code

242 lines implementing self-consistency sampling: run a query N times
(`consistency_samples`, default 3), have the model pick the consensus, return
it with a confidence score. `VotingStrategy::UnanimousOrEscalate` halves
confidence on disagreement so a caller can escalate to a human.

The config end is fully built — `OrchestrationConfig::consistency_samples`,
an `ORCHESTRATION_CONSISTENCY_SAMPLES` env override, `orchestration.toml`
support. The call end never arrived. Nothing constructs a voter.

### What it is for

Self-consistency (Wang et al., 2022) works when three things hold: the answer
is discrete and checkable, the model is right more often than wrong, and
errors are uncorrelated across samples. It is actively harmful when the model
is *systematically* wrong — every sample shares the error and the result is a
confidently unanimous wrong answer, which is worse than one uncertain answer.

Its placement in `OrchestrationConfig`, beside `max_recursion_depth` and
`RecursiveCall`, suggests the intent was to vote on RLM sub-query answers
before composing them — a wrong sub-answer silently poisons everything
downstream of it.

### Two problems with the implementation

**It cannot do what its doc says.** The module comment claims it runs samples
"with temperature variation". `ModelProvider::chat` takes no temperature, and
temperature is a provider-construction setting, so all N samples run at
identical settings. The only variation is sampling nondeterminism. On a local
model pinned to a low temperature that can yield three near-identical samples
and a confidence of 1.0 that means nothing.

**The agreement metric measures the wrong thing.** Agreement is stop-word-
filtered bag-of-words overlap above 0.5 against the consensus text. That is
vocabulary reuse, not agreement: two responses recommending opposite actions
in similar words score as agreeing.

### Where it would actually pay

`SignalClass::Judge` in the outcome pipeline. When a turn has no verifiable
outcome and no explicit user feedback, a model is asked whether it went well,
and that verdict feeds `rustykrab-dream`, where `Readiness::permits_mutation`
gates self-improvement on evidence quality. A judge verdict is exactly the
right shape: discrete (`success`/`failure`/`ambiguous`), high-stakes, and
unverifiable by other means. "Three of three judges agreed" is materially
better evidence than one sample, and the readiness machinery already has
somewhere to put that distinction.

But applied there, most of this file disappears. For a categorical verdict
you do not need an LLM consensus call or a fuzzy overlap metric — you sample
three times and count. The two weakest parts, roughly 60% of the file, are
the parts built for free-text answers.

**Recommendation:** keep the file — the sampling scaffolding (semaphore,
per-sample timeout, partial-failure handling) is correct and would otherwise
be rewritten. Fix the doc comment so it stops advertising temperature
variation it cannot perform. **Stop exporting it from `lib.rs`**: a public
export implies a supported entry point, and there isn't one. Treat "wire
self-consistency into the judge path" as its own scoped work.

---

## 4. `HarnessRouter::_classifier` — delete the field

```rust
/// A model provider kept for potential future use (e.g. RLM context
/// management). Not used for profile classification.
_classifier: Arc<dyn ModelProvider>,
```

Held, never read. `HarnessRouter::new` requires an `Arc<dyn ModelProvider>`
to construct a router that does keyword matching.

The cost is not the pointer. It is that `HarnessRouter::new(provider)` reads
as "the router consults a model" to anyone skimming, and the doc comment two
lines up says the opposite. In a codebase whose comments are otherwise
reliable, a misleading signature is expensive.

**Recommendation:** delete the field and the constructor parameter. If an LLM
classifier is wanted later, add it back with a call site attached.

Related, and worth raising separately: the profiles this router selects
between differ by at most three integers, and `HarnessProfile::research()` is
identical to `default()` except its name. Whether the routing machinery earns
its keep is a design question, not a dead-code question — see
`02-extension-seams.md`.

---

## 5. `ModelProvider::chat_with_choice` — newly unreferenced, keep it

Not dead when the review was written. It became unreferenced outside
`rustykrab-providers` when the two agent loops were merged: the surviving
loop streams, so every run now calls `chat_stream_with_choice`.

Anthropic's implementation is correct and matches its streaming counterpart.
The method is public trait API and a provider may reasonably implement it.

**Recommendation:** keep, and do not add callers. Flagged here so it is not
rediscovered later as inexplicable dead weight. The related hazard —
`chat_stream_with_choice`'s default chain silently discards the tool-choice
constraint, so a provider implementing only the non-streaming variant loses
the iteration-0 guard — is documented on the loop-unification change.

---

## Method

Each item was checked with `impl <Trait> for` and call-site greps across the
workspace at `origin/main`, excluding test doubles where noted. Two items
were not what the initial pass suggested: `TurnRecorder` is uncommitted work
that was never in the repository, and `chat_with_choice` became dead during
this review rather than before it. Both are reminders that "unreferenced" is
a property of a snapshot, not of a symbol.
