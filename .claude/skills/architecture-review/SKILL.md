---
name: architecture-review
description: Verify docs/architecture and crates/*/ARCHITECTURE.md still describe the code, and re-run the structural review. Use after landing structural work, before merging a refactor, or when the docs are suspected stale.
argument-hint: "[check|verify|full]"
allowed-tools: Bash, Read, Grep, Glob, Edit, Write
---

# Architecture Review

You are auditing whether this workspace's architecture documents still tell
the truth, and — at `full` — re-running the structural review that produced
them.

Two kinds of claim live in those documents, and they fail differently:

- **Facts** — counts, dependencies, which crate a trait lives in. These are
  guarded by `scripts/check_architecture_docs.py` and CI.
- **Arguments** — "this duplication is a risk", "this abstraction does not
  earn its keep", "these tables should be one". Nothing can check these
  mechanically. **That is what you are for.**

An argument that has quietly become false is worse than no document, because
a reader trusts it. Your job is to find those.

## Modes

`$ARGUMENTS` selects depth. Default to `verify` when nothing is given.

| Mode | Does |
|---|---|
| `check` | Mechanical only — run the script, report, stop |
| `verify` | Re-verify every load-bearing claim in the docs against the code |
| `full` | `verify`, plus hunt for structural problems the docs do not mention |

---

## Step 1 — mechanical (all modes)

```sh
python3 scripts/check_architecture_docs.py
```

Failures here are unambiguous: regenerate with `--fix` and note what drifted.
If this passes, the *facts* are current. It says nothing about the arguments.

Stop here for `check`.

---

## Step 2 — verify the claims (`verify` and `full`)

The documents assert specific, checkable things. Re-measure each. **Do not
trust the number written in the doc — derive it, then compare.** The point is
to catch the case where the code moved and the sentence did not.

Read `docs/architecture/OPINION.md` and `02-extension-seams.md` first; they
carry most of the load-bearing claims. For each, establish whether it is
**still true**, **now resolved**, or **now wrong in a new way**.

Recurring claims and how to measure them:

**Duplication.** The docs claim specific things are written N times. Verify by
normalising whitespace and diffing, not by eye:

```sh
sed -n '<start>,<end>p' <file> | sed 's/^[ \t]*//' > /tmp/a.txt
sed -n '<start>,<end>p' <file> | sed 's/^[ \t]*//' > /tmp/b.txt
diff /tmp/a.txt /tmp/b.txt | grep -c '^[<>]'
```

For "the turn sequence appears N times", count a marker that is hard to fake:

```sh
grep -rn "persisted_ids" crates --include="*.rs" | grep -v "^.*://"
```

**Trait implementors.** Claims like "one implementor" or "dead":

```sh
for t in Tool ModelProvider Channel MemoryBackend GatewayBackend Skill; do
  printf "%-18s %s\n" "$t" "$(grep -rho "impl $t for" crates --include='*.rs' | wc -l)"
done
```

**Dead code.** Something claimed dead may have been wired up:

```sh
grep -rn "ConsistencyVoter\|GatewayTool\|automation_tools" crates --include="*.rs"
```

**Ambient configuration.** The docs count env reads outside the composition
root:

```sh
for d in crates/*/; do
  printf "%-24s %s\n" "$(basename $d)" \
    "$(grep -rho 'env::var[_os]*("[A-Za-z_0-9]*"' $d/src 2>/dev/null | wc -l)"
done
```

**Schema.** Tables, and which references are enforced:

```sh
grep -rh "CREATE TABLE" crates --include="*.rs" \
  | sed -E 's/.*CREATE TABLE (IF NOT EXISTS )?([a-zA-Z_0-9]+).*/\2/' | sort -u
grep -c "REFERENCES" crates/rustykrab-store/src/lib.rs
```

Distinguish **missing** from **deliberately unenforced**. This schema
documents provenance columns as intentionally lacking foreign keys; flagging
those as defects is a false positive, and the DDL comment says so. Read the
comment before reporting.

---

## Step 3 — hunt for what is not documented (`full` only)

Everything above checks claims that exist. This looks for what is missing.

1. **New structure.** Crates, tables, traits, or top-level modules added since
   the docs were written and not mentioned in them.
2. **Files that outgrew their description.** Compare the generated line counts
   against what the prose says about each file's role. A file described as
   "one job" that has doubled usually now has two.
3. **New duplication.** Two functions with near-identical bodies; a constant
   spelled out in several places; the same sequence of calls at several sites.
   The last of these has produced real bugs here, where a fix landed in one
   copy and not the others.
4. **Abstractions with one implementor.** A trait with a single implementor is
   not automatically wrong, but it is worth asking whether it is describing a
   seam or advertising one that does not exist.
5. **Comments that contradict the code.** These are the highest-value find in
   this codebase, because its comments are unusually reliable and therefore
   unusually trusted. A stale one is load-bearing in the wrong direction.

---

## Step 4 — report, and update

Write the findings back. The documents are the deliverable, not your message.

- **Claim still true** — leave it.
- **Claim resolved** — move it to `docs/architecture/05-first-pass-outcome.md`
  with what closed it. **Do not delete it.** The record of what was wrong,
  and of what a review got wrong, is the most reusable part of these
  documents.
- **Claim now false** — correct the sentence in place. If the original claim
  was overstated rather than merely outdated, say so explicitly and leave a
  marker; a review that silently revises itself cannot be calibrated.
- **New finding** — add it to `OPINION.md` with a measurement attached, not an
  adjective.

Then re-run `python3 scripts/check_architecture_docs.py --fix` and include the
regenerated metrics in the same change.

---

## How to be right about severity

The first pass of this review overstated severity three times out of eleven,
and every one was caught by *implementing* the fix rather than by re-reading
the code. Static analysis is good at locating things and bad at judging how
much they matter.

So:

- **Attach a measurement to every claim.** "82% identical, 167 of 933 lines"
  survives review; "significant duplication" does not.
- **Prefer evidence over argument.** The strongest finding in the first pass
  was confirmed not by reasoning but by watching an unrelated change land in
  both copies of a duplicated loop. Look for costs already paid.
- **Check whether a suspicious thing is commented.** Repeatedly here, what
  looked like a defect had a comment explaining why it was deliberate —
  the empty-string sentinel, the non-back-filled version columns, the
  unenforced provenance columns, the drain that must not happen on app
  surfaces. Read before reporting.
- **State confidence, and mark taste as taste.** Distinguish "this is a bug,
  here is the failure path" from "I would structure this differently".
- **Do not report a finding you have not located in the code.** A file and
  line, or it does not go in.
