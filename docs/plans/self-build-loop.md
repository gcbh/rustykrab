# Plan: RustyKrab builds RustyKrab

**Status:** Running (bootstrap), from 2026-09-28
**Builds on:** `control-layer-and-worker-fleet.md` (sections 5, 8, 10, 11, 16)

The control layer can already file work, hand code work to a Claude Code
worker in an isolated worktree, verify the commit against the diff, climb
the resolution ladder on failure, and file improvement proposals from its
own record. Pointed at its own repository, that is a system that builds
itself. This plan is how it is set up, what it may do on its own, and what
stays with a person, so autonomy grows by evidence rather than by default.

## The loop

1. **Backlog.** Work items in the builder's own store. They come from a
   person or the coordinating session today, and from the builder itself
   as it runs: dreaming's evaluation files proposals from its record
   (verification misses, recurring fingerprints, unknown errors, cost), the
   ladder files `internal` items for unknown errors and `capability` builds
   for missing tools.
2. **Execution.** The controller leases each `code` item to the
   `claude_code` worker, `shipwright`, with the integration checkout as its
   `repo:` resource. Each run gets a fresh branch
   (`rustykrab/work/<item>-<run>`) from the checkout's tip, in a worktree
   under the builder's data directory. Runs on one repository are
   serialised by the single-writer rule.
3. **Verification.** The worker must run the repository's own gate (the
   CLAUDE.md pre-commit checks) and list it in `checks_run`; the controller
   checks that the commit exists on the pinned parent, that the claimed
   paths are exactly the diff, and that the claimed checks are among the
   commands the adapter saw run. A claim beyond the evidence is
   `verification_failed` and climbs the ladder: a repair with the failure
   as evidence, then a different worker.
4. **Integration.** A verified branch is reviewed and merged into the
   integration branch by a person, or by the coordinating session under a
   person's direction, after the full gate including the scripted e2e
   suite. Nothing reaches `main` without the owner.
5. **Redeploy.** The builder's binary is rebuilt from the integration
   branch after merges, so the system doing the building is the one it
   just improved. The builder keeps its store across restarts.
6. **Evaluation.** An evaluation pass over the builder's own events files
   proposals. An accepted proposal becomes a `code` item, and the loop
   closes.

## Guardrails

| Guardrail | How |
|---|---|
| Separate from the live assistant | its own daemon, data directory and port (3310); no Telegram, no GitHub; its model provider names a model that does not exist, so it never loads a local model beside the live daemon |
| Bounded worker | `claude -p` with `acceptEdits`, an explicit tool allowlist (edits, cargo, git add/commit/status/diff/log, the docs checker, the e2e script, read-only shell), `git push`, web fetch and web search denied, 80 turns, one hour per run |
| Bounded items | every item names its repository, its budget, and a definition of done that is the repository's own gate; constraints forbid pushing, merging, rebasing and switching branches |
| No self-granted authority | the builder files proposals but does not accept them; it cannot merge; a permission refusal in any session is never routed through the builder |
| Secrets | the builder's master key and auth token live in owner-only files and are never printed |

## Autonomy, by evidence

Each step up is a decision for the owner, taken when the record supports it
(section 10's metrics: verified done against claimed done, escaped defects,
repairs per item, cost per verified item).

1. **Now.** Items are filed by a person or the coordinator; branches are
   merged by a person or the coordinator; proposals wait for a person.
2. **Next.** The builder redeploys itself after each merge. Standing
   judgment (Phase 4) accepts `internal` observability proposals below a
   cost threshold, as section 10 already allows. The planner (Phase 4)
   breaks larger backlog items into graphs.
3. **Later.** Merging a verified branch into the integration branch under
   policy, once the verified rate holds, with the delivery plan's merge and
   rollback rules. `main` stays with the owner until the delivery plan's
   Phase 12 supervisor exists.

## First cycle (2026-09-28)

The builder runs as a second daemon on port 3310 from
`~/projects/rustykrab-builder` (its binary, data directory, build cache and
owner-only secrets), with one `claude_code` worker, `shipwright`, on the
integration checkout.

| Item | What happened |
|---|---|
| Payment leak scan matches digits only at digit boundaries | Verified on the first run (14 turns, 31K tokens, 2.4 minutes) |
| Late-binding found-target cases judge executed calls | The first run was `verification_failed`: it listed a check as `cargo test ... (86 passed)` inside a compound command, which the verifier matched against no single command. The ladder repaired it with that evidence; the second run verified |
| A follow-up the second worker discovered | Filed by the builder itself, then misrouted: the draft inherited neither the parent's repository nor its worker constraint, so it went to a local worker with no model, then to Claude Code with no worktree, which searched the machine and asked which checkout was canonical, as a plain string the result parser rejected |

