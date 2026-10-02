# Construction Plan: Autonomous Software Delivery

Status: proposed construction sequence
Companion architecture: `docs/plans/autonomous-software-delivery.md`
Planning model: `docs/plans/conversational-project-planning.md`
Verification model: `docs/plans/adaptive-verification-skills.md`

## Outcome

RustyKrab will maintain an ongoing project-planning conversation and turn ready,
authorized slices of that evolving plan into small, functional, natively
stacked GitHub pull requests. It will independently implement, verify, repair,
publish, merge, deploy, and continuously assess those slices within explicit
repository policy.

The finished system has one durable control loop:

```text
conversation <-> evolving project model
  -> propose and authorize one ready slice
  -> compile internal delivery manifest
  -> implement layer
  -> independently verify exact commit
  -> submit native GitHub stack
  -> observe GitHub checks
  -> merge eligible prefix
  -> execute repository deployment contract
  -> verify deployed behavior continuously
  -> remain healthy, roll back, quarantine, or open a remediation stack
```

## Construction strategy

This program is built as a **stack of stacks**, not as one very long PR stack.

- Each construction milestone is a native `gh stack` of three to five PRs.
- Every PR leaves the workspace buildable and adds a testable capability.
- A construction stack is merged before the next stack is based on `main`.
- Every merged stack ends with a black-box demonstration, not only unit tests.
- Later authority is dormant behind policy until the preceding read-only path is
  proven.
- Evidence always names the exact source, tree, artifact, and deployed revision.
- A failed or stale proof cannot authorize the next transition.
- Rollback boundaries align with merged construction stacks.

The current worktree contains unrelated in-progress changes. Construction must
therefore begin from a clean worktree based on the intended integration commit;
the existing worktree and its uncommitted changes remain untouched.

## Architecture evidence contract

Repository architecture write-ups are required planning and delivery inputs,
but they are versioned evidence rather than timeless truth. At the start of a
delivery, the controller loads `docs/architecture/`, the `ARCHITECTURE.md` files
for affected crates, and any repository architecture-review checker or skill
from the exact frozen base commit. It records their paths, content hashes, and
base SHA with the repository observations used to authorize the slice.

The orchestration agent and verifier must reconcile those documents with the
code. A contradiction creates a freshness finding; the code at the frozen base
wins until the document is corrected. Opinion and recommendation documents
remain design evidence, not mandatory policy, unless repository policy promotes
a claim into an explicit gate.

Every structural layer updates the affected architecture write-ups in the same
PR, including generated metrics and outcome/history records required by the
repository. Architecture checks prove that the description stayed coherent;
they never replace functional tests, database evidence, or runtime probes.

## Common PR contract

Every PR in every stack must include:

1. One concise functional promise in its title and body.
2. The dependency on its immediate parent PR, if any.
3. Automated tests for the capability it introduces.
4. A deterministic local verification command.
5. A fixture, trace, or artifact showing the observable result.
6. An explicit statement of what authority remains disabled.
7. No unrelated refactor or formatting churn.
8. Architecture-document updates, or an evidence-backed statement that the
   layer does not change a documented boundary, dependency, schema, or flow.
9. Hosted CI configured to run against that stacked PR's exact head even when
   its base is another feature branch rather than `main`.

