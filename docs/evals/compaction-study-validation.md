# Compaction ablation and continuity fixes

September 11, 2026 (Pacific). Base `0b565fdbe80095668a402cdc6b5d410efecc867b`
(v5.2.38), with uncommitted `codex/broadway-context-eval` changes. Not deployed.
No production conversations, credentials, browser profiles or configuration changed.

**Outcome:** 99 live compaction/readout trials completed, plus eight separate
live daemon next-action trials. Real deletion, incomplete-summary acceptance and
a contradictory tool-availability report were found. The message-tail hybrid
with fieldwise correction guidance is the best candidate to advance, **not a
proven optimum or production-ready reliability claim**. It repairs the observed
repeated-compaction loss; task-switch output failures and recall/restart coverage
remain unresolved. Legacy remains the default; the candidate is opt-in.

## Primary comparison — complete

Eight synthetic sessions, two repetitions, five conditions: **80 trials**.
Installed local Ollama 0.33.3 / `gemma4:26b`, Q4_K_M, 65,536-token window,
2,048-token generation reservation, temperature 0.1, no fixed seed. Strategy
order rotates. All compactors share a 3,072-estimated-token total-message ceiling;
actual retained sizes differ. The approximately 5,000-native-token source
histories contain heavy tool noise, and compaction is forced rather than
triggered by the production threshold. Full history is a reference, not an
equal-budget compression competitor.

The cases cover ambiguous Broadway follow-up, changed Broadway dates, explicit
task switch to Rust, Hyannis bus/time/budget constraints, older FBAR fixture
identifiers, a draft-save timeout with unknown effect, return after a topic
detour, and two compactions with successive flight-date corrections.

The real production Rust compactor and Ollama adapter run. A fixed prompt asks
the model for an inert `session_checkpoint`: intent, direction, relevant facts,
proposed next action and verified completed work. **No domain tool executes.**
The primary baseline uses the legacy summary prompt plus this worktree's
first/latest-anchor and input-budget fixes; it is not unmodified released code.

| Condition | Raw complete readout | Schema-valid readouts | Required facts present in valid readouts | Median actual probe input tokens |
| --- | ---: | ---: | ---: | ---: |
| Legacy summary | 8/16 | 15/16 | 13/15 | 683.5 |
| Structured summary | 11/16 | 14/16 | 11/14 | 768.5 |
| Structured + whole-user-turn tail | 11/16 | 15/16 | 12/15 | 756.5 |
| Extractive first/latest + whole-turn tail | 3/16 | 14/16 | 3/14 | 631.5 |
| Full-history reference | 13/16 | 14/16 | 14/14 | 5,153 |

These are **pilot readout counts, not production reliability rates**. The fact
column explicitly excludes invalid readouts; those still fail the overall probe.
The structured prompt's higher combined score does not establish better fact
retention: legacy retained more required facts among valid readouts. Nor do
the tests establish statistical significance or an optimal token allocation.

## Causal findings

1. **Repeated compression really loses older constraints.** All four primary
   compression conditions fail both repeated-correction readouts. Inspecting
   the actual histories shows that `nonstop` and `carry-on` disappear, not merely
   that the answer omits them. In legacy and whole-turn-tail trials, the first
   summary retains them and the second drops them while preserving new dates.
   This is genuine context loss. Full history retains and uses both constraints.
2. **Incomplete generations were accepted as summaries.** Three primary summary
   responses end with native `done_reason=length`. One structured flight summary
   stops mid-field and loses the older preferences before the second reduction.
   A subsequently implemented guard refuses such summaries and leaves history
   intact. A naturally terminated summary can still omit facts; this guard is
   not a semantic correctness guarantee.
3. **Whole-turn retention can spend its allowance badly.** One large browser
   exchange crowds out short nearby dialogue. The extractive condition often
   keeps only first/latest inputs; it proposes recall but cannot directly name
   needed older facts. Recall is not executed in this probe, so this is not a
   claim that the recall implementation itself failed.
4. **Context presence and model use differ.** In structured Broadway repetition 1,
   Hamilton and Wicked remain in the compacted context but are absent from the
   checkpoint. In whole-turn-tail FBAR repetition 1, the address fact remains
   in context but is omitted from the checkpoint. These are readout/use failures,
   not compactor deletion.
5. **Some summaries change direction.** Legacy unknown-effect repetition 1
   changes checking external draft state into searching the local environment.
   The unknown outcome and draft ID survive, but the next direction changes.

## Rubric audit

