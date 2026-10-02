# Plan: Adaptive Repository Verification Skills

**Status:** Proposed
**Date:** 2026-09-01

## 1. Decision

Repository-specific verification strategy is implemented as versioned skills.
A small explicit-code kernel owns evidence integrity, capability enforcement,
state transitions, and the final verification verdict.

The governing distinction is:

> Verification methods are skills. Verification authority is code.

This gives each repository and deployment setup a verifier that can understand
its architecture, develop better tests, and improve from observed outcomes
without allowing learned instructions to redefine trustworthy proof.

## 2. Why verification belongs partly in skills

Useful software verification is not a fixed list of commands. It requires
judgment that varies by repository, component, risk, and environment:

- deciding which behavior is most likely to be missing;
- understanding repository-specific architecture and invariants;
- designing adversarial, property-based, migration, compatibility, or journey
  tests;
- recognizing tests that merely bless an incorrect implementation;
- selecting relevant checks without running every expensive suite;
- interpreting failures and assigning them to the right stack layer; and
- learning from escaped defects, false positives, and recurring repair loops.

Encoding all of that in Rust would make repository knowledge slow to update and
would turn the controller into a growing collection of special cases. Skills
are versioned instructions, can be scoped to a repository or setup, and can be
evaluated and improved independently of the RustyKrab binary.

## 3. What remains explicit code

The verification kernel owns every claim or action that must remain true even
when a model or skill is wrong.

### 3.1 Evidence integrity

- exact base, parent, and head SHAs;
- repository, worktree, toolchain, environment, and skill-pack fingerprints;
- command arguments, sandbox policy, start/end time, status, and output digest;
- produced artifact hashes and retention;
- acceptance-criterion and finding identifiers;
- evidence freshness and invalidation after any relevant change; and
- separation of deterministic, explicit, implicit, and judge evidence.

### 3.2 Authority and isolation

- verifier capabilities and tool allowlists;
- command, network, filesystem, credential, and environment restrictions;
- no push, merge, deployment, or policy-edit authority for verifier contexts;
- repository mutation only in controller-owned worktrees; and
- typed, idempotent external operations.

### 3.3 Non-negotiable gates

- clean and linear Git ancestry;
- forbidden-path and scope enforcement;
- branch-protection and required-CI conclusions;
- repository-configured mandatory checks;
- unresolved blocking findings;
- exact-SHA evidence matching;
- frozen acceptance and authority snapshots; and
- valid delivery-state transitions.

### 3.4 Verdict

Skills propose checks, produce analysis, and report findings. The controller
decides whether required evidence exists and is fresh enough to transition a
layer to `verified`. A skill cannot set or clear that state directly.

## 4. Verification layers

Every layer is evaluated through four cooperating sources:

| Source | Form | Can be changed by repository? | Can authorize `verified` alone? |
|---|---|---:|---:|
| Platform invariants | Rust code | no | no |
| Repository mandatory checks | trusted repository policy and CI | yes, next run | no |
| Verification skill pack | versioned `SKILL.md` instructions | yes, next run | no |
| Acceptance evidence | tests, artifacts, inspections, runtime probes | generated | no |

The coded verdict requires their valid intersection. No single model judgment,
test command, or skill response is sufficient by itself.

## 5. Repository verification skill packs

A repository may own a verification pack at a trusted path such as:

```text
.rustykrab/verification/
  repository-verifier/SKILL.md
  architecture-review/SKILL.md
  rust-workspace/SKILL.md
  api-contracts/SKILL.md
  database-migrations/SKILL.md
  deployment/SKILL.md
```

A setup may add signed overlays for facts that do not belong in the repository,
such as a particular staging environment, supported operating system, or
hardware integration. The effective pack is the intersection or union defined
by policy; an overlay can add checks but cannot remove a repository or platform
requirement.

Start with one repository-orchestrator skill and a small number of coherent
specialists. Do not create a skill per test command. Split a skill when it owns a
distinct outcome, evidence source, and improvement corpus.

### 5.1 Example skill responsibilities

**Repository verifier**

- maps changed components and acceptance behavior to applicable specialist
  skills;
- identifies cross-component and stack-level risks;
- proposes the verification plan; and
- checks that every acceptance behavior has an evidence path.

**Architecture reviewer**

- loads system and affected-component architecture write-ups from the trusted
  base revision and records their content hashes;
- runs the repository's mechanical architecture checker when one exists;
- re-derives load-bearing prose claims from code instead of trusting stale
  counts or conclusions;
- distinguishes descriptive facts, review opinions, and mandatory repository
  policy; and
- requires structural changes to update the relevant write-ups without
  treating documentation consistency as functional proof.