Before a PR is submitted, the repository gates remain:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets
cargo test --workspace
```

Each stack also has a stack-specific end-to-end gate. A layer is ready to submit
only when its own gate and all inherited gates pass against the exact head SHA.

## Construction stack 0: Durable project planning model

**Goal:** establish long-lived project understanding before introducing
autonomous execution or asking the user for a formal specification.

### PR 0.1 — Add the conversational-planning scenario harness

- Land the architecture, conversational-planning, and construction plans.
- Add fixture-repository support to `rustykrab-e2e`.
- Add planning and delivery scenarios as explicit expected failures.
- Make unexpected passes fail so scenarios cannot silently drift.
- Add conversation fixtures beginning with intentionally vague project ideas.
- Make every promoted scenario exercise exactly the lifecycle named by its
  identifier; snapshot reconstruction, daemon restart, and model compaction are
  separate proofs and must not stand in for one another.

Functional proof: the harness reports every unimplemented planning behavior by
stable scenario identifier and can replay a multi-turn project conversation.

### PR 0.2 — Add the pure project-planning domain crate

Create `rustykrab-projects` with no model, network, process, or Axum dependency:

- projects and immutable project revisions;
- typed plan nodes and relationship edges;
- provenance, confidence, freshness, and supersession;
- decision, question, assumption, risk, milestone, and outcome rules; and
- transactional `PlanChangeSet` validation.
- Add its required `ARCHITECTURE.md` and update the workspace architecture
  index against the exact integration base.

Functional proof: a sequence of changes creates deterministic revisions;
corrections supersede history, invalid edges fail, and source provenance remains
traceable.

### PR 0.3 — Persist projects, revisions, and provenance

- Add idempotent SQLite migrations and project store modules.
- Link project revisions to existing conversation messages without duplicating
  transcript text.
- Persist questions, decisions, repository observations, and judgment policy.
- Add restart, concurrency, and idempotent replay tests.
- Enforce same-project identity across current revisions, parent revisions,
  materialized nodes, and edges with composite constraints and
  `PRAGMA foreign_key_check` coverage.
- Define whether project provenance outlives conversation deletion, expose a
  stable persisted message reference, and prove the chosen cascade, restrict,
  or retention behavior with a real stored conversation and message.
- Update the store and data-model architecture write-ups, including explicit
  ownership, foreign-key, cascade, and deliberately-unenforced provenance
  decisions for every new reference.

Functional proof: project state, open questions, decisions, and their sources
reconstruct identically after store reopen.

### PR 0.4 — Derive project projections and expose them through the API

- Add deterministic brief, roadmap, decision-log, question, risk, architecture,
  behavior-catalog, and current-understanding projections.
- Add project create, inspect, revision, compare, and projection endpoints.
- Accept external request DTOs rather than domain change sets: the runtime
  derives actor and time, and only a verified repository-observation service
  may mint observed provenance classifications.
- Ensure all projections name their source project revision.
- Keep projection semantics canonical in the domain, or explicitly version a
  transport adapter; gateway handlers must not invent independent scope rules.
- Promote projection consistency and restart scenarios.
- Update the gateway and E2E architecture write-ups for the new transport and
  black-box evidence paths.

Functional proof: multiple views generated from one revision agree about scope,
decisions, and milestone state; comparing two revisions explains the material
delta without erasing history.

**Merge gate:** a vague fixture project becomes durable, inspectable project
state and survives restart. No model invocation, code mutation, GitHub mutation,
merge, or deployment authority exists yet.

## Construction stack 1: Planning runtime and orchestration agent

**Goal:** make the ordinary conversation the planning interface and connect it
to the durable project model.

### PR 1.1 — Make `rustykrab-runtime` own the canonical turn lifecycle

- Extend the existing runtime crate from agent preparation into the shared
  load, append, run, heartbeat, persist, outcome, and reply lifecycle.
- Preserve the intentional distinction that chat surfaces drain pending
  credential links while app surfaces expose credential requests through their
  own UI and must not persist capture links into transcripts.
- Add direct runtime tests before moving more call-site behavior into it.
- Preserve current streaming and non-streaming semantics through thin adapters.

### PR 1.2 — Route all turn surfaces through the runtime

- Make HTTP, SSE, delegated tasks, Telegram, Signal, WebChat, and CLI invoke the
  same application service.
- Centralize cancellation, outcome capture, and trace identifiers.
- Add cross-surface conformance tests.

### PR 1.3 — Add one canonical planning conversation per project

- Bind a project to a durable orchestration-agent conversation.
- Rehydrate planning context from project state after compaction or restart.
- Convert each turn into a proposed `PlanChangeSet` with provenance.
- Separate user statements, observations, inferences, and recommendations.

### PR 1.4 — Add repository observation and planning research

- Give the orchestration agent read-only repository inspection.
- Store revision-bound repository observations and freshness policy.
- Challenge assumptions when source evidence disagrees.
- Continue safe research while a user-facing question remains open.

### PR 1.5 — Add decisions, delegated judgment, readiness, and slice proposals

- Surface material plan revisions conversationally before settling them.
- Ask the highest-leverage question rather than performing a form interview.
- Record delegated reversible decisions with their authority basis.
- Assess readiness per candidate slice, not for the whole project.
- Present the next slice in ordinary language for authorization or revision.

**Merge gate:** starting from a vague request, the orchestration agent inspects
the fixture repository, develops options, records a consequential user decision
and a delegated reversible decision, survives restart and compaction, leaves a
future question open, and proposes one traceable slice without exposing YAML.

## Construction stack 2: Executable delivery contract

**Goal:** turn only an authorized conversational slice into deterministic,
durable controller state.

### PR 2.1 — Add the pure delivery domain crate

Create `rustykrab-delivery` with no network or process dependencies:

- immutable `DeliveryManifest`, `StackManifest`, and layer manifests;
- delivery, layer, verification, merge, and deployment state enums;
- validated state-transition functions and content hashes; and
- authority, risk, budget, and repository policy types.

Functional proof: an authorized slice compiles to a deterministic manifest;
illegal transitions, missing provenance, authority widening, and post-freeze
mutations are rejected.

### PR 2.2 — Persist delivery state, leases, evidence, and events

- Add stores for manifests, deliveries, layers, attempts, evidence, findings,
  and events.
- Add expiring controller leases and per-repository mutation locks.
- Make event appends and state transitions transactional.
- Add restart and duplicate-delivery tests.

### PR 2.3 — Connect slice authorization to manifest compilation

- Freeze the exact project revision, repository base, acceptance behaviors,
  assumptions, constraints, budgets, and authority snapshot.
- Reject material unresolved questions affecting the slice.
- Keep future planning conversation independent of the frozen delivery.
- Supersede rather than mutate a manifest when active scope changes.

### PR 2.4 — Run and reconcile a synthetic delivery

- Add delivery inspect, pause, resume, and cancel endpoints plus SSE events.
- Add a deterministic scripted worker used only by tests.
- Reconcile completion evidence, milestone progress, assumptions, and follow-up
  findings into a new project revision.
- Promote lifecycle, restart, cancellation, and reconciliation scenarios.

**Merge gate:** conversational authorization starts a synthetic three-layer
delivery, which survives restart and exposes complete history; later planning
cannot mutate it, and its result updates the project model exactly once.

## Construction stack 3: Deterministic local delivery

**Goal:** construct and verify real linear Git branches locally without using a
model or changing GitHub.

### PR 3.1 — Add confined workspace inspection

- Add a typed workspace backend for repository discovery and read-only inspection.
- Validate repository roots, base commits, remotes, clean-state requirements, and
  allowed path scopes.
- Record the base SHA and repository fingerprint in the manifest.

### PR 3.2 — Build native local stack branches in isolated worktrees

- Initialize the first layer with native `gh stack init` and create later layer
  branches with native `gh stack add`.
- Create one isolated worktree per registered stack layer.
- Enforce parent ancestry and allowed-path ownership.
- Commit scripted fixture changes with delivery metadata.
- Clean up only worktrees owned by the delivery.

### PR 3.3 — Verify exact commits and capture artifacts

- Add the coded verification manifest, sandboxed command runner, and
  base-pinned verification-skill snapshots.
- Record command, environment fingerprint, parent SHA, head SHA, result, duration,
  skill-pack fingerprint, and artifact hashes.
- Invalidate evidence after any tree or ancestry change.
- Classify deterministic failure, infrastructure failure, timeout, and flake.

### PR 3.4 — Repair and resume a local stack

- Add bounded attempt state and repair scheduling.
- Resume after controller termination from durable state.
- Prove that a seeded omission is repaired on the owning layer.
- Reject fixes placed only in a descendant layer.

**Merge gate:** a structured manifest produces three locally verified functional
branches; a forced restart and seeded defect are both recovered automatically.

## Construction stack 4: Autonomous implementation and verification

**Goal:** replace the scripted builder with bounded agent roles while preserving
the deterministic controller and independent verification.

### PR 4.1 — Decompose frozen slices into functional stack layers

- Add a delivery-decomposition role with structured output.
- Require each layer to have user-visible value, acceptance criteria, owned paths,
  risks, dependencies, and verification commands.
- Reject circular, oversized, unverifiable, or purely mechanical layer plans.

### PR 4.2 — Add scoped implementation workers

- Give each worker only its registered native-stack layer worktree, task context,
  permitted tools, and a stack-aware Bash profile.
- Allow ordinary local Git inspection, staging, and commits while denying raw
  remote publication and non-stack branch creation.
- Require structured completion with changed paths and claimed evidence.
- Keep controller ownership of commits, transitions, and authority.

### PR 4.3 — Add independent verification and review

- Add the repository-verifier skill and initial Rust, API, migration, and
  deployment specialist packs.
- Select applicable base-pinned skills in a separate verifier context with no
  reliance on the implementer's claim.
- Isolate skill-generated ephemeral tests in a disposable verifier worktree.
- Emit typed findings that name the responsible layer, exact SHA, skill hash,
  acceptance behavior, and evidence.

### PR 4.4 — Add bounded repair policy

- Route findings to the owning layer.
- Keep the coded kernel authoritative over mandatory evidence and `verified`.
- Enforce attempt, token, elapsed-time, and cost budgets.
- Distinguish `needs_repair`, `needs_replan`, `blocked`, and `failed`.
- Complete without user input when within pre-authorized policy.

**Merge gate:** an authorized conversational slice becomes a locally verified
three-layer stack, including one independently discovered and repaired defect,
without intermediate user supervision.

## Construction stack 5: Native GitHub PR stacks

**Goal:** publish, synchronize, verify, and merge through native `gh stack`
functionality without reimplementing GitHub's merge semantics.

### PR 5.1 — Enforce native stack execution profiles

- Detect `gh stack` availability and authentication.
- Add layer-executor, stack-coordinator, and merge-operator Bash profiles.
- Permit local Git work while denying raw push, `gh pr` mutation, generic
  `gh api` mutation, and destructive stack restructuring.
- Inject short-lived credentials only into an approved native stack subprocess.
- Record commands and post-command native state in a durable journal.

### PR 5.2 — Submit idempotent native stacks

- Have a stack-coordinator model initialize or resume the native stack.
- Require new layer branches to enter through `gh stack add` in manifest order.
- Submit through native `gh stack submit --auto --open` with stable PR metadata
  and delivery identifiers.
- Reconcile existing PRs after retry instead of duplicating them.

### PR 5.3 — Reconcile CI and cascading rebases

- Observe required GitHub checks for each exact head SHA.
- Have a stack-coordinator model synchronize native stack ancestry after an
  upstream repair.
- Invalidate descendant evidence after rebase.
- Re-run only the verification invalidated by the new commit graph.

### PR 5.4 — Merge the eligible prefix through GitHub

- Compute the largest prefix satisfying repository policy.
- Unlock a merge-operator model profile only for that prefix and require it to
  invoke native `gh stack merge`.
- Persist GitHub's merge results and recover from partial observations.
- Stop safely on policy, protection, review, or stale-evidence changes.

### PR 5.5 — Exercise a real fixture repository

- Provision a dedicated integration repository with branch protection and CI.
- Test submit, repair, cascading rebase, CI failure, interrupted observation, and
  successful prefix merge.
- Keep destructive fixture cleanup strictly scoped and auditable.

**Merge gate:** a local delivery becomes a native GitHub stack with verified PRs;
models cannot publish outside the stack path, and when merge authority is
enabled, GitHub merges only the eligible prefix.

## Construction stack 6: Repository-defined deployment

**Goal:** deploy the exact GitHub-merged revision through a contract owned by the
target repository.

### PR 6.1 — Parse and freeze the deployment contract

- Load `.rustykrab/delivery.toml` from the verified source revision.
- Validate driver, environment, required artifacts, health checks, rollout,
  rollback, and timeout policy.
- Freeze the contract hash before merge.
- Reject a deployment whose merged contract differs from the approved contract.

### PR 6.2 — Add durable deployment orchestration

- Create `rustykrab-deploy` with typed driver boundaries.
- Persist deployments, attempts, target revisions, external identifiers, evidence,
  and terminal outcomes.
- Add a fake driver for idempotency, timeout, and restart tests.

### PR 6.3 — Add a GitHub Actions deployment driver

- Dispatch the repository-defined workflow for the exact merged SHA.
- Track workflow and environment state.
- Verify artifact provenance before promotion.
- Distinguish GitHub, runner, application, and policy failures.

### PR 6.4 — Add rollout health and rollback

- Run contract-defined post-deploy checks.
- Support staged rollout and explicit probation.
- Trigger the repository-defined rollback path on terminal health failure.
- Report deployment and rollback evidence through API and events.

**Merge gate:** a merged fixture stack deploys the exact merged revision and
either becomes healthy or automatically returns to the last known-good revision.

## Construction stack 7: Continuous behavioral assurance

**Goal:** continuously prove deployed behavior rather than treating successful
deployment as completion.

### PR 7.1 — Add the assurance contract and runtime identity

- Load `.rustykrab/assurance.toml` from the deployed revision.
- Persist probe definitions, schedules, budgets, evidence TTLs, and response policy.
- Expose a runtime fingerprint that includes source and artifact identity.

### PR 7.2 — Add typed fast probes and an independent scheduler

- Add readiness, dependency, queue, persistence, canary-conversation, and external
  heartbeat probes.
- Expire evidence into `unknown`; never treat missing monitoring as healthy.
- Run the scheduler outside the serving process where policy requires it.

### PR 7.3 — Add synthetic, replay, and deep-evaluation probes

- Run isolated synthetic journeys with fixture cleanup.
- Replay privacy-approved production shapes without production side effects.
- Schedule model/judge/ablation evaluation through the existing E2E harness.
- Enforce per-probe rate and cost budgets.

### PR 7.4 — Add outcome baselines and signal-quality controls

- Scope outcome capture to managed deployments.
- Feed deployment-aware outcomes to `DreamWorker` analysis.
- Add minimum sample sizes, confidence bounds, hysteresis, and bad-signal alerts.
- Compare candidate behavior to the previous known-good baseline.

### PR 7.5 — Add the assurance response controller

- Map high-confidence failures to rollback or quarantine.
- Reconcile repairable regressions into a project finding and proposed
  remediation slice.
- Prevent monitoring proxies from exercising mutation authority.
- Expose healthy, degraded, unhealthy, unknown, and quarantined states.

**Merge gate:** a deployed fixture progresses through probation to healthy; a
seeded regression triggers the configured rollback or remediation stack; stale
or broken monitoring becomes `unknown`, never `healthy`.

## Construction stack 8: Adaptive verification improvement

**Goal:** continuously improve repository verification skills from grounded
outcomes without letting a verifier weaken or validate itself.

### PR 8.1 — Attribute verification outcomes to exact skill versions

- Record repository, change class, exact skill and learned-strategy hashes,
  proposed checks, findings, repairs, later outcomes, cost, latency, and flake.
- Distinguish escaped defects, false positives, ambiguous outcomes, and
  infrastructure failures.
- Require ground-truth signal before a skill becomes mutation-eligible.

### PR 8.2 — Analyze verifier quality off-cycle

- Extend `DreamWorker` with read-only missed-defect, false-positive, stale-rule,
  cost, and recurring-repair analysis.
- Keep proxy-only and judge-only findings non-actionable.
- Produce one evidence-backed improvement opportunity per bounded target.

### PR 8.3 — Generate and falsify bounded skill deltas

- Propose small learned-strategy deltas without rewriting human-authored rules.
- Evaluate historical defects, correct changes, mutations, fake tests, and
  repository-specific edge cases in a held-out corpus.
- Measure defect recall, false-positive rate, escaped defects, latency, cost,
  and flake so an always-rejecting verifier cannot appear improved.

### PR 8.4 — Publish, shadow, promote, and roll back skill PRs

- Create normal GitHub PRs with old-versus-new replay evidence.
- Run candidates in shadow mode without affecting delivery eligibility.
- Promote grounded additive strategies after minimum-sample probation.
- Restore the prior skill pack on ground-truth regression.
- Treat weakening or removal as a separately authorized policy change.

**Merge gate:** a base-pinned verifier misses a seeded ground-truth defect;
Dreaming creates one small PR; held-out evaluation rejects an always-failing
candidate; and a genuinely better strategy promotes after shadow probation
without weakening an existing gate.

## Construction stack 9: RustyKrab self-management

**Goal:** let RustyKrab use the same pipeline on its own repository without
allowing the process being replaced to be its only recovery mechanism.

### PR 9.1 — Make releases immutable and stack-safe

- Remove the release path that amends and force-pushes `main` after merge.
- Put version and changelog changes inside the verified stack.
- Produce releases and artifacts from the immutable merged SHA.
- Record signed provenance from source through artifact.

### PR 9.2 — Embed and expose build identity

- Embed source SHA, build identity, schema version, and artifact digest.
- Expose identity through health and diagnostics endpoints.
- Make deployment reject an artifact that cannot prove the expected source.

### PR 9.3 — Add the standalone supervisor

- Create a minimal `rustykrab-supervisor` executable with no model dependency.
- Stage and verify candidate artifacts.
- Stop, atomically swap, start, and probe the service.
- Preserve the previous artifact until probation completes.

### PR 9.4 — Add self-update recovery and durable handoff

- Snapshot compatible data before migration.
- Enforce forward and rollback schema compatibility policy.
- Resume the same deployment and delivery from durable state after restart.
- Roll back when startup, identity, health, or handoff verification fails.

### PR 9.5 — Enable the repository's own contracts

- Add RustyKrab's deployment and assurance contracts.
- Add a self-update test environment using the real supervisor boundary.
- Exercise successful update, failed candidate, failed migration, interrupted swap,
  and post-deploy behavioral regression.

**Merge gate:** RustyKrab builds, merges, deploys, restarts, and resumes its own
delivery under supervision; a broken candidate automatically restores the last
known-good version.

## Construction stack 10: Production hardening

**Goal:** prove the control plane under concurrency, infrastructure failure, and
adversarial repository inputs before enabling broad autonomous authority.

### PR 10.1 — Add controller fault injection

- Terminate processes at every durable transition boundary.
- Inject SQLite contention, full disk, lost leases, and duplicate events.
- Prove idempotent recovery and single-writer repository mutation.

### PR 10.2 — Add external-system chaos tests

- Inject GitHub rate limits, check outages, branch changes, and merge races.
- Inject deployment timeouts, lost callbacks, and supervisor crashes.
- Prove safe stop, reconciliation, or rollback for each case.

### PR 10.3 — Add security and policy hardening

- Audit command, path, worktree, credential, and secret boundaries.
- Add signed policy and contract verification where required.
- Add protected-environment and destructive-migration approval tiers.
- Prove that plan text and repository content cannot expand authority.

### PR 10.4 — Add operational controls

- Add queue fairness, concurrency limits, cost accounting, and retention policy.
- Add operator inspection, pause, resume, quarantine, and emergency-stop commands.
- Add delivery, deployment, and assurance dashboards and runbooks.

**Merge gate:** the fault matrix passes, authority escalation tests remain denied,
and every interrupted external action reconciles to a safe durable state.

## Authority rollout

Authority is enabled progressively and independently per repository:

| Stage | Planning | Local writes | GitHub writes | Merge | Deploy | Auto-response |
|---|---:|---:|---:|---:|---:|---:|
| 0. Harness | yes | fixture only | no | no | no | no |
| 1. Local delivery | yes | isolated worktrees | no | no | no | no |
| 2. Shadow GitHub | yes | yes | read-only | no | no | no |
| 3. Stack publisher | yes | yes | PR stack | no | no | no |
| 4. Merge operator | yes | yes | yes | eligible prefix | no | no |
| 5. Deployment operator | yes | yes | yes | yes | policy-defined | rollback only |
| 6. Continuous operator | yes | yes | yes | yes | yes | rollback, quarantine, remediation |

No stage is enabled merely because its code exists. Promotion requires the
preceding stage's black-box evidence for that repository and environment.

## First construction stack: file-level plan

The first stack should touch only the new domain, store, E2E, and API seams.

```text
Cargo.toml
crates/rustykrab-projects/
  Cargo.toml
  src/lib.rs
  src/model.rs
  src/revision.rs
  src/change_set.rs
  src/provenance.rs
  src/decision.rs
  src/projection.rs