Raw scores are preserved. A literal `explain|compar` criterion incorrectly
rejects “technical explanation of the differences” in the task-switch case.
The source rubric now accepts the `explan` stem and has a regression test;
the independent audit reports that narrow morphology repair beside, never over,
the original score. It also reports wire integrity, native stop reasons and
literal fact presence in each compaction round.

Other field-local checks can undercount semantic continuity: a Hyannis intent
field says “evaluate bus options” while the direction explicitly names Boston
and Hyannis; a correct missing-item response omits the word “FBAR” from its
intent field. Those are not evidence of memory loss. Conversely, keyword passes
do not prove general factuality or that a proposed action is the best one.

The full-history reference fails to emit a checkpoint in both explicit-switch
repetitions. Those captures contain no visible text/tool call despite native
generation and normal `stop`; the hidden reasoning is not retained. This is
an unscorable semantic/protocol outcome, not proof that the task was forgotten.

## Exploratory retention follow-up

The separately recorded `structured-message-tail` policy keeps small recent
dialogue messages within a three-user-turn window and the same total budget.
Oversized tool call/result groups are archived whole, not split; nearby dialogue
can survive on both sides. This trades contiguous raw evidence for verbatim
context plus summary/recall. It was designed after inspecting the primary
failures, so it is exploratory rather than a held-out confirmation.

The 16-trial follow-up is complete: **11/16 raw combined passes**, **15/16 valid
readouts**, and **14/15 complete fact readouts**. One morphology-only correction
raises the combined count to 12/16; raw scores remain unchanged. Median native
probe input is **813.5 tokens**, versus 683.5 for legacy and 5,153 for full history.
This suggests a useful retention/cost tradeoff, not a statistically established
winner. Its two FBAR intent failures are field-local keyword mismatches despite
correct FBAR direction, file, identifier and missing-item facts elsewhere in the
checkpoint. Its other failures are one missing task-switch checkpoint and one
repeated-correction readout omitting older preferences.

Crucially, the repeated-case contexts retain `nonstop` and `carry-on` **through
both compactions in both repetitions**. One readout uses them; the other does
not. This repairs the observed deletion but does not fully repair model use.
The follow-up has **34/34** independently verified wire exchanges and no
generation-limited summaries. Both comparison executables predate the generation-limit guard,
keeping summary acceptance constant. Compiled-source and executable hashes
identify those builds separately from later hardening.

### Focused fieldwise-correction v2 follow-up

The next revision explicitly says that same-task corrections update only the
fields changed: older independent requirements remain unless withdrawn or
contradicted. Explicit task switches do not inherit unrelated constraints.
This changes the summary and continuation prompts, plus the generic daemon
framing. It includes the generation-limit guard and corrected morphology rubric.
It is a focused exploratory follow-up, **not another 16-trial comparable arm**.
Both repeated-correction trials pass. Their actual summaries and readouts retain
nonstop, carry-on and afternoon departure preferences, the latest December 3–6
dates, and the no-booking boundary through two compactions. Median native probe
input is 1,291 tokens for this specific case, not an eight-case median.
The explicit-switch control still emits no checkpoint despite a normal native
`stop` (859 generated tokens). Its retained summary correctly switches to Rust
Mutex/RwLock, marks Broadway superseded and says not to browse; semantic readout
cannot be scored. **Eight of eight wire exchanges verify independently**, with
no length-stopped summaries. Two targeted successes do not establish a general
winner, and the switch failure remains visible.

The implementation keeps Legacy as the default. `compaction_strategy =
"structured-message-tail"` selects the revised candidate in `harness.toml`;
task routing preserves that choice. No production configuration is changed.

## Separate daemon next-action checks

These use the real daemon and Telegram path, local model, full short synthetic
Broadway history and production tool schemas, but **inert domain tools**. They
have a four-iteration cap and no compaction. They are not additional compaction
arms or real merchant journeys. The daemon uses its own generation reservation
(4,096), unlike the direct study's 2,048-token readout probe.

- With tools always unavailable, both ambiguous repetitions attempt browser
  startup/status but no task-specific URL/search. Both explicit reminders issue
  a Broadway search; one also calls a `memory_search` tool that was not offered.
  All four retain the objective and latest input on every request. The original
  aggregate is one pass, three failures; inspect the causes rather than pooling
  them into a reliability rate.
