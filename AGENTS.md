# RustyKrab agent instructions

Read `CLAUDE.md` completely before changing this repository. Its build,
validation, architecture, and security instructions apply to every coding
agent, regardless of which product launched it.

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
