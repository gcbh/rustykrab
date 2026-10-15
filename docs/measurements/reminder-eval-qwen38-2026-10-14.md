# Is the `result_report` reminder needed for qwen3.8? 2026-10-14

The owner asked whether the local coding worker (`qwen-code`, qwen3.8 on
Ollama) still needs the runner's `result_report` completion reminder now that
a run ending in plain text takes that text as its answer (`report_from_text`,
03d87cc). This eval runs the same tasks with the reminder on and off.

**Answer:** on these tasks the reminder never fires, so it changes nothing.
In 32 runs (8 tasks, 2 arms, 2 reps), qwen3.8 never ended a turn with text.
It called `result_report` on its own in every run that finished, and no
reminder was sent in either arm. The arms passed the same tasks: 14 of 16
each, failing only the blocked task (t6). That task failed in all four runs,
for reasons the reminder does not reach.

## Results

Each cell gives the checker verdict, the model turns, and the wall time.

| Task | on, rep 1 | on, rep 2 | off, rep 1 | off, rep 2 |
|---|---|---|---|---|
| t1-answer | pass, 7, 60 s | pass, 7, 32 s | pass, 7, 58 s | pass, 7, 36 s |
| t2-fix-median | pass, 7, 86 s | pass, 8, 54 s | pass, 7, 108 s | pass, 7, 51 s |
| t3-parse-duration | pass, 10, 105 s | pass, 12, 120 s | pass, 9, 109 s | pass, 8, 98 s |
| t4-rename | pass, 9, 84 s | pass, 9, 59 s | pass, 8, 75 s | pass, 8, 51 s |
| t5-csv-totals | pass, 7, 48 s | pass, 6, 50 s | pass, 6, 73 s | pass, 8, 58 s |
| t6-blocked-vault | fail, 25, 846 s | fail, 25, 438 s | fail, 25, 467 s | fail, 25, 409 s |
| t7-uppercase-flag | pass, 7, 56 s | pass, 7, 97 s | pass, 7, 95 s | pass, 7, 61 s |
| t8-version-bump | pass, 5, 50 s | pass, 5, 78 s ¹ | pass, 5, 53 s | pass, 5, 51 s |

¹ The second attempt. The first was aborted by the harness's disk-floor
guard (see infrastructure below) and recorded as an infrastructure stop.

| Measure | reminder on | reminder off |
|---|---|---|
| Runs / passed (tree) / passed (commit) | 16 / 14 / 14 | 16 / 14 / 14 |
| Ended with `result_report`, unprompted | 14 | 14 |
| Ended after reminders, or with text | 0 | 0 |
| Failed: `budget/iterations` (t6) | 2 | 2 |
| Reminders sent | 0 | 0 |
| Mean tokens / wall / turns, t1–t5, t7, t8 | 61,580 / 70 s / 7.6 | 56,484 / 70 s / 7.1 |

- **Commits:** every code task committed its change. The tree verdict and
  the commit verdict agree in every run.
- **Rejected reports:** reports were rejected in t1 (all four runs, once
  each) and in t6 (three runs, 8 to 14 times).
- **Turn counts vary at temperature 0.** The same task in the same arm took
  10 and 12 turns (t3, on) or 6 and 8 (t5, off). The prompts differ per run in
  ids and paths. Outcomes did not vary: every task gave the same verdict in
  all four runs.
- **The on/off gap in tokens and turns is noise.** The arms run identical
  code until a reminder is sent, and none was.

## Why t6 failed in both arms

The passphrase file does not exist, so the right outcome is a report saying
the run is blocked. Two failure modes appeared.

