# Compaction study: findings and split scope

The second replacement for PR #642 contains context/budget/compaction fixes and
the reproducible evaluation harness. It builds on the turn-durability PR.
Credential security is the following separate PR. Legacy remains the default;
`compaction_strategy = "structured-message-tail"` opts into the candidate.

## Original study

The September 11 study used local Ollama 0.33.3 / gemma4:26b, synthetic sessions,
forced compaction and inert readouts. These are not exact historical production
replays or merchant journeys. [Full original report and raw evidence](evidence.md)
remain pinned to their original commit, including failures and build identities.
Splitting the code does not turn those runs into new standalone experiments.

The primary study has 80 trials: eight cases, two repetitions and five conditions.
Each compactor shares a 3,072-estimated-token ceiling, not equal actual usage.
Full history is a reference, not an equal-budget compression competitor.

| Condition | Raw complete readout | Schema-valid readouts | Complete facts among valid readouts |
| --- | ---: | ---: | ---: |
| Legacy | 8/16 | 15/16 | 13/15 |
| Structured | 11/16 | 14/16 | 11/14 |
| Structured + whole-turn tail | 11/16 | 15/16 | 12/15 |
| Extractive | 3/16 | 14/16 | 3/14 |
| Full history | 13/16 | 14/16 | 14/14 |

The separate exploratory message-tail arm has 16 trials: 11/16 raw combined
passes, 15/16 valid readouts and complete facts in 14/15 valid outputs.
Its median native probe input is 813.5 tokens versus 5,153 for full history.
A narrow morphology-only rubric repair raises its combined count to 12/16;
the original grades are preserved, not overwritten.

The final focused fieldwise-prompt follow-up has three trials: both repeated
date-correction trials pass, but the explicit-switch trial produces no checkpoint.
That failure has a correct retained summary and a normal native stop; it is not
proof that the task was deleted. These exploratory revisions are not pooled
into the primary comparison or presented as statistically established winners.

## Substantial findings

- Repeated compaction genuinely deleted older nonstop/carry-on constraints.
  Message-tail retained them through both reductions in both repetitions; the
  fieldwise prompt then improved the two targeted readouts.
- Three primary summaries ended at the generation limit. The guard added after
  that comparison rejects incomplete summaries before history replacement.
- Facts can remain in context yet be omitted by the model. Retention and use
  must be scored separately; absent output is not automatic semantic amnesia.
- A large tool result can crowd out useful adjacent dialogue in whole-turn
  retention. Message-tail can archive the bulky exchange whole and keep dialogue.
- Real daemon captures exposed contradictory tool guidance: an absent
  memory_search was reported active and recommended by the prompt. Filtering
  active names and making generic tool guidance conditional repairs that contract.

Eight original live daemon probes used full short synthetic Broadway context,
zero compactions and inert domain tools. Earlier ambiguous follow-ups include
clock drift despite retained context. Both final post-fix probes stayed on task
with valid offered tools. One pair is not a production reliability estimate.

## Tradeoffs and remaining work

Message-tail is the strongest candidate to advance, not a proven optimum.
It consumes more context than minimal summaries and relies on recall for raw
observations omitted from the prompt. Recent history can contain superseded
values, so corrections need provenance. Refusing overflow/incomplete summaries
can stop runs that previously continued with damaged context.

Before a default change: held-out sessions, many successive compactions,
archive recall after process restart, explicit task switches under different
readout protocols, dynamic schema-availability transitions and real browser
journeys. Transport gaps and async memory durability remain separate concerns.
No real login, booking, payment or filing is established by these readouts.

## Reproduction

See [study contract](compaction-study-contract.md) and
[daemon context suite](context-continuity.md). CI explicitly selects the
message-tail candidate for deterministic contracts; Legacy still fails the
deliberately lossy recent-context fixture. This is not a changed runtime default.
Raw bundles are kept outside these PR diffs, with immutable links and SHA-256
checksums in [the evidence index](evidence.md).

## Split validation — September 12, 2026

On top of the durability commit, 1,053 workspace tests pass (28 ignored), and
workspace/all-target clippy, formatting, architecture generation/checks and
diff whitespace pass. The rebuilt real daemon passes all 26 deterministic
context cases/surfaces with the explicit message-tail policy. This run uses
scripted model responses and inert domain tools; the 99 original live trials
are retained evidence, not rerun or relabelled. The six credential-related
source files are deliberately excluded until the final layer. Cargo uses
the documented offline/non-ONNX path.