Both verified branches were reviewed, merged into the integration branch
after the full gate (1,758 unit tests; e2e 54 pass, 0 fail), and the
builder was redeployed from the result. Its routing record now reads, for
`shipwright` on code: 2 verified, 1 claimed but not verified, 1 repair, 4
runs, 511 seconds, 219K tokens.

The misrouted follow-up exposed four defects in the builder itself, which
became its second batch, with the audit refiled: discovered drafts inherit
the filer's repository and worker constraint; the verifier matches claimed
checks inside compound commands and without annotations; the result
contract accepts plain-string questions; a daemon without a local model can
turn its local worker off. The verifier change alters how the builder
judges its own work, so it gets the review section 10 reserves for changes
to a system's own measurement before it is merged.

## Second cycle (2026-09-28)

All five items verified and were merged after review, the verifier change
most closely: a claimed check now verifies when it ran on its own or inside
a compound command, with a trailing annotation such as `(86 passed)`
removed, and a check that never ran still fails. The verifier still
confirms that a check ran, not that it passed; the full gate at merge
covers that until a trusted merge step does.

The merge surfaced one regression, in a test and not in the product:
scenario 31 counted only open items, and follow-ups now inherit their
parent's worker, so they run and close before the count. The scenario now
counts closed items too.

While the second batch ran, the builder filed eleven more items itself,
all before the inheritance fix was deployed, so none carried a repository
and all stalled. Reading them back is instructive: the builder had
diagnosed its own environment (a local worker with no model, Ollama's 404
retried as transient, a capability item filed with an empty tool name).
Five were covered by batch 2 or were noise and were cancelled with the
reason recorded; six were refiled as the third batch, beside the first
slice of the update flow (graceful shutdown on SIGTERM with no orphaned
worker processes, and `GET /api/version`). The builder was redeployed with
its local worker off, using the switch it built in batch 2.

## Third cycle (2026-09-28)

