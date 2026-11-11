# RustyKrab agent instructions

Read `CLAUDE.md` completely before changing this repository. Its build,
validation, architecture, and security instructions apply to every coding
agent, regardless of which product launched it.

## Useful agency and self-correction

For work on execution, context, scheduling, permissions, UI, dreaming or
evaluation, read [the owner's design direction](docs/architecture/08-agent-agency-and-self-correction.md).
Give agents coherent authority and relevant evidence to complete the original
goal. Remove artificial barriers within delegated scope, verify recovery on the
affected operation, and measure useful results rather than repair activity.
Record remaining gaps and failed hypotheses; a design target is not proof of
implemented behavior or a grant of additional authority.

## Verification is part of the work

A change is not done, and you do not say it is, until all of these exist. Do them as part
of the work, without being asked; Geoff should never have to request one (Geoff,
10 November 2026; the same rules as fkdoc's `AGENTS.md`):

1. **Your own verification, on the real thing.** Run the changed behavior on the actual
   daemon build, through the channel or surface a person uses (Telegram, Signal, WebChat,
   the CLI, a scheduled job), or through `scripts/e2e.sh` where that reaches it. Record the
   commit, the exact commands, what you observed before and after, and what you could not
   reach. Unit tests, clippy and a green CI run are not this.
2. **An independent run** for any change to what RustyKrab does (execution, context,
   scheduling, permissions, channels, storage, UI, dreaming, evaluation). Once your own
   verification is written and the head is final, start a fresh-context subagent that
   built none of the change. A separate session is fine too. The run counts only when:
   - it makes its own cases: brief it with the PR, the head commit and the original request,
     not your conclusions or your harness; it derives its scenarios from the claim and the
     diff and writes its own checks;
   - it reaches the same live layer, or says what it substituted and why;
   - the code is frozen while it runs; if you change code after it, it checks the change;
   - it writes its own report, which you never write, finish or edit. Post it on the PR as
     the subagent wrote it, marked `verifier: independent, operator: subagent of the builder`.
     Never present your own run as independent.
3. **Its findings fixed,** verified again and rechecked by it, or listed as open in the PR
   and in your report to Geoff, in its words.
4. **The pre-commit checks in `CLAUDE.md` passing.**

If one of these cannot be done, stop and tell Geoff which and why. Do not report the work
as finished or ask for review with one missing. Never touch the live instance, its data or
its credentials to verify: use a scratch home, port and state directory.

The PR description carries, under those headings:

- **Behavioral changes.** Every way RustyKrab now behaves differently for a person, an
  agent or an operator, written from the diff: before, after, where it is met (which
  channel, command, tool, job, table or config), whether it was asked for or is a side
  effect, and what shows it. Include changed defaults, timings, wording, errors and what no
  longer happens. "None" only with the reason.
- **Experience change.** `none`, or each change a person would see or feel in a channel,
  the web UI or the CLI, with a before and an after capture of comparable states (a
  screenshot, or the message text for a chat channel). The merge does not wait for Geoff's
  approval of it. The independent run checks both lists against the diff for omissions.
- **Verification.** Your record from step 1, the independent report, and what remains open.

## Architecture maintenance is required

Architecture write-ups are part of the implementation, not a later cleanup.
Before planning or editing a structural change:

1. Read `docs/architecture/README.md`, `00-system-overview.md`, and the
   `ARCHITECTURE.md` for every affected crate.
2. Read the relevant data-model, extension-seam, opinion, and outcome documents
   when the change touches those subjects.
3. Re-derive load-bearing claims against the exact base commit. The documents
   are versioned evidence, not timeless truth; if prose and code disagree,
   correct the prose in the same change.

Update the affected write-ups in the same PR whenever you add or remove a
crate, module, dependency, trait, implementation, table, index, foreign key,
execution path, runtime responsibility, or other documented boundary. A new
crate always needs its own `ARCHITECTURE.md`. When a change resolves a finding
in `docs/architecture/OPINION.md`, move the finding to the outcome/history
document instead of erasing it.

Before committing, regenerate and verify the mechanical facts:

```sh
python3 scripts/check_architecture_docs.py --fix
python3 scripts/check_architecture_docs.py
cargo fmt --all -- --check
cargo clippy --workspace --all-targets
cargo test --workspace
```

Never hand-edit content between `generated-metrics` markers. The checker guards
counts, dependencies, crate coverage, and the presence of these instructions;
the agent remains responsible for the semantic truth of the prose. In the PR
or handoff evidence, name the architecture documents updated, or explicitly
state why the change does not affect documented architecture.
