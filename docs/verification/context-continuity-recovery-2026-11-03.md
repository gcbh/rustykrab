# Local context continuity and recovery — 2026-11-03

## Failure and scope

Historical run `9524d230-f26d-487c-b915-6e2e7c934f3b` started in a clean
worktree, initiated a two-parent merge, then repeated immutable source reads.
The retained record has 27 successful model requests and 53 exec results.
Actual prompt size grew from 36,252 to 97,501 tokens within a configured
131,072-token window. Hard window exhaustion was not observed.

Release-code reproduction established loss of ordinary content accompanying
Ollama calls. The normal runner also discarded ModelResponse.text. The brief
clipped its active objective at 400 characters while embedding 39 work rows,
including stale planning constraints, and large results entered history whole.
Original raw responses were not saved; the individual causal contribution of
those defects to the model's repeated choice remains unisolated.

## Change and boundaries

- Retain ordinary mixed text through both Ollama modes, runner messages,
  persistence and the next wire request; exclude thinking-tag text.
- Preserve the complete active objective, acceptance, constraints and decisions.
- Index local project history only when recall is permitted; retain the exact
  frozen context in the conversation archive and save its parent before the
  FK-bound archive write. Without a reader the complete view stays inline.
- Archive large results before projecting a bounded preview, retaining full
  session-log observations, IDs, images, error state, shell verdict and progress
  warnings. Deliver late recall schemas by the provider's existing append path.
- Connect ordinary local workers to the existing SQLite recall backing via
  RunTranscripts. Frozen read-only reviews still omit automatic recall.

No schema, scheduler or write authority is added. Recall writes retain their
existing best-effort contract. Small duplicate outputs are not deduplicated;
fingerprint novelty remains distinct from progress on the goal.

## Deterministic execution verification

The workspace suite passed **2,516 tests, 39 ignored**, using four test threads.
Clippy, format, architecture and environment documentation checks passed.
An initial parallel suite hit two controller-fixture timing limits; both pass
in the complete reduced-concurrency run. No controller timing limit was changed.

New regressions check mixed text on the next request and after serialization,
streaming transport, retrievable large results and raw callback evidence, a
stable tool prefix under append binding, complete output without a permitted
reader, complete multiline assignment fields, and a 39-row historical context
recovered from SQLite through a fresh archive instance. The last initially
failed because the conversation parent had not been saved; ordering was fixed.

Checks ran on reviewed #744 code plus this patch. The branch then moved onto
the exact v6.6.19 source `8208ae342f04e4a1d2901caccc605d501a5e47ec`; the
release differs from reviewed #744 only in Cargo versions and CHANGELOG.

## Real executor and rollout evidence

The official v6.6.19 binary for #744 passed isolated self-correction, persistent
stall and real Codex recovery canaries. The native case retained spent switch
and repair history, restarted, accepted an explicit fixture-owner continuation,
produced a Git-verified merge and completed the original fixture operation.
Independent Cargo verification passed; usage, owner answers and a separate
consent hold remained. Fixture daemons and servers stopped.

v6.6.19 was deployed with signature/digest checks, rollback backup and advancing
healthy controller ticks. Five independent approval holds, 293 question records,
owner answers, cumulative usage, configuration and shared checkout were
preserved. A previously queued gateway task began running after deployment;
execution start is not proof of completion.

The context candidate's real Qwen historical-merge replay is still running at
this record's first commit. It uses the actual repaired daemon without proxy
text restoration or bounds. It retains both original source parents and the
incoming implementation requirement, but uses a focused fixture brief and the
runner's observed tool declarations. It is not a matched single-factor replay
of the original 19-tool conversation. Its final commit, tests and completion
must be recorded separately before claiming model or live effectiveness.