- A neutral-readiness control returns an empty tab list on the first browser
  call, with no site/date/task hints; subsequent calls still fail. The ambiguous
  trial searches `current+time+in+NYC` and requests a world-clock page. The
  explicit reminder requests Broadway search/navigation. This directly shows
  misinterpretation with full context **in this synthetic run**, not an exact
  replay of the original incident. The old rubric labels the ambiguous failure
  `no_verified_on_task_action`; a subsequent audited repair recognizes encoded
  clock queries and world-clock URLs. The raw verdict stays failed.

### A real tool-context contradiction, not just a model mistake

Inspection of the invalid memory call found that `tools_load.active` listed
`memory_search` and other default-seeded names absent from the registered,
permitted schema set, while the soul unconditionally said to use memory lookup.
The actual next request offered seven tools, none named `memory_search`. The
model was receiving contradictory guidance. This defect invalidates attributing
that call solely to model hallucination.

The active report now filters registered, available and permitted tools; generic
memory instructions are conditional on availability, and the runtime appends
an authoritative offered-schema rule even for existing custom soul files.
The user-owned files are not rewritten. New deterministic HTTP and Telegram
tests execute the real `tools_load`, require missing memory to be `unknown`,
and join every active/loaded name to the next request's schemas. Both pass.
Unit negative controls reject phantom names and a missing new result even when
an old load report exists. Dynamic availability changes without a registry
version bump remain a separate schema-cache risk, not a claimed fix.

The post-fix neutral-readiness pair passes **2/2**. Both ambiguous and explicit
follow-ups request Broadway navigation/fetch, with no clock call or unoffered
tool. The ambiguous trial executes `tools_load` and its report matches the next
schemas; the explicit trial uses already offered browser tools without loading.
Both retain full context and trigger zero compactions. Changing both guidance
and the report is a combined intervention, not a one-factor ablation. Sampling
variation remains possible; one pair does not establish a reliable behavioral fix.

## Fixes and independent verification

- Provider budgets use the actual active tool schemas. Oversized Ollama inputs
  are refused unchanged instead of dropping older history. Pairing validation
  rejects orphan/missing tool results. The real daemon's HTTP and Telegram
  budget fixtures both retain the original/latest inputs with zero dispatches.
- Summary inputs are losslessly split at UTF-8 boundaries, mandatory anchors
  cannot be clipped to fit, and cancelled/failed summarization cannot leave a
  temporary prompt in the live conversation. Generation-limit rejection is
  independently exercised through both HTTP and Telegram: one partial-summary
  response, no actor step, original history intact in SQLite.
- Telegram and Slack journal inbound UUIDs before admission and share owned
  completion/cancellation and reset-generation handling. The Telegram failure
  and mid-run-injection fixtures independently join original journal UUID/data
  to final stored messages. Reset race unit tests do not prove all transport
  timing or crash interleavings.
- HTTP/SSE persist initial and partial-error histories. The scripted daemon
  suite passes 23 scenarios with six pre-existing expected failures, no unexpected
  passes. Same-conversation concurrent HTTP serialization remains unimplemented.
- The explicit message-tail policy passes **26/26 deterministic daemon/context
  trials**, including the noisy case that fails under the legacy policy, both
  generation-limit guards and both tool-availability joins. CI explicitly sets
  the candidate policy rather than implicitly changing the production default.
- Real Chrome verifies exact-origin/object-bound credential fill using a synthetic
  secret. Four invalid-target cases cause zero vault reads; a valid password field
  receives one value without focus leakage, output/snapshot disclosure or submit.
  Mutation after target preparation is rechecked. This is not merchant login proof.

The final availability-stage workspace suite passes **1,055 tests**, 30 normally
ignored. Workspace/all-target clippy, formatting, diff whitespace and architecture
generation/checks pass on the documented offline/no-default-features path. The
final deterministic context suite passes **26/26** cases/surfaces; the final
scripted HTTP/SSE suite also passes with its six expected failures unchanged.

## Evidence and limits

- [Primary raw captures](evidence/compaction-primary-raw.tar.gz), SHA-256
  `3407120d2dd811d1b5e953a875c26c99ee752fe85322e077fa6a13f170b24cab`.
- [Independent primary audit](evidence/compaction-primary-audit.json).
- [Message-tail raw captures](evidence/compaction-message-tail-raw.tar.gz), SHA-256
  `1521da78dc83b74a069d89a5188ec2e67bcc8fb7b315262bc22df4e5d7f2c1fe`;
  [independent follow-up audit](evidence/compaction-message-tail-audit.json).
