# Confined worktree access — 2026-10-22

Base: `7a286d938eacc28ca696bb4b17ef1800aa6fca11` (v6.6.7).

## Observed failure and reproduction

RustyKrab recovery work `6ef846e5-c89d-4048-bbf2-5c1785aff94a` started on
`planner` in a real linked worktree under the private daemon data directory.
Its recorded run made 105 exec calls; 98 results included “Operation not
permitted” and 95 included “Unable to read current working directory”. It
stopped after 100 iterations with 93 repeated steps without new evidence.
Creating the directory on the host had not established effective worker access.
The run produced no verified commit or original-task recovery.

A deterministic macOS test builds a real repository and linked worktree inside
a private temporary data directory, then uses the production `ExecTool` and
Seatbelt profile. Before the fix, reading a file inside the worktree succeeds,
but `git status --porcelain` fails with exit 128: an ancestor path is inaccessible.
The older private-file regression tested only `cat`, so it did not detect this.
The new test is `a_confined_worktree_can_resolve_its_cwd_inside_private_data`.

## Narrow repair

The profile continues to deny reads of private data, and permits reads/writes
only under its existing worktree/common-Git exceptions. It now permits
`file-read-metadata` on the exact literal ancestors of those directories.
Git can resolve the path without granting access to the ancestors’ directory
entries or other file contents. Ancestors are canonical and deduplicated.
The existing outside-write, hooks/config, forced-removal, Docker and loopback
restrictions remain in force.

The regression checks physical working-directory resolution, an absolute `cd`,
Git status and repository discovery, and staging/committing a real change. It
also checks that the private service-file sentinel cannot be read and that the
private data directory cannot be listed.

## Validation and limits

The exact new regression fails before the policy change and passes after it.
It runs outside the invoking Codex sandbox because nested `sandbox-exec` fails
at `sandbox_apply` before the tested policy is applied; that environment failure
is retained separately from the actual Git-path regression.

Required formatting and architecture/environment checks pass. Strict
`cargo clippy --offline --workspace --all-targets -- -D warnings` passes.
`cargo test --offline --workspace -- --test-threads=1` passes: 2458 passed,
0 failed, 39 ignored. No model is used by these
regressions. Architecture review is the skill’s mechanical `check` mode plus
selected verification of this execution boundary; it is not a fresh full-repo
structural audit.

This restores a tested executor operation. It does not implement shared
merge-conflict repair, descendant-aware continuation, early loop handoff or
live operation probes. The independent failing continuation fixture is retained
in the task artifacts, not weakened or included as a failing access-fix test.
The live repair remains budget-held; this change grants no budget, answers no
question and does not deploy itself. Original-goal recovery requires the fixed
build to be deployed through the authorized path and the affected operation
and tasks to be exercised again.