**Rust workspace verifier**

- selects affected crates and cumulative workspace gates;
- checks lint, feature, target, dependency-lock, and generated-file behavior;
- recommends targeted property or regression tests; and
- recognizes test-only changes that weaken behavior.

**Migration verifier**

- inspects forward, backward, restart, and partial-rollout compatibility;
- creates disposable old/new database fixtures;
- verifies rollback and mixed-version expectations; and
- reports destructive or irreversible transitions as high risk.

**Deployment verifier**

- maps source SHA to artifact and running identity;
- exercises repository-defined rollout and health behavior;
- checks rollback evidence; and
- remains separate from the deployment credentials and driver.

## 6. Verification-skill contract

The existing `SKILL.md` outcome declaration remains the top-level optimization
setpoint. Verification skills additionally need a typed contract understood by
the verification runtime. A future frontmatter extension may include:

```toml
[outcome]
success = "Material defects in affected Rust workspace behavior are found before merge without rejecting correct changes."
checks = ["verification.defect_detected", "verification.correct_change_passed"]
signal = "verifiable"

[verification]
scope = ["rust", "cargo-workspace"]
evidence = ["command", "test-report", "diff-inspection", "mutation"]
may_generate_tests = true
may_write_repository = false
max_minutes = 20
```

The exact serialization is secondary. The runtime contract must identify:

- applicability and exclusions;
- required tools and evidence types;
- outcome and trusted signal class;
- cost and time bounds;
- whether ephemeral test generation is allowed;
- whether the skill is mandatory, advisory, or experimental; and
- stable skill identity, version, content hash, and publisher provenance.

The skill body explains how to reason and use tools. It does not grant those
tools; session capability code remains authoritative.

## 7. Per-layer execution protocol

For each exact layer head:

1. The controller loads the verification pack pinned from the trusted base.
2. It loads base-pinned architecture write-ups for affected components and
   records any freshness conflict with the code.
3. Deterministic applicability rules plus the repository-verifier skill select
   relevant specialists.
4. The controller records the selected skill identities and hashes.
5. A fresh verifier context receives the frozen acceptance slice, incremental
   and cumulative diffs, repository observations, and read-only evidence tools.
6. Skills propose checks and may generate ephemeral tests in a disposable
   verifier worktree.
7. The coded runner validates and executes permitted checks.
8. Skills analyze structured results and emit typed findings with source,
   severity, affected criterion, responsible layer, confidence, and evidence.
9. The controller independently evaluates hard gates and evidence coverage.
10. Failure creates repair work for the implementation layer; the verifier does
   not commit production changes.
11. Repair invalidates affected evidence and repeats the protocol on the new
    exact SHA.

Useful generated tests should be proposed for the repository through the
normal implementation and review path. Ephemeral verifier tests are evidence,
not hidden permanent source changes.

## 8. Pinning and self-verification safety

A change must never weaken the verifier used to judge that same change.

- Verification skills are resolved and content-hashed from the trusted base
  revision before implementation starts.
- Candidate-branch edits to verification skills are ignored for the candidate's
  eligibility and take effect only after merge and probation.
- A verification-skill change is evaluated by the prior trusted skill pack plus
  fixed platform invariants.
- During probation, the effective required set is the union of the previous and
  candidate pack where either can catch a regression.
- A candidate cannot edit the outcome definition, trusted signal, held-out
  corpus, and skill instructions in one self-validating change.
- Removing or weakening a mandatory check is a separately classified policy
  change, never an ordinary learned delta.
- Skill-pack hashes are part of delivery, verification, deployment, and runtime
  fingerprints.

## 9. Dreaming improvement loop

Adaptive verification is an ideal consumer of the existing Dreaming outer-loop
design because verifier executions produce unusually strong outcome signals.

### 9.1 Monitor

Record for each verification-skill use:

- repository, base SHA, change classification, and affected components;
- exact skill identity, version, content hash, and learned-strategy entries;
- proposed and executed checks;
- findings, repairs, and final disposition;
- later CI, review, deployment, rollback, and assurance outcomes;
- escaped defects attributable to behavior the skill should have checked;
- false-positive findings overturned by stronger evidence; and
- cost, duration, flake, and retry counts.

### 9.2 Analyze

DreamWorker remains deterministic and read-only while it identifies:

- repeated defect classes the skill misses;
- checks that never contribute evidence;
- false-positive patterns;
- expensive or flaky strategies;
- repair cycles caused by unclear findings;
- repository evolution that makes instructions stale; and
- sufficient ground-truth evidence to permit an improvement proposal.

### 9.3 Propose