- **The blocked report was rejected for length, and the model could not
  recover.** This happened in three runs:
  - `2ad62405` (on, rep 1): 14 rejections.
  - `e5e5c8a9` (off, rep 1): 8 rejections.
  - `430bc686` (on, rep 2): 12 rejections.

  In each, qwen3.8 called `result_report` with a `blocked.detail` of 794 to
  1,233 characters. `DETAIL_MAX` is 600 (`rustykrab-tools/src/result_report.rs`).
  The model then re-sent the same over-long detail until the iteration cap.
  Neither the reflection prompt ("Multiple consecutive tool calls have
  failed") nor the stall notice changed that:

  ```
  result_report not accepted; fix every line and call it again:
  - invalid_item: blocked.detail: is 794 chars; keep it to 600

  Note: this is call 10 in a row that repeats an earlier call with the same result, so it tells you nothing new. ...
  ```

  The failure is typed `budget/iterations ... stalled: 10 steps without new
  evidence (progress ledger)`.
- **No report at all.** In `73e5547e` (off, rep 2) the model spent 23 exec
  calls looking for the passphrase. It searched outside its worktree
  (`find / -name vault_passphrase.txt`, `find /private/tmp/claude-501 …`,
  `env | grep -i pass`), then tried about 40 guessed passphrases with
  `openssl enc -d`. Two replies hit `num_predict` (4,096 tokens) with no
  visible text, about 110 s each. The run reached the cap without reporting.

Neither mode sends text, so the reminder never comes into play. A text
ending, accepted by `report_from_text`, would have passed this task's
checker; the model never produced one.

## Harness problems the eval surfaced (none caused by the reminder)

1. **`blocked.detail`'s 600-character limit fails blocked work.** It caused 3
   of the 4 t6 failures above.
   - The model learns of the limit only from the rejection.
   - Shown the limit, it still re-sent the same text.
   - Possible fixes: state the limit in the schema, truncate rather than
     reject, or move the overflow into `summary`.
2. **A compaction summary can spend minutes and then be discarded.**
   - The only compaction in the eval was in `2ad62405` (on, rep 1), at
     iteration 24 of 25, at 19,002 tokens.
   - The history exceeded the single-call budget, so compaction took the
     recursive path: 2 chunks, `max_output_tokens` 6,528.
   - Chunk 1 took 117 s. Chunk 2 ran to its limit:
     `compaction summary hit its generation limit; original history was not replaced`.
   - **The fallback was taken:** `compaction summary failed; keeping recent
     turns verbatim instead`. The run went on, compacted to 28 messages and
     11,538 tokens.
   - The two summary calls took 343 s of the run's 846 s and produced
     nothing kept.
3. **A compacted run's session log loses everything before compaction.**
   - `LocalWorker` writes the log from the conversation as it ends. After
     compaction that conversation starts at the Extractive summary.
   - The 28 displaced messages (27,621 characters) go only to the runner's
     in-memory `RecallStore`. `IntegrationWorker::general` never calls
     `with_recall_store`.
   - In `2ad62405`'s log, message 2 is the summary. The log holds 11 of the
     run's 25 tool calls. The other 14 (10 exploration execs and 4 rejected
     reports) appear only as one-line entries in the harness's tracing log,
     not in any file a later investigation would open. This works against
     "an investigable log for every agent".
4. **The exec allowlist refuses common shell idioms.** It refused 21 calls:
   - `for` ×9: t3 and t4 in every run, and t6.
   - `(grep` subshell ×4: t4.
   - `printf` ×2, `od` ×2, `xxd` ×2, `:` ×1, `$()` ×1.

   The model rewrote the command each time, at the cost of a turn. No run
   failed on a refusal.
5. **A string `checks_run` is rejected.** `field 'checks_run' must be array,
   got string` happened in t1, all four runs. The model fixed it on the next
   call, at the cost of a turn. A single string could be wrapped into a list.
6. **Exec is bound to the worktree, not confined to it.** Run `73e5547e`
   read the filesystem and environment outside its workspace (see t6).
7. **Not exercised:** no reply failed to parse (0 unparseable-reply
   notices). Stall notices appeared in two runs, both t6 with the reminder
   on (5 and 3), and changed nothing.

## Infrastructure