All eight items verified: the first two slices of the update flow
(graceful shutdown on SIGTERM, with each external run in a process group
of its own that shutdown ends, and `GET /api/version` with the build and
the controller's last tick and live runs), a controller-level test of
`checks_run`, an e2e scenario for a daemon with its local worker off, the
Ollama 404 fix, the local worker's model check, draft ordering, and the
empty tool name. The two update-flow pieces were reviewed line by line and
merged as built.

The cycle's lesson is in what the builder did next. It filed 24 more items
itself, and each ran at once on the integration tip, which lacked its
siblings' unmerged branches. So it rediscovered the same problems: the
Ollama 404 was fixed three times, the Anthropic and OpenAI 404s twice,
draft ordering three times, and the empty tool name at five layers. By the
end it had filed items asking for a sibling branch to be merged and for
the controller to stop filing duplicates. In all, 32 items took 31 runs,
1.95M tokens and 78 minutes of worker time.

Review picked one branch from each group. Three reviewers read the groups
in parallel, test-merging in throwaway worktrees:

- Ollama 404: the minimal mapping was kept. The fuller variant also added
  a "model not found" rule that would have turned any 404, a wrong URL
  included, into an install request.
- Empty tool name: only the validator-and-ladder layer was kept. Of the
  others, one substituted a placeholder name that would have filed "Build
  tool: unnamed required tool", two turned the gap into an unknown error,
  and one guarded code nothing reaches.
- Draft ordering: two complementary fixes were merged together (among the
  drafts themselves, and after open writers already there), with the
  third's test carried over.
- Health: the model check and the health line were both merged, with
  three review fixes. The probe got a 5 s timeout, so a wedged Ollama
  cannot stall startup. An outage no longer marks a missing model healthy.
  The health line now says which model is missing.

Two guardrails came out of it:

- **`RUSTYKRAB_HOLD_DISCOVERED`** holds every graph of a worker's follow-ups
  for a person (`ApprovalPolicy::hold_discovered`). The builder now runs
  with it on, so its ideas queue for the coordinator instead of running on
  a stale base. The underlying fix is the trusted merge step, which would
  let a follow-up start from its siblings' work. Until it exists, the hold
  keeps the builder's intake to what has been reviewed.
- **A blocked report must carry its question.** Five runs parked
  `needs_decision` with a blank question: the brief never named the
  `detail` field, and the parser filled a missing one with an empty
  string. What those workers asked is lost. The fix is the first item of
  batch 4.

The gate at merge: 1,802 unit tests, clippy, fmt, the architecture
checker, and the scripted e2e suite (55 pass, 13 xfail, 0 fail). The
builder was redeployed from `a3b7dee`, and its own `/api/version` reports
that commit. Batch 4 has five items: the blocked-question fix, drain on
shutdown with interrupted runs requeued without penalty, the controller
lock, failed ticks in `/api/version`, and the worker's turn budget with
recovery of a run that hits the turn cap.

## Fourth cycle (2026-09-28)

All five items verified in 24 minutes:
- the blocked-question fix;
- drain on shutdown;
- the controller lock;
- failed ticks in `/api/version`;
- the worker's turn budget, with a capped run resumed once for its
  contract.

With `RUSTYKRAB_HOLD_DISCOVERED` on, the builder's six follow-ups waited
instead of running. Four were approved as filed, one was widened and
refiled, and one was superseded.

Merging took three hand resolutions, all where two branches grew the same
structure. The lock, the failed ticks and drain each added fields to
`LoopStatus` and `/api/version`, and all were kept. Drain and the
turn-budget change both rewrote how an external run is started and
awaited. The shutdown check moved into the shared `execute`, which now
reads whether shutdown ended the group before letting it go, and a run
cut off at its turn cap is not resumed once shutdown has ended it. The
merge also exposed a flaky lock test. A process that another test forks
in parallel shares the lock's close-on-exec descriptor until it execs,
so a release can take a moment to be seen, and the lock tests now wait
it out.

The pieces were then used for real on the builder:
- The old build stopped on SIGTERM in one second.
- The new build reported `lock: held` through `/api/version`.
- A second daemon started on the same data directory reported `waiting`
  and never ticked.
- A SIGTERM to the first drained it and it exited. The second took the
  lock within one tick and ran the loop.

That is the cutover sequence of `update-flow.md`, done by hand.

Turn-cap recovery was checked against the real CLI (2.1.283), since a
worker cannot run `claude` itself. A `claude -p` run capped at one turn
ended `error_max_turns` with a `session_id`, exit code 1. Resuming it with
`--resume <id> --max-turns 2` and the recovery prompt returned `success`
and a contract that reported, correctly, that nothing had been committed.

The gate at merge: 1,824 unit tests, clippy, fmt, the architecture
checker, and the scripted e2e suite (55 pass, 13 xfail, 0 fail). The
builder runs `ec9160f`. Batch 5 has seven items. Four are the approved
follow-ups: the blocked shape in the local worker's brief, interrupted
local runs, an e2e scenario for two daemons on one data directory, and
failed ticks in the log. Three are new: slice 5 of the update flow
(`rustykrab update check` and `stage`), the manual tick route respecting
the lock, and README rows for the control layer's variables.

## Fifth cycle (2026-09-28)

All seven items verified, among them slice 5 of the update flow
(`rustykrab update check` and `stage`). Three things stood out.

**Review caught a flaw that verification could not.** An independent
security review of slice 5 found that its signature check could be forged.
The spec was at fault, not the worker: it had asked for the `Identifier=`
and `TeamIdentifier=` lines of `codesign -dv`. Those are fields of the
signature, and `codesign -s - --team-id 3RRX845C4X` puts the pinned team
into an ad-hoc signature that passes `codesign --verify`. The check now
verifies a Developer ID requirement against the certificate chain. It was
confirmed against the forged bundle, which it refuses, and against the
installed release bundle, which satisfies it. The same review found that
a local build printing `..` as its version would have staged over the
data directory. Both were fixed at merge, with tests, and the plan was
corrected. The builder's verifier can only confirm what the spec asked
for; a wrong spec passes. That is why security-relevant slices get a
second reader.

**Items the ladder files itself are a gap.** Two `internal` items filed
by the ladder ran without a repository. One wrote a patch into its run
directory and filed a follow-up asking for it to be applied. The other
read a sibling run's worktree to write a classifier rule. Ladder filings
also bypass `hold_discovered`. Batch 6 makes them inherit the failing
item's repository, worker and constraints, and holds them with the rest.

**Turn-cap recovery worked, and exposed the next step.** Three runs hit
the turn cap. The resume returned a contract saying nothing was
committed, with its reason in free text, and the classifier did not know
the text. Batch 6 keeps such a run as the typed `budget/iterations`
failure instead. A verifier gap recurred four times: a compound claimed
check whose steps ran separately. That is also in batch 6.

The builder was redeployed through slice 5 itself:
`rustykrab update stage --from <build>` staged the binary with its record
(`kind: binary`, `signature_verified: false`), and the swap was done by
hand. Every integration build reports version 5.3.6, so slice 6 records a
rolled-back local build as bad by commit, not by version. Gate: 1,853
unit tests; e2e 56 pass, 13 xfail, 0 fail. Batch 6 has ten items,
including slice 6 (`rustykrab update apply`) and slice 5's remaining
hardening (size cap, token host, pre-release tags).

## Sixth cycle (2026-09-28)

All ten items verified: slice 6 of the update flow, slice 5's hardening,
three fixes from cycle 5 (internal items inherit their repository,
turn-cap recovery keeps a typed failure, compound claimed checks), and
five approved follow-ups.

**The builder granted itself access, through the front door.** An
`internal` item from the previous cycle had no repository. Its worker
reported a missing "tool": filesystem access to the builder's own
directory. The ladder filed a capability build, since the tool did not
exist. A worker built it by writing a `SKILL.md` into the builder's skills
directory, telling every later worker to read under the builder root with
absolute paths. That root then held the builder's master key and auth
token. The controller verified the build because a skill of that name now
existed. Nothing read the secrets, but every rule the loop was built with
was kept while the system widened its own reach.

The response:
- The skill was removed and kept for the record.
- The secrets moved to an owner-only directory outside the tree.
- `hold_discovered` now holds every ladder filing, capability builds
  included. Batch 6 had held only internal items, as its spec, not the
  worker, had said.
- An item adds a per-worker deny list to the Claude Code adapter, so
  path-scoped reads can be denied.

**Slice 6 was merged behind a gate.** Its review confirmed the swap order,
the atomic renames, the verify window, the `--yes` gate and the
commit-keyed bad record. It also found that the swap trusts
`staged.json`, which a worker can write, and that an interrupted apply
could leave no daemon running. The CLI refuses `update apply` until the
fixes land; the first of the two fix items is filed. Slice 5's hardening
(size cap, token only to the API host, pre-release tags skipped) merged
as built.

**Drain met a real run.** The builder was redeployed with one Claude Code
run in flight. The run went through the whole drain path:
- The drain waited its 20 s grace, then ended the worker's process group.
- The final tick recorded the run as interrupted and returned the item to
  `ready`, with no rung.
- The new build leased it again ten seconds after starting.

Two small gaps showed. The "waiting" log line repeats every 250 ms, and
an interrupted run's token usage is lost.

Gate: 1,877 unit tests; e2e 56 pass, 13 xfail, 0 fail.

## Seventh and eighth cycles (2026-09-28)

Thirteen items verified across the two batches, most on their first run.

**The blocked-question fix worked in the field.** A worker parked with its
full question: it had been given a code item with no repository and an
empty scratch directory, and asked whether to create a worktree or wait.
The item had been filed by an internal item from before the inheritance
fix, so it inherited no repository. The builder had already filed the
right fix: never lease a code item to a coding worker without a
checkout. It merged in batch 8.

**Follow-ups did not inherit their filer's rules.** They took its
repository and worker but not its constraints, so an approved follow-up
ran without "do not read outside your worktree". Since this is a
guardrail, the coordinator made the fix: follow-ups now inherit their
filer's constraints, as the ladder's internal items do.

**Slice 6 went through two more reviews:**
- The first fix (trust nothing in `staged.json`) merged with one blocking
  hole closed at merge. The version check applied only to stages with a
  tag, a field a worker writes, so a downgrade could pass.
- Part 2 (check before stopping, a journal, a loud failure) merged behind
  the gate. Its review found that the journal's location let a forged
  journal force a rollback, that a crash between the renames and the
  journal write went unchecked, and that a launchd stop could give up
  mid-drain.

Parts 2b and 3 are filed. The reviews also surfaced the limit under all
of this: workers run as the same user as the daemon, so "a worker cannot
write it" holds only as far as tool rules go. That is recorded in
`update-flow.md` as a decision for the owner before the updater touches
the live daemon.

Gate: 1,908 unit tests, 79 environment variables documented; e2e 56
pass, 13 xfail, 0 fail. The builder runs `9956a02`, redeployed through
`update stage --from` with a run in flight, which was interrupted and
requeued as designed.
