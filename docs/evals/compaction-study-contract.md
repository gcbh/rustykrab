# Compaction fixes and controlled ablation contract

Base: `0b565fdbe80095668a402cdc6b5d410efecc867b` (v5.2.38), isolated
`codex/broadway-context-eval` worktree, with the recorded uncommitted fixes.
The study will record the exact source hashes, model identity/configuration,
inputs, compacted messages, provider requests and bounded visible responses.

## Claim and comparison

Given synthetic chronological sessions containing goals, corrections, failed
actions, task switches, older necessary facts and irrelevant tool noise, the
production compactor must produce a bounded handoff without modifying the
user's latest input or promoting failed actions into completed work. A real
local model must then preserve session intent, current direction, and the
facts required for its next action. No domain tool is executed in this study.

Compare Legacy (pre-study progress prompt, first/latest anchors), Structured
(change the prompt only), StructuredTail (also retain a bounded suffix of
complete user turns), and Extractive (no generated summary). Uncompressed
history is a reference, not a compression competitor. Use identical model,
generation settings, source histories and total compacted-budget ceilings.
Record actual retained sizes: a shared ceiling is not equal token usage.
Rotate strategy order by case/repetition; do not infer latency wins from a
single warmed-cache run. Score fact retention separately from chosen action.
This deliberately forces compaction on roughly 5,000-native-token synthetic
histories; it does not test the production threshold or fill the 65,536-token
model window. Legacy means the earlier summary prompt with this worktree's
anchor/budget fixes, not a byte-identical replay of released v5.2.38.
Include at least two repetitions; this is a bounded pilot, not a production
reliability estimate. Report every failure and sensitivity to corrections,
explicit switches, ambiguity, older detail, and repeated compression.

An exploratory follow-up, added after the primary comparison started, uses
`RUSTYKRAB_COMPACTION_STUDY_ARM=message-tail`. It retains recent message groups
rather than whole user turns, keeping assistant tool calls and their results
together. Oversized tool exchanges can be skipped and archived whole while
nearby short dialogue is retained, within the same budget and three-user-turn
limit. This tests the observed oversized-browser-turn failure: one large result
should not exclude useful dialogue immediately before or after it. It trades
contiguous raw tool evidence for verbatim dialogue plus summarized/archived
observations. Its run is separate and must not be portrayed as pre-registered.

The probe requests exactly one inert `session_checkpoint` call. Score readout
protocol success separately from semantic success: absent/malformed calls are
end-to-end probe failures but unscorable semantic outputs. Generation-limit
failures are not automatically context failures. Keep raw keyword scores and
publish any human/model-assisted semantic adjudication separately with concrete
output evidence; for example, `explanation` can miss a literal `explain` match.
Compare a missing readout fact with the actual retained context before assigning
causality. Archive existence alone does not mean recall was executed.

The generation-limit rejection guard was added after the primary study exposed
an accepted partial summary. Both comparison executables were built before that
guard, to keep summary acceptance unchanged across arms. Their compiled-source
hashes identify this boundary. The guard has separate regression evidence; do
not relabel the ablation as a live test of the subsequently hardened build.

A focused v2 follow-up changes the message-tail summary and continuation prompts
to treat same-task corrections as field-level updates, preserving independent
requirements unless withdrawn. It runs two repeated-correction trials and one
explicit-switch control. It also includes the generation guard and morphology
rubric repair: these three exploratory trials are not pooled with the 16-trial
retention arm. The generic runtime framing is tested separately through the
daemon, first with unavailable tools and then with neutral browser readiness.
Those short daemon histories do not trigger compaction. Pre/post tool-availability
prompt and meta-tool fixes are recorded separately; a passing schema contract is
not automatically a passing task-selection result.

Run the primary comparison with `scripts/e2e.sh --mode compaction-study --reps 2`.
It uses already installed local models only. `--case SUBSTRING` selects a
fixture; `--trial-timeout` bounds each trial. This mode does not require the
daemon even though the launcher builds it for source-version consistency.

## Safety and independent evidence

All sessions and identifiers are synthetic. Use installed local Ollama only;
no model downloads, merchant traffic, real credentials, payments, filing or
bookings. Save source histories before running; capture actual post-adapter
wire requests and correlate them with outputs and independent graders. Raw
hidden reasoning is not retained. Retain evidence in the worktree, not /tmp.

## Initial uncertainty register

| Gap | Effect on claim | Evidence to close it |
| --- | --- | --- |
| Original incident wire contexts unavailable | Cannot retrospectively prove causes in Broadway/Hyannis/FBAR | Incident-native pre/post-adapter traces on future runs |
| Synthetic sessions and inert domain tools | Measures context/action selection, not merchant success | Separate browser/provider/production E2E tests |
| Heuristic token counts | Fit under estimate does not prove fit under native tokenizer | Captured actual model prompt counts and explicit overflow outcomes |
| Model sampling and cache order | Small samples are not reliability or latency rates | Paired repetitions, rotated order, publish per-cell results |
| Summary factuality | A well-shaped summary can still be wrong | Independent fixture fact/correction/forbidden-effect checks and action probes |
| Recall archive not automatically consulted | Archived detail can remain behaviorally inaccessible | Explicit older-fact/recall scenarios; do not count archive existence as model recall |
| Compaction vs framing confounding | A new agent prompt could mask compaction defects | Hold actor prompt constant across strategies; test framing separately |
| Reusable card policy undecided | Cannot safely enable cross-site card storage | Explicit vault choice and separate security/consent verification |

The verify and architecture-review skills require these boundaries and
uncertainties to remain visible in the results and architecture write-ups.