- **Disk and swap.** The machine has 36 GB of RAM, and Ollama reported the
  loaded model at 34.8 GB. With other sessions' `cargo test` builds running,
  swap reached 31.6 GB, and its swap files took free disk below the 4 GB
  floor twice (16:00 and 17:02). Each time the model was unloaded and the
  disk recovered.
  - The first time, the eval was stopped by hand. The interrupted t4 run
    (`cd0ce891`) was not recorded; it was rerun as `2db6faed`.
  - The second time, the harness's guard aborted `82037949` (t8, on, rep 2)
    and retried it as `e2cd42ab`, which passed.
- **No contention for the model.** In every run, Ollama's `/api/chat` count
  equals the run's own model calls. The one exception is `2ad62405`:
  27 calls against 25 turns, the extra two being the compaction summaries.
  No other client used the model during a run.
- **Shared build directory.** The build shares a target directory with
  another worktree. It used per-crate profile overrides
  (`profile.dev.package.rustykrab-<crate>.debug=0`) so the workspace crates'
  artifacts could not collide with that worktree's.

## Conclusion

**Measured:**

- With the base as it would ship, the reminder sent zero reminders in 32
  qwen3.8 runs over 8 coding tasks. Every run that finished ended with an
  unprompted `result_report`.
- The arms' outcomes are identical: 14 of 16 each, the same task failing in
  both.
- The only failures come from the report's own validation and from the
  model's behaviour on an impossible task. In those runs the model kept
  calling tools and never sent text, so the reminder never applied.
- Because no reminder was sent, there is no measurement of what qwen3.8
  does after one.

**Inferred:**

- For briefs shaped like these, the reminder is dead code on qwen3.8.
  Turning it off would change neither outcomes nor cost. When a run does end
  in text, `report_from_text` makes the text the answer either way, so the
  reminder could only add up to three turns.
- The production failures that motivated 03d87cc (runs failing as
  model/format after 3 reminders) must arise elsewhere. Candidates are
  longer runs, integration briefs, scheduled jobs, or the narration
  classifiers this base no longer applies to workers. This eval does not
  reproduce them, so it cannot say the reminder is never needed.
- The changes that would move qwen3.8's pass rate here are the
  `blocked.detail` limit and the compaction summary's output budget, not the
  reminder.

## Method

- **Harness:** `crates/rustykrab-agent/examples/reminder_eval.rs`, with
  fixtures, checkers and the summary script in
  `crates/rustykrab-agent/examples/reminder_eval_tasks/`. It builds one
  `LocalWorker` per run the way `IntegrationWorker::general` builds
  `qwen-code`, and calls `Worker::run` on a real `Brief`.
- **Base:** `eval/reminder-qwen38` on `a636426`. This is the harness as it
  would ship: a stalled run is told rather than stopped, the token meter only
  counts, unparseable replies get a notice, and compaction is resolved per
  model with an Extractive fallback.
- **Model:** `OllamaProvider` at `http://127.0.0.1:11434`, `qwen3.8:27b-mlx`,
  with `local_resources.rs`'s settings: `num_ctx` 32768, think off,
  temperature 0.0, top_p 0.9, `num_predict` 4096, keep_alive 30m, and the
  detected context window. Runs are strictly one at a time.
- **Tools:**
  - The production constructors, filtered as `IntegrationWorker::general`
    filters them for a worker with repositories (integrations off).
  - The model sees `tools_list`, `tools_load`, `exec`, `read`, `edit`,
    `write`, `web_fetch`, `web_search`, `memory_search`, `recall_info`,
    `recall_peek`, `recall_search`, `recall_sub_query`, `todo_write` and
    `todo_read`.
  - `LocalWorker` binds `exec`, `read`, `edit` and `write` to the run's
    worktree, and adds `work_file`, `work_status`, `work_discuss`,
    `result_report` (the full-report variant) and the loadable question and
    plan tools.