crates/rustykrab-store/src/
  projects.rs
  lib.rs                       # idempotent migration registration
crates/rustykrab-e2e/src/
  planning_suite.rs
  fixture_repo.rs
  main.rs                      # suite registration
crates/rustykrab-gateway/src/
  project_routes.rs
  routes.rs                    # API composition
  lib.rs                       # module and router registration
docs/plans/
  conversational-project-planning.md
  adaptive-verification-skills.md
  autonomous-software-delivery.md
  autonomous-software-delivery-construction.md
```

Exact names may follow existing module conventions discovered during
implementation, but the dependency direction must remain:

```text
gateway -> projects -> core
gateway -> store
store   -> projects
e2e     -> gateway/cli public behavior
```

`rustykrab-projects` must not depend on the gateway, store, CLI, GitHub, model
providers, or a shell runner. This keeps revision and projection rules
deterministic.

### First-stack acceptance cases

- A project can begin with a vague natural-language idea and no schema.
- Applying the same plan change to the same parent revision is deterministic.
- Invalid node relationships and missing provenance are rejected.
- A correction supersedes an earlier claim without deleting its history.
- A question can move from open to decided and retains its source messages.
- Project state reconstructs identically after a store reopen.
- Brief, roadmap, decision, and question projections from one revision agree.
- Revision comparison explains material changes and their provenance.
- Replaying a project-create or change request does not duplicate state.
- The scenario catalog reports stable expected-failure identifiers for every
  capability intentionally left for later construction stacks.

## Promotion checklist for every construction stack

A stack may merge only when all answers are yes:

- Does every PR provide useful, verifiable behavior independently of descendants?
- Does each PR pass workspace formatting, linting, and tests?
- Does the stack-specific black-box demonstration pass from a clean checkout?
- Is evidence attached to the exact head SHAs currently submitted?
- Are restart, retry, and idempotency tested for every new side effect?
- Are disabled authorities still structurally unreachable?
- Can the whole stack be reverted without corrupting later durable state?
- Is the next stack based on the resulting `main`, rather than an older ancestor?

## Estimated shape

The program is approximately eleven construction stacks and forty-eight PRs.
That number is a planning boundary, not a quota. A PR should split when it stops
being independently understandable or verifiable, and combine only when neither
piece would provide a functional proof by itself.

The critical path is:

```text
durable project model
  -> planning runtime and orchestration agent
  -> authorized execution-slice contract
  -> deterministic local delivery
  -> autonomous workers
  -> native GitHub stack
  -> repository deployment
  -> continuous assurance
  -> adaptive verification improvement
  -> external self-update supervisor
  -> hardening and authority promotion
```

## Immediate starting action

1. Preserve the current dirty worktree exactly as it is.
2. Select the clean integration commit that should contain any prerequisite work.
3. Create a fresh worktree and native construction stack from that commit.
4. Implement PR 0.1 first: documentation, fixture-repository harness, and explicit
   expected-failure scenarios.
5. Submit the four PRs of construction stack 0 together only after each local
   layer passes its inherited gates.
6. Merge stack 0, verify the durable project-model demo on `main`, and start
   stack 1 from the new `main` SHA.

The first irreversible external authority does not arrive until construction
stack 5, and even then it is opt-in repository policy. Everything before that can
be developed and proven against isolated local fixtures.
