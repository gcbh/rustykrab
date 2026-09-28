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