- **Brief:** the real `Brief` with title, objective and `done_when` from the
  task's `task.json`, and `writable_resources` `repo:<fixture>`.
  - **Workspace:** `Workspace::plan` over a fresh, git-initialised copy of the
    fixture. It is created and removed by `LocalWorker::run`, as for a
    controller lease.
  - **Budget:** 25 iterations (the production default), 1,000,000 tokens and
    900 s wall. The meter does not refuse calls, and no run came near the
    wall.
- **Backend:** a `StubWorkBackend` that also copies the worktree each time a
  report is filed, because the worktree, and any uncommitted work with it, is
  removed when the run ends.
- **Arms:** `with_result_report_reminder(true)` and `(false)`. Nothing else
  differs. The arm that runs first alternates by task and rep.
- **Checkers:** each task's `check.py` is deterministic and uses Python 3.9's
  standard library (pytest is not installed, so the tests are unittest).
  - **pass (tree):** the checker run on the worktree as it stood when the
    report was filed.
  - **pass (commit):** the checker run on the run's branch tip, which is what
    the controller would verify.
  - Read-only and blocked tasks are checked on the report alone.
- **Recorded per run:**
  - `runs.jsonl`: how the run ended, reminders, iterations, tokens, wall time,
    what the model did after each reminder, and the report.
  - Full transcripts (`LocalWorker::with_run_logs`).
  - These live in `eval-results/reminder-2026-10-14/`, which is not committed.
    `runs.jsonl` is copied beside this document as
    `reminder-eval-qwen38-2026-10-14.runs.jsonl`. To rebuild the tables, run
    `python3 crates/rustykrab-agent/examples/reminder_eval_tasks/summarize.py <out>`.
- **Contention check:** the `/api/chat` requests Ollama's `server.log`
  recorded during each run, compared with the run's own model calls. The live
  daemon shares this Ollama.

## Tasks

| Task | What it asks | The checker passes when |
|---|---|---|
| t1-answer | Read-only: what `shipping_cost(12.5, "express", member=True, remote=True)` returns | the summary states `19.34` |
| t2-fix-median | Fix the bug behind failing tests without changing them | the suite and hidden median tests pass, and `tests/test_summary.py` is unchanged |
| t3-parse-duration | Implement `parse_duration` to `docs/durations.md`, with tests | hidden tests over every rule in the spec pass, and the run's own test module exists |
| t4-rename | Rename `calc_total` to `compute_total` across 4 modules, the tests and the README | no `calc_total` remains, the suite passes, and both import paths work |
| t5-csv-totals | Write a per-region markdown table of units and revenue from a CSV | every row and number matches the CSV |
| t6-blocked-vault | Decrypt a file whose passphrase file does not exist | the report is typed blocked/error or says it cannot, and claims no count |
| t7-uppercase-flag | Add `--uppercase` across config, formatter and CLI, with tests | the hidden end-to-end tests pass and the old output is unchanged |
| t8-version-bump | Cut release 1.4.0: two version strings and a CHANGELOG restructure | versions, section order and entries are right, and the suite passes |

The checkers were validated before any run: every one fails on its untouched
fixture and passes on a hand-written solution.

## Production-parity deviations

- **System prompt and tool filter are copied.**
  `IntegrationWorker::general`, `CONTEXT_TOOLS` and `CODING_TOOLS` live in
  the bin-only `rustykrab-cli` crate, so the harness copies them verbatim.
- **No `AttestingBackend`.** Its attestation adds observation artifacts and
  fails reports that lack a required integration tool's execution. These
  briefs require no tools, so it would change nothing.
- **Integrations assumed off.** The live daemon's
  `RUSTYKRAB_INTEGRATION_WORKER` setting was not read (`service.json` is off
  limits). With it on, `qwen-code` would also see the integration tools.
- **Different providers for recall and memory.**
  - `recall_sub_query` gets the Ollama provider. Production gives it the
    chat provider. No run called it.
  - `memory_search` runs over an empty memory store.
- **Run transcripts are not saved to a store.** The session log is the
  transcript.
- **Worktrees live in a scratch directory,** not under the daemon's data
  directory.