The mutation stage creates a small delta, preferably in a loop-managed
`Learned strategies` section rather than rewriting human-authored instructions.
Every proposal states:

- observed failure or opportunity;
- affected stable strategy entries;
- evidence and counterexamples;
- expected improvement;
- risks and rollback condition; and
- the evaluation suite that may falsify it.

### 9.4 Validate

Before a PR is eligible, the candidate skill is evaluated against a held-out
corpus containing:

- historical escaped defects;
- historical correct changes;
- seeded omissions and mutation tests;
- weakened or fake tests;
- repository-specific edge cases;
- changes outside the skill's scope; and
- adversarial attempts to alter the verifier or its measurement.

Evaluation measures at least defect recall, false-positive rate, escaped-defect
rate, cost, latency, and flake. Optimizing only defect recall would reward an
always-rejecting verifier, so no single metric may authorize promotion.

### 9.5 Publish and promote

The proposal becomes a normal GitHub PR or native stack against the repository
or signed setup-skill source. It carries old-versus-new replay evidence.

Promotion tiers are:

1. **Counter-only:** update evidence counters on existing learned entries.
2. **Shadow:** run new instructions without affecting eligibility.
3. **Additive:** add a strategy or advisory check with strong ground truth.
4. **Required:** make a proven strategy eligibility-affecting after probation.
5. **Weakening/removal:** always treated as a policy change with the highest
   configured review tier.

Autonomous merge may be allowed for low-risk additive changes when repository
policy requires held-out improvement, no hard-gate regression, successful
shadow comparison, and an automatic rollback path. Judge-only or proxy-only
improvement remains proposal-only.

### 9.6 Probation and rollback

After promotion:

- old and new skill versions run together for a bounded sample window;
- outcome attribution remains version-specific;
- regression thresholds use hysteresis and minimum samples;
- the candidate returns to shadow or is rolled back on worse ground-truth
  outcomes; and
- a silent verifier or missing attribution blocks promotion rather than
  implying success.

## 10. Data model additions

```text
verification_skill_snapshots
  id, repository, base_sha, source, skill_id, version, content_hash,
  publisher, contract, status, created_at

verification_skill_selections
  id, run_id, layer_id, generation, skill_snapshot_id, reason,
  mandatory, selected_at

verification_skill_uses
  id, selection_id, attempt_id, verifier_fingerprint, proposed_checks,
  findings, cost, started_at, finished_at

verification_outcomes
  id, skill_use_id, signal, verdict, source_kind, source_id,
  observed_at

verification_skill_proposals
  id, skill_snapshot_id, dream_cycle_id, delta, evidence, status,
  branch, pr_url, created_at

verification_skill_experiments
  id, proposal_id, corpus_hash, baseline_metrics, candidate_metrics,
  shadow_metrics, decision, created_at
```

General delivery evidence remains in the delivery evidence store. These tables
record attribution and improvement state rather than duplicating artifacts.

## 11. Required scenarios

1. A repository-specific skill selects a migration test that generic commands
   would not discover.
2. A skill-generated ephemeral test catches a seeded omission and its artifact
   is tied to the exact head SHA.
3. A skill reports success but missing hard-gate evidence prevents `verified`.
4. A candidate edits its verification skill; the base version still judges the
   candidate and the edit applies only to future work.
5. A skill-pack update cannot remove a platform or repository mandatory check.
6. A lower-layer repair changes SHAs and invalidates every affected skill run.
7. DreamWorker identifies a repeated escaped-defect class from ground-truth
   outcomes and creates one bounded proposal.
8. Held-out replay rejects a candidate that catches more defects by rejecting
   every correct change.
9. Proxy-only improvement cannot become eligibility-affecting.
10. A proven additive strategy enters shadow, passes probation, and promotes.
11. Ground-truth regression during probation restores the prior skill version.
12. Verification skill, model, toolchain, or repository drift creates a new
    fingerprint and comparison cohort.

## 12. Construction impact

This decision changes three construction areas:

1. The deterministic local-delivery stack builds the coded evidence kernel,
   sandboxed runner, exact-SHA invalidation, and skill-snapshot persistence.
2. The autonomous-verification stack builds repository verification packs,
   skill selection, fresh verifier contexts, generated-test isolation, and
   structured findings.
3. A later adaptive-verification stack builds Dreaming attribution, read-only
   analysis, delta proposals, held-out replay, PR publication, shadow execution,
   probation, and rollback.

The first adaptive milestone is:

> A base-pinned repository verification skill catches a seeded defect, records
> versioned evidence, later misses a different ground-truth defect, and causes
> Dreaming to create a small skill-improvement PR that passes held-out replay,
> improves in shadow mode, and promotes without weakening any existing gate.
