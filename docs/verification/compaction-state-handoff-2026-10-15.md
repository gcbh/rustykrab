# Compaction state handoff: verification (2026-10-15)

## What changed

The default (`legacy`) compaction summarizer now writes an explicit state
handoff in named sections (`STATE_SECTIONS` in `compaction.rs`): OBJECTIVE,
LATEST USER INSTRUCTION, CONSTRAINTS AND PREFERENCES, CANDIDATES, FINDINGS,
ATTEMPTED, DONE, OPEN, NEXT STEP. On the recursive path it gets its own system
message (`SUMMARIZER_SYSTEM`), the history fenced in `<history>` tags and the
instructions after it. Every policy's summarizer reads the history without the
agent's own system messages. Retention is unchanged: the first and latest user
input stay verbatim, and the agent's system prompt stays in the compacted
conversation.

## Why

On qwen3.8:27b at a 131,072-token window, the old summarizer was handed the
agent's own instructions ("Complete the user's task… Keep going…") inside the
history. With thinking off on summary calls (since #709), it answered the task
instead of summarizing in 8 of 16 summary calls. The handoff then recorded the
answer's guesses as fact: "Agent lacks browser automation capabilities". The
history said only that one browser connection had timed out.

## How the prompt was chosen

A hill-climb on the context eval: `--mode context-model`, qwen3.8:27b-mlx at
131,072 (the Ollama runner checked at that window before every trial), cases
`broadway-noisy` and `broadway-explicit-reminder`, 3 rounds each. Candidates
were applied through an experiment-only override (not in this change).

- **Primary score:** the suite's own grader. Does the next action stay on the
  Broadway task.
- **Secondary score:** eight state features in the handoff:
  1. the objective;
  2. exact dates;
  3. location;
  4. the named candidates;
  5. the blocker stated accurately;
  6. no invented capability claim;
  7. written as a handoff, not a reply;
  8. a next step.

| Candidate | On task | Features | Summary calls that answered instead | Mean trial |
|---|---|---|---|---|
| C0, the old prompt | 2/6 | 0.46 | 8 | 266 s |
| C1, old wording + no agent prompt, own system message, instructions last | 6/6 | 0.88 | 0 | 320 s |
| C2, named state sections | 6/6 | 1.00 | 0 | 391 s |
| C3, C2 with thinking on | 6/6 | 1.00 | 0 | 446 s |
| **C4, C2 + constraints/preferences and candidates (this change)** | **6/6** | **1.00** | 0 | 409 s |

C0's time is low partly because in 2 trials a summary call ran to its output
limit, compaction aborted, and the turn took no action.

### Generalization

The compaction study has 8 different scenarios: task switches, corrections,
older facts, unknown outcomes and returning to a topic. Each ran 2 times, on
the one-call path:

| | Old prompt (three runs) | C2 | **C4** |
|---|---|---|---|
| State kept (scored runs) | 37/39 | 14/16 | **16/16** |
| Fully passed (audited) | 11, 11, 11 of 16 | 10/16 | **14/16** |
| Readout errors | 9 of 48 | 0 | **0** |

C4's two misses are the readout's own schema check. C2's real loss was
preferences stated mid-session ("nonstop, carry-on included") and an
unverified candidate flight. C4's CONSTRAINTS AND PREFERENCES and CANDIDATES
sections exist for that.

### Scoring audits

All three were applied to every candidate alike. Every flagged sentence was
read before a number was accepted.

- The invented-claim check went through three versions. The final one flags a
  sentence about the browser that denies its availability or the agent's
  capability. It does not flag a conditional, a record of what was not tried
  ("No browser tool was invoked"), or a statement about static fetches.
- The study's false-completion check counted negated statements ("Confirmed no
  tickets purchased", "NOT confirmed saved") as completions. The audited score
  ignores a forbidden word in an item that negates it.
- The study's fact check required ISO dates. The audited score also accepts the
  same date written out ("September 18-20, 2026").

### Eval fix in this change

The context fixture's show dates move to September 2027. With the old dates
already past, qwen sometimes declined, correctly, to look for seats on them,
and the action grader scored that as a failure.

## Production build

The production code (this change, not the experiment's override) on the same
eval, with the same guard, finished 2026-10-16 00:26 UTC.

| | Result |
|---|---|
| On task | **5/6** (`broadway-noisy` 2/3, `broadway-explicit-reminder` 3/3) |
| State features | **1.00** in every trial |
| Summary calls that answered instead | 0 |
| Clock drift | 0 |
| Runner window | 131,072 throughout |
| Mean trial | 372 s |

The miss is not drift, and not a compaction loss. Its handoff scored all eight
features. qwen's first words were "I'll use the browser to check Broadway show
availability for September 14–16, 2027". It then spent the eval's four steps
on the browser's `status`, `start`, a tool lookup and `status` again. The eval
stubs the browser to fail, and none of those calls names Broadway, so the
grader cannot credit them. qwen then stopped with an accurate account of the
failures. That is within the run-to-run variation of C4's 6/6 in the climb.

## Gates

On the exact tree, base #717 `c8e40a1`:

| Check | Result |
|---|---|
| `cargo fmt --all -- --check` | clean |
| `cargo clippy --workspace --all-targets -- -D warnings` | clean |
| `cargo test --workspace --no-default-features --no-fail-fast` | 2,372 passed, 0 failed, 38 ignored |
| `scripts/e2e.sh` (scripted) | 69 pass, 7 xfail, 0 fail, 0 xpass |
| `scripts/e2e.sh --mode context --trial-timeout 30`, CI's env | 28/28; the `provider-trim-control` pre-dispatch refusal is intact |
| `python3 scripts/check_architecture_docs.py --fix` | metrics regenerated; OK |
| `python3 scripts/check_env_docs.py` | OK |

New test: `runner::tests::the_summarizer_is_not_handed_the_agents_instructions`.
On the recursive path, every summarizer request carries `SUMMARIZER_SYSTEM`,
never contains the agent's system prompt, and fences the history before the
instructions. The compacted conversation keeps the agent's system prompt.
Also `compaction::tests::the_default_handoff_names_every_section_and_does_not_act`.

## Limits

- **Both evals are at or near their ceiling.** C2, C3 and C4 cannot be told
  apart on the Broadway cases. C4's edge rests on the study, at 2 repetitions
  per scenario.
- **One model, one hardware setup:** qwen3.8:27b at 128K on a 36 GB Mac.
- **The handoff is longer,** about 650–750 words against about 150. That costs
  prompt tokens on every later turn, which a 128K window absorbs.
- **Not measured:** the real token count of the full eval history. Requests are
  chunked, and tool output over 8 KiB is elided before summarizing.

The raw artifacts are in
`~/projects/rustykrab-antifragile/evidence/compaction-prompt-climb-20261015/`.