- [Focused fieldwise raw captures](evidence/compaction-fieldwise-raw.tar.gz), SHA-256
  `6a3f40d66127b341a1d81a6320fb4e841c4f27afa5f34766fcd7731a4687dc4a`;
  [independent focused audit](evidence/compaction-fieldwise-audit.json).
- [Daemon/Chrome/checks raw evidence](evidence/runtime-validation-raw.tar.gz),
  SHA-256 `82cd26c6fe96b391902e74742048e9ce6f587725e202933875b6116071d1213c`.
  It includes pre-fix unavailable (`context-c610c9b8…`), pre-fix ready
  (`context-8d465803…`), post-fix ready (`context-5b9b952d…`), final deterministic
  (`context-dddd5a5a…`), and earlier failing Legacy (`context-37106e02…`) runs,
  the Chrome boundary result, scripted-suite report and build/test logs.
  A separate hash/byte-count/JSON equality audit passes **178/178** daemon wire
  exchanges across these five captures, including failed trials. No full raw
  daemon log is bundled. The final deterministic run alone has 68 exchanges;
  the three live groups have 20, 10 and 10 respectively.
- The audit reparses and hashes **134/134 primary wire exchanges** successfully.
  The archive contains the model/config/source-hash manifest, fixtures, complete
  pre/post-compaction contexts, visible responses and untouched raw grades.
  No hidden reasoning text or real credentials are retained.
- [Study design and reproduction](compaction-study-contract.md).

The verify skill shaped this work by separating independent wire/SQLite/browser
observations from model claims, preserving failed trials and keeping synthetic
substitutes explicit. Green unit tests alone are not the evidence for model
quality, merchant success or historical causality.

## Recommendation and tradeoffs

Advance `structured-message-tail` to a held-out evaluation and opt-in canary,
not a silent default change. It combines verbatim recent dialogue, an explicit
intent/direction/constraint summary and archived bulky tool observations. The
fieldwise rule preserves old independent requirements through a date correction.
The alternative of simply making the summary more structured still deleted
facts in this study; schema shape is not a retention guarantee.

The hybrid consumes more prompt space than minimal summaries and skips some raw
tool evidence. Three recent user turns cannot preserve all older details; archive
retrieval must actually happen. Retained old text can also contain superseded
values, so correction provenance matters. Refusing over-budget or length-stopped
summaries preserves history but can end a run that previously continued with
damaged context. More budget or an explicit recovery path may be required.

Before changing the default, add held-out sessions with many successive
compactions, older-fact recall after a process restart, explicit task switches
under both natural-answer and structured-readout protocols, and real browser
journeys. Measure retention, task selection, tool validity and verified external
outcome separately. Do not pool the exploratory v2 results into the original
comparison or treat 2/2 targeted repairs as a reliability estimate.

| Remaining gap | Consequence / next evidence needed |
| --- | --- |
| Original Broadway/Hyannis/FBAR request bytes unavailable | No exact historical reconstruction; retain native request traces for future incidents |
| Synthetic sessions, reduced schema, inert action probe | Does not prove merchant success or production next-action reliability; run separate daemon action and browser journeys |
| Two repetitions, shared local model, fixed small fixtures | No reliability/latency estimate or broad optimum; extend to held-out sessions and many compaction generations |
| Recall not executed by checkpoint probe | Add archive retrieval plus resumed-task E2E, including process restart |
| Missing checkpoint on explicit task switch | Compare visible-answer/structured-readout protocols separately; do not count absent output as semantic amnesia |
| Static tool report repaired; dynamic schema availability not invalidated | Add availability-transition tests and a shared schema-version contract |
| Heuristic budgeting and unobserved rendered template | Native prompt counts help but do not prove all models/images fit |
| Missing user/assistant role provenance in old stored rows | New runner prompts have corrected roles; old synthetic user rows are not automatically rewritten |
| Channel transport acknowledgement gap and async memory hooks | Admission journal is not exactly-once execution or fully durable working memory |
| Credential-origin discontinuity | Old keys remain stored but cannot auto-fill; re-enrollment required. Cross-origin SSO frames are rejected; real framework/login compatibility needs testing |
| Reusable payment vault undecided | Card enrollment/reuse remains off; arbitrary evaluate/screenshots/page reflection are not isolated secret channels |

Architecture maintained: core, agent, providers, runtime, CLI, gateway, store,
tools, skills and e2e crate write-ups; system overview, data model, extension seams,
opinion and outcome history. A new inbound-journal table/index and direct
agent/provider evaluator dependencies are documented. No commit or deployment
has been performed.
