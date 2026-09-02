# Architecture & Reusability Evaluation

A structural review of the RustyKrab workspace. Second pass, against `main` at
`fd1f1e2` — roughly 84k lines across 13 crates, 921 tests. The first pass ran
against `d945495`; what it changed is recorded in
[`05-first-pass-outcome.md`](05-first-pass-outcome.md).

The review is deliberately split into **description** and **judgement**, because
they age differently. The description is meant to stay true as long as the shape
of the code holds; the judgement is one reviewer's opinion at one point in time.

## Descriptive (what the system is)

| Document | Covers |
|---|---|
| [`00-system-overview.md`](00-system-overview.md) | Crate graph, layering, runtime topology, the path of a message |
| [`01-data-model.md`](01-data-model.md) | Every table in both databases, keys, relationships, and where joins are missing |
| [`02-extension-seams.md`](02-extension-seams.md) | Every trait/abstraction in the system, who implements it, and whether it earns its keep |
| [`03-dead-code-audit.md`](03-dead-code-audit.md) | The unreferenced items, what each is for, and whether to wire or delete it |
| [`05-first-pass-outcome.md`](05-first-pass-outcome.md) | Which first-pass findings were acted on, and which were wrong |

Per-component detail lives next to the code, one file per crate:

- [`crates/rustykrab-core/ARCHITECTURE.md`](../../crates/rustykrab-core/ARCHITECTURE.md)
- [`crates/rustykrab-store/ARCHITECTURE.md`](../../crates/rustykrab-store/ARCHITECTURE.md)
- [`crates/rustykrab-memory/ARCHITECTURE.md`](../../crates/rustykrab-memory/ARCHITECTURE.md)
- [`crates/rustykrab-agent/ARCHITECTURE.md`](../../crates/rustykrab-agent/ARCHITECTURE.md)
- [`crates/rustykrab-providers/ARCHITECTURE.md`](../../crates/rustykrab-providers/ARCHITECTURE.md)
- [`crates/rustykrab-tools/ARCHITECTURE.md`](../../crates/rustykrab-tools/ARCHITECTURE.md)
- [`crates/rustykrab-gateway/ARCHITECTURE.md`](../../crates/rustykrab-gateway/ARCHITECTURE.md)
- [`crates/rustykrab-channels/ARCHITECTURE.md`](../../crates/rustykrab-channels/ARCHITECTURE.md)
- [`crates/rustykrab-skills/ARCHITECTURE.md`](../../crates/rustykrab-skills/ARCHITECTURE.md)
- [`crates/rustykrab-dream/ARCHITECTURE.md`](../../crates/rustykrab-dream/ARCHITECTURE.md)
- [`crates/rustykrab-cli/ARCHITECTURE.md`](../../crates/rustykrab-cli/ARCHITECTURE.md)
- [`crates/rustykrab-e2e/ARCHITECTURE.md`](../../crates/rustykrab-e2e/ARCHITECTURE.md)

## Judgement (whether it is right and sensible)

| Document | Covers |
|---|---|
| [`OPINION.md`](OPINION.md) | Ranked verdicts on correctness and sensibility, with the reasoning behind each |

## Keeping these true

The numbers here are generated, not written. `scripts/check_architecture_docs.py`
regenerates the `generated-metrics` block in each crate's `ARCHITECTURE.md`,
verifies every workspace member has one, and checks the crate count asserted
in prose. CI runs it on every pull request, so a structural change that
leaves these documents stale fails the build.

That check exists because the alternative was already demonstrated: before it
was added, `CLAUDE.md` claimed 11 crates and `README.md` claimed 10, when
there were 13. Both were written carefully. Nothing verified them.

What it does not check is the prose — a reviewer's reasoning cannot be
verified mechanically, and pretending otherwise would add false confidence.
`CLAUDE.md` tells agents to update the argument when they invalidate it; this
guards the facts.

```sh
python3 scripts/check_architecture_docs.py --fix   # regenerate
python3 scripts/check_architecture_docs.py         # verify (what CI runs)
```

## Method

Read-only static review: crate graph, module and function inventories, all DDL,
all delete paths, trait-implementor counts, duplication measurement by
normalised diff, and env-var reads per crate. No code was changed and no
benchmark was run. Where a claim is a measurement it is stated as one; where it
is taste it is marked as taste.
