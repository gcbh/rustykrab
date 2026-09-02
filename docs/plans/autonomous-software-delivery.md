# Plan: Autonomous Software Delivery, Deployment, and Continuous Assurance with GitHub PR Stacks

**Status:** Proposed
**Date:** 2026-09-01
**Repository:** `gcbh/rustykrab`

## 1. Product promise

Through an ongoing project conversation and a repository, RustyKrab should be
able to:

1. help the user discover, refine, and revise the long-term plan without
   requiring a complete specification up front;
2. maintain a durable, evidence-backed project model containing outcomes,
   decisions, assumptions, questions, risks, milestones, and repository facts;
3. identify the next sufficiently understood, valuable execution slice;
4. compile only that authorized slice into an immutable internal delivery
   manifest;
5. turn the slice into an executable, dependency-ordered work graph;
6. implement every increment on its own layer of a native GitHub PR stack;
7. verify each layer against its own acceptance criteria and the cumulative
   stack below it;
8. repair failures within bounded budgets;
9. submit a linked `gh stack` whose pull requests each carry machine-checkable
   evidence;
10. when pre-authorized repository policy permits it, ask GitHub to merge an
   eligible prefix or the complete stack;
11. execute the repository's own deployment contract for the resulting merge;
   and
12. verify the running deployment, rolling it back when its contract says the
   rollout is unhealthy; and
13. continuously verify the deployed system's functional behavior, safety
    invariants, and outcome quality, opening a new remediation stack when
    durable evidence shows drift—all without requiring the user to participate
    in execution or review after the slice has been authorized.

The normal terminal outcomes are:

- **stack ready** — every planned layer is a verified GitHub PR, but policy
  requires a human merge for one or more layers;
- **partially merged** — a verified contiguous prefix landed and the remaining
  layers are still open, blocked, or being repaired;
- **merged** — GitHub landed the requested stack, and the repository declares
  no deployment target;
- **deployed** — GitHub landed the requested stack and the repository-defined
  deployment and health verification succeeded;
- **rolled back** — deployment failed its health contract and the last known
  good version was restored;
- **blocked** — the system cannot proceed safely and reports the exact missing
  authority, decision, or external dependency; or
- **failed** — a bounded execution or repair budget was exhausted, with all
  attempts and evidence preserved.

Those are delivery-run outcomes. Every deployed version immediately enters a
separate, persistent assurance state: `unknown`, `healthy`, `degraded`,
`quarantined`, or `remediating`. “Deployed” is therefore not a claim that the
system will remain correct; it is the point at which continuous verification
takes over.

Planning is a durable conversational product; execution is a delivery system,
not one unusually long agent turn. The orchestration agent owns project
coherence and the user relationship. Deterministic controllers—not a model—own
execution lifecycle, durability, policy, verification state, and the definition
of done.

## 2. Operating assumptions

- The user develops the plan through a canonical project conversation rather
  than supplying a complete schema or step-by-step execution instructions.
- The project plan remains editable and may contain unresolved future work.
  Readiness is assessed for one execution slice at a time.
- The user authorizes a slice in ordinary language or through standing project
  policy; after authorization, execution does not require intermediate input or
  review unless an un-delegated material decision makes safe progress impossible.
- The orchestration agent records user decisions, its recommendations,
  repository observations, assumptions, and delegated judgment as distinct,
  provenance-bearing project state.
- The target repository and base revision are explicit and immutable for one
  delivery run, but need not be fixed for the entire project lifetime.
- Every stack layer is a functional increment with its own objective,
  acceptance criteria, branch, commits, pull request, and evidence.
- All implementation happens in isolated worktrees or equivalent checkouts.
- An executor can modify only the worktree for its assigned layer.
- All stack branches live in the same GitHub repository; cross-fork stacks are
  not part of the target architecture.
- GitHub's native `gh stack` workflow is the primary publication mechanism.
  Because it is currently a public-preview feature, the runtime must pin and
  capability-check its CLI contract before mutating a remote stack.
- Stack-aware model sessions use native `git` and native `gh stack` commands
  through Bash. Stack discipline is enforced by shell policy, credentials,
  allowed subcommands, and postcondition validation rather than hidden behind a
  model-facing source-control abstraction.
- Raw `git push`, `gh pr create`, `gh pr merge`, generic `gh api` mutation, and
  direct GitHub API mutation are unavailable during delivery. Remote publication
  must flow through `gh stack submit`, `push`, or `sync`; merge must flow through
  `gh stack merge` after deterministic policy unlocks the authorized prefix.
- A fresh verifier evaluates the implementation from the frozen slice manifest
  and resulting diff. It does not inherit the implementer's claim that the work
  is done.
- Lack of user interaction does not imply unlimited authority. Authority is
  granted before the run by repository policy.
- Direct pushes to a protected base branch are never part of the autonomous
  path. GitHub is the merge authority: an authorized merge-operator model
  invokes native `gh stack merge`, and the controller verifies GitHub's result.
- Deployment is repository-defined. RustyKrab never invents deployment
  commands from prose or assumes that merging and deploying are equivalent.
- The repository-owned deployment contract names the driver, environment,
  artifact, health checks, promotion rules, and rollback procedure.
- The repository also declares a behavioral assurance contract: what must keep
  working, how often to test it, what evidence is trustworthy, and which
  response is authorized for each severity.
- Continuous does not mean an unbounded tight loop. Cheap deterministic probes
  run frequently; expensive model and E2E evaluations run on schedules or
  during idle capacity; real outcomes are captured as work occurs.
- Behavioral canaries use isolated principals, data, and side-effect-free or
  explicitly disposable resources. Production users are never test fixtures.
- Merge credentials and deployment credentials are separate, least-privilege
  capabilities. Executors and verifiers receive neither.
- A process cannot safely supervise replacement of its own executable. A
  minimal external supervisor owns staged installation, restart, health
  confirmation, and rollback when RustyKrab deploys RustyKrab.

## 3. Existing foundation

RustyKrab already contains useful parts of the system:

- `AgentRunner` executes tool loops under sandbox and capability policy.
- `todo_*` preserves short-horizon intent through context compaction.
- `task_complete` prevents a model from ending an active task with a planning
  manifesto instead of a completion signal.
- `delegated_tasks` is a durable asynchronous queue with cancellation,
  attribution, tool narrowing, and peer-node execution.
- sub-agent definitions can create fresh, capability-restricted agent runs.
- peer nodes support durable remote tasks and bounded delegation depth.
- the E2E harness boots the real daemon, asserts on output and stored state,
  and represents future behavior as `xfail` scenarios.
- CI already has a single `CI Pass` gate across formatting, Clippy, tests,
  security audit, and E2E evaluation.

The important gaps are:

- there is no durable project abstraction joining a canonical planning
  conversation to outcomes, decisions, assumptions, questions, risks,
  milestones, and execution history;
- there is no versioned plan graph, provenance model, readiness assessment, or
  natural-language execution-slice proposal;
- context compaction can preserve short-horizon todos, but cannot reconstruct a
  long-lived project's current understanding and decision history;
- todos and local sub-agent sessions are in-memory rather than durable;
- the multi-session API is stubbed;
- delegated tasks return text, not structured artifacts or verification
  evidence;
- there is no delivery-level state machine spanning many agent turns;
- there is no isolated-worktree manager, stack-aware shell policy, or durable
  native-command reconciliation journal;
- completion is model-declared rather than proven against a verification
  manifest;
- no policy engine decides whether a passing change may be pushed, opened as a
  pull request, or merged;
- a daemon restart fails an in-flight delegated task instead of resuming a
  checkpointed software delivery;
- there is no durable concept of a functional increment, stack layer,
  cascading rebase, native GitHub stack, or per-layer verification state;
- there is no repository deployment contract or typed deployment backend;
- the installer preserves an old RustyKrab bundle, but no external supervisor
  verifies a new self-deployment and restores that bundle automatically;
- the current release workflow amends and force-pushes `main` after a PR merge,
  which breaks immutable merge-SHA provenance and is not safe as the release
  trigger for an atomically merged PR stack; and
- outcome capture is opt-in and the `DreamWorker` is read-only/report-only, so
  no standing controller currently compares deployed behavior to a contract,
  detects release regressions, or creates a verified remediation delivery.

## 4. Conversational planning and execution boundary

The product input is an ongoing conversation, not a configuration file. A
canonical orchestration agent helps the user form the plan while maintaining a
durable, versioned project model behind the conversation. The detailed planning
model and interaction contract are defined in
`docs/plans/conversational-project-planning.md`.

The project model records typed, provenance-bearing outcomes, requirements,
constraints, non-goals, decisions, options, assumptions, questions, risks,
repository observations, workstreams, milestones, acceptance behaviors, and
dependencies. Human-readable briefs, roadmaps, decision logs, risk views, and
next-slice proposals are projections from that model rather than independent
sources of truth.

The complete long-term plan never has to be frozen. Instead, the orchestration
agent assesses whether one bounded execution slice is ready. It develops that
slice through conversation, repository inspection, research, and small
experiments. Future questions do not block current work unless their answers
materially change the slice.

When ready, the agent proposes the slice in ordinary language, including:

- the outcome it advances;
- included behavior and explicit non-goals;
- observable acceptance scenarios;
- relevant decisions, assumptions, dependencies, and risks;
- the required repository and external authority;
- expected recovery or rollback behavior; and
- important wider-project questions deliberately left unresolved.

The user authorizes, revises, or defers the proposal conversationally. A
standing judgment policy may authorize the agent to select or start reversible,
low-risk slices within explicit boundaries.

Authorization compiles the proposal into an immutable internal
`DeliveryManifest`. Users do not author this manifest. It freezes only the
project revision, repository base, acceptance behavior, constraints, authority,
budgets, and evidence needed for that delivery. Later planning conversation may
continue, but cannot silently mutate an active manifest.

The compiler may derive work items, tests, and affected components, but it must
not silently widen permissions, weaken acceptance criteria, or resolve an
un-delegated material question. Ambiguity that does not affect safety can use a
recorded default. Ambiguity that changes public behavior, data loss risk,
security posture, cost, or authority blocks only the affected slice before code
is changed.

The manifest, originating project revision, base commit, policy version,
deployment contract, and assurance contract are content-hashed. Every later
artifact points back to those hashes.

The delivery compiler then produces a `StackManifest`, ordered bottom to top.
Every layer has:

- one independently explainable capability or behavior change;
- acceptance criteria that become true at that layer;
- an exact parent layer or the stack trunk;
- an expected scope and risk class;
- a verification manifest;
- a branch name and eventual GitHub PR identity; and
- explicit dependencies on lower layers.

“Functional” does not have to mean user-interface work. A layer may introduce
an internal platform capability, migration seam, or test harness, but it must
leave the cumulative repository buildable, testable, and useful for a stated
next consumer. Mechanical fragments that have no independently verifiable
purpose should remain commits within one layer rather than separate PRs.

### 4.1 Repository deployment contract

Deployment behavior comes from a versioned file in the repository, not from
the model. A minimal `.rustykrab/delivery.toml` might be:

```toml
version = 1

[merge]
provider = "github-stack"
method = "squash"

[deploy]
driver = "github-actions"
workflow = "release.yml"
trigger = "merge"
environment = "production"
artifact = "rustykrab-aarch64-apple-darwin.dmg"
timeout_seconds = 3600

[health]
stability_seconds = 300

[[health.checks]]
driver = "http"
url = "http://127.0.0.1:3000/api/health"
expect_status = 200
timeout_seconds = 120

[[health.checks]]
driver = "version"
expect_source_sha = "${merged_sha}"

[rollback]
driver = "previous-artifact"
required = true
timeout_seconds = 300
```

The schema supports typed drivers rather than arbitrary shell supplied at run
time. A repository may use a checked-in command driver, but the executable,
arguments, working directory, environment allowlist, and artifact inputs must
all be fixed by the trusted contract and constrained by repository policy.

The active run uses the contract from its frozen trunk SHA and records its
hash. A stack cannot edit its own deployment contract and immediately gain new
deployment authority. Contract changes are high risk and become eligible only
for a later run after they exist on the trusted trunk.

If the trusted trunk contains no deployment contract, successful GitHub merge
is the terminal state. If a contract exists, `merged` is an intermediate state
and the run cannot report success until deployment health is proven or rollback
is completed and reported.

### 4.2 Behavioral assurance contract

A repository declares ongoing expectations separately from rollout mechanics.
A minimal `.rustykrab/assurance.toml` might be:

```toml
version = 1

[budgets]
max_probe_cpu_seconds_per_hour = 300
max_model_tokens_per_day = 100000
expensive_evals = "idle-only"

[[probes]]
id = "gateway-health"
driver = "http"
interval_seconds = 60
signal = "verifiable"
severity = "critical"
url = "http://127.0.0.1:3000/api/health"
expect_status = 200

[[probes]]
id = "credential-overwrite-guard"
driver = "e2e-case"
interval_seconds = 3600
signal = "verifiable"
severity = "critical"
case = "credential-agent-overwrite"
isolation = "ephemeral"

[[probes]]
id = "tool-task-success-rate"
driver = "outcome-window"
interval_seconds = 21600
signal = "ground-truth"
severity = "high"
minimum_observations = 20
maximum_regression = 0.10

[actions]
critical_during_probation = "rollback"
critical_after_probation = "quarantine-and-remediate"
high_confidence_regression = "create-stack"
insufficient_evidence = "remain-unknown"
```

Every probe declares its evidence class, cost, isolation, expected result,
severity, and authorized response. Deterministic postconditions and explicit
ground truth outrank model judges and implicit behavioral proxies. A judge may
triage or add evidence, but it cannot be the sole basis for rollback, code
change, or a `healthy` verdict.

The assurance setpoint cannot weaken itself during the rollout it evaluates.
During probation, the controller runs the union of the pre-deploy contract and
the candidate contract, choosing the stricter threshold on overlap. Removing a
probe, lowering severity, loosening a threshold, or changing an action requires
a separately policy-approved contract change. The candidate contract becomes
the baseline only after probation succeeds.

RustyKrab's initial assurance contract should cover at least:

- external gateway health, source SHA, queue progress, and assurance heartbeat;
- authentication, origin, capability, sandbox, and credential-overwrite
  boundaries using deterministic scripted scenarios;
- `task_complete`, planning-only recovery, tool execution, and context
  compaction behavior;
- scheduled-job delivery and restart recovery with an isolated canary target;
- memory save/retrieval/compaction invariants in a disposable namespace;
- one smoke scenario for every configured provider and enabled channel;
- repeated model-behavior suites after any model/provider fingerprint change;
- real tool/skill outcome quality with signal-readiness gates; and
- a synthetic plan-to-verified-stack exercise against a local fixture, without
  remote merge or deployment authority.

## 5. Delivery lifecycle

```text
draft
  -> validating
  -> stack_planned
  -> building_stack
       -> layer[1..n]: executing <----------------------+
                        -> verifying                    |
                             -> repairing -> verifying -+
                             -> pr_ready
  -> stack_ready
       -> merge_eligible
       -> merging
       -> partially_merged -> restacking -> verifying
       -> merged
            -> deployment_pending
            -> deploying
            -> verifying_deployment
                 -> deployed
                 -> rolling_back -> rolled_back

Any active state -> blocked | failed | cancelled
```

Continuous assurance is a second state machine keyed by repository,
environment, deployed SHA, provider/config fingerprint, and contract version:

```text
unknown
  -> probation
       -> healthy -> observing ------------------------------+
                       |                                     |
                       -> degraded -> diagnosing              |
                            -> recent-release rollback         |
                            -> quarantine                      |
                            -> remediation_delivery -> stack --+

Any state -> unknown when monitoring is stale or evidence is insufficient
```

The delivery and every layer have separate states. The delivery reaches
`stack_ready` only when all planned layers have published PRs whose exact head
SHAs are verified. A lower layer may become ready while an upper layer is still
executing, but no upper layer can be declared merge-eligible while a required
lower layer is unverified.

`merged` is controller-observed GitHub state, never a local-git claim. When the
frozen repository contract declares a deployment, merge success creates a
durable deployment intent and advances to `deployment_pending`. A run cannot
skip from `merged` to `deployed`, and a failed health check cannot be reported
as success merely because the artifact was installed.

`healthy` is always time-bounded. It means the required probes passed recently
enough for their declared freshness windows; it is never a permanent label.

State transitions are controller-owned and transactional. An agent cannot set
itself to `verified`, `stack_ready`, `merge_eligible`, `merged`, `deployed`, or
`rolled_back`. A judge cannot set `healthy`, clear a finding, or authorize an
assurance action; those states are derived by deterministic policy from stored
evidence.

### 5.1 Planning

Long-term planning happens in the canonical project conversation. The
orchestration agent maintains the evolving project model, resolves or delegates
only the questions that matter now, and proposes a bounded execution slice when
its readiness evidence is sufficient.

After conversational authorization, the delivery compiler freezes that slice
and partitions it into a linear `StackManifest`, then turns each layer into a
DAG of small work items. It does not attempt to freeze the rest of the roadmap.

Each layer must satisfy these invariants:

- it introduces one cohesive, independently describable capability;
- its cumulative branch builds and passes required repository checks;
- its own acceptance criteria are demonstrated at that layer;
- its incremental diff is small enough to inspect and diagnose;
- it does not intentionally leave the repository broken for a later PR;
- schemas, APIs, and migrations remain compatible with every state obtainable
  while the stack is partially merged; and
- reverting that layer and all layers above it has a defined result.

Every work item within a layer has:

- an objective and non-goals;
- dependencies;
- expected files or subsystem scope;
- executable acceptance checks;
- a risk classification;
- an implementation budget; and
- the evidence required to mark it complete.

The controller rejects dependency cycles, layers without functional acceptance
evidence, work whose requested scope exceeds policy, and plans that split one
atomic safety change across PRs in a way that creates a broken or insecure
intermediate state.

### 5.2 Execution

The controller builds layers bottom to top. Layer 1 starts from the frozen
trunk SHA. Layer N starts from the last verified head SHA of layer N-1. The
first implementation should execute one layer at a time; later versions may
speculate on upper layers, but speculative results cannot publish until every
ancestor is stable.

The controller leases one ready work item to an executor. The executor receives
only its layer and work-item slice, parent SHA, repository context, current
worktree state, allowed tools, and relevant prior failed evidence.

On completion, the executor must return structured output:

```json
{
  "layer": 2,
  "summary": "Implemented create-only credential writes",
  "changed_paths": ["crates/rustykrab-store/src/secret.rs"],
  "commit": "<sha>",
  "checks_run": ["cargo test -p rustykrab-store"],
  "known_limits": []
}
```

The controller verifies the commit and changed paths itself. It never trusts
those fields merely because the model returned them.

### 5.3 Verification

Verification is a hybrid of a coded evidence kernel and repository-specific,
versioned verification skills. The detailed skill contract, pinning model, and
Dreaming improvement loop are defined in
`docs/plans/adaptive-verification-skills.md`.

Every PR layer receives two scopes of verification:

- **Incremental verification** inspects `parent_layer_head..layer_head` and
  proves the functionality claimed by this PR.
- **Cumulative verification** checks the repository at `layer_head`, including
  every lower layer, and proves the stack remains buildable and coherent.

For each delivery, the controller resolves and content-hashes the verification
skill pack from the trusted base revision plus any signed setup overlays. A
candidate edit to its own verifier never judges that candidate; it applies only
to future deliveries after independent evaluation and probation.

Verification then has four evidence classes:

1. **Structural:** worktree clean except intended changes; no forbidden paths;
   no unresolved conflict markers; a linear commit path to the parent layer;
   generated files and dependency locks are consistent.
2. **Deterministic:** the coded runner records formatting, compilation, lint,
   unit, integration, E2E, security, and repository-mandatory command results
   with bounded output artifacts.
3. **Acceptance:** repository verification skills design or select relevant
   tests, state assertions, adversarial cases, and inspections. Each layer
   criterion links to evidence, and global criteria map to the first layer that
   establishes them plus later layers that can invalidate them. “Tests pass”
   alone is not proof that the requested behavior exists.
4. **Independent review:** a fresh verifier loads the applicable pinned skills
   and reads the frozen slice, base diff, incremental layer diff, cumulative
   diff, test changes, and evidence. It checks missing behavior, test quality,
   regressions, unsafe scope, misplaced changes, and whether the implementation
   merely changed tests to bless incorrect behavior.

Skills may propose commands and generate ephemeral tests in a disposable
verifier worktree. Code validates capabilities, executes checks, hashes
artifacts, and evaluates hard gates. Skills emit typed findings but cannot set
or clear `verified`, remove required checks, push, merge, or deploy.

Verifier findings are typed by severity and include file/line evidence. A
blocking finding creates repair work; it cannot be dismissed by the executor.
After repair, all invalidated checks run again. If a lower layer changes, its
head SHA and the base SHA of every layer above it change; all affected
descendants are cascaded through `gh stack rebase` and their evidence is
invalidated. The verifier has no push or merge authority. Every evidence record
also names the exact skill-pack and verifier fingerprints that produced it.

### 5.4 Integration and merge

The source-control artifact is a native GitHub stack. Stack-aware execution
models use the native CLI directly through Bash. The expected construction flow
is:

```text
gh stack init --base <trunk> <layer-1-branch>
# implement and commit layer 1
gh stack add <layer-2-branch>
# implement and commit layer 2; repeat as needed
gh stack submit --auto --open
```

This is not general `gh` authority. Each execution phase receives a native
command profile:

- **layer executor:** local `git` inspection, staging, and commits plus
  read-only `gh stack view` and navigation;
- **stack coordinator:** `gh stack init`, `add`, `view`, `rebase`, `submit`,
  `push`, and `sync` for the assigned stack and repository; and
- **merge operator:** `gh stack merge` only up to the deterministically
  authorized PR after every gate has passed.

The shell denies alternate publication paths and initially denies destructive
stack restructuring such as `gh stack unstack` or unrestricted `modify`.
`gh stack submit --auto --open` remains the non-interactive publication path: it
pushes branches, creates or updates their PRs with the correct bases, marks them
ready, and links them as a native GitHub stack.

GitHub credentials are not placed in a general model-visible environment. After
the shell policy approves an exact native stack command, a credential broker
launches that command with a short-lived repository-scoped credential and
removes it before returning bounded output. From the model's perspective this
is ordinary Bash and native `gh stack`; from the security boundary it is not an
arbitrary credentialed shell.

Every mutating command records its intended stack, repository, current head,
allowed effect, exact invocation, result, and post-command observation. On
restart, the controller asks Git and GitHub what happened rather than replaying
the command blindly.

Before one stack layer is declared PR-ready, the execution session and
controller must together:

- have the stack coordinator fetch or sync current trunk and remote metadata
  through native stack commands;
- have the controller prove the local stack is linear and matches the frozen
  manifest;
- have the controller prove the layer branch is based on the verified head of
  the layer below it;
- run incremental and cumulative verification;
- have the stack coordinator push and submit or update the native GitHub stack;
- wait for the required checks on that layer; and
- have the controller confirm the PR head SHA and parent SHA still match the
  verified pair.

GitHub evaluates branch protection, required checks, reviews, CODEOWNERS, and
Actions against the stack trunk, even when a PR directly targets the branch
below it. This is desirable: every functional layer is held to the same
repository standard and CI runs for every layer without custom base-branch
workflow rules.

If the trunk or a lower layer changes, a stack-coordinator session runs a
cascading `gh stack rebase`, publishes with `gh stack push`, and requests
reverification of every changed layer. A non-interactive divergence or rebase
conflict blocks the stack with evidence; neither model nor controller guesses
which history is authoritative.

The stack must merge in dependency order. The non-interactive operation
`gh stack merge <top-eligible-pr> --yes --squash` atomically lands the
contiguous prefix ending at that PR; merging the complete stack omits the PR
argument. With squash merging this yields one squashed commit per PR on the
trunk. If a queued or external merge stops partway through, the merged prefix
remains landed and a stack-coordinator session runs `gh stack sync --prune`,
restacks, and reverifies the remaining suffix before retrying.

### 5.5 Deployment and runtime verification

After GitHub reports a successful stack merge, the controller:

1. resolves the resulting trunk commits and proves they correspond to the
   merged stack layers;
2. writes a durable deployment intent containing the repository, environment,
   merged SHA, contract hash, driver, and idempotency key;
3. observes or triggers the repository-defined build/release workflow;
4. resolves the produced artifact back to the merged SHA;
5. verifies checksums, signatures, attestations, and any contract-specific
   provenance before installation;
6. asks the deployment backend to stage and promote the artifact;
7. runs every repository-defined health and version check; and
8. records `deployed`, or invokes the declared rollback and records
   `rolled_back` with both the failed and restored versions.

Deployment is idempotent by `(repository, environment, merged_sha,
contract_hash)`. A restart may resume observation or health verification, but
it must not launch a second rollout for the same intent.

The repository chooses when deployment occurs:

- `complete-stack` — deploy only after the entire planned stack lands; this is
  the safe default;
- `merged-prefix` — deploy each policy-approved prefix, allowed only when the
  repository explicitly guarantees every prefix is independently deployable;
  or
- `none` — merging is the terminal action.

A GitHub Actions driver may observe an existing merge-triggered workflow or
dispatch a named workflow. Other repositories may declare a typed Kubernetes,
container, package, serverless, SSH, or local-supervisor driver. The controller
does not infer a driver from incidental files and does not hand deployment
credentials to an agent shell.

### 5.6 Continuous behavioral assurance

After deployment, verification continues at several timescales:

1. **Liveness and safety invariants (seconds to minutes):** external health,
   version, queue progress, sandbox denials, credential boundaries, and
   deadman/freshness checks. These are deterministic and cheap.
2. **Synthetic functional canaries (minutes to hours):** isolated executions of
   repository-defined critical paths with machine-checkable postconditions.
3. **Replay and shadow evaluation (hours):** sanitized historical tasks run
   against the current version with external side effects stubbed or directed
   to disposable resources, then compared with stored ground truth.
4. **Real outcome windows (continuous aggregation):** success, failure,
   ambiguity, retry, latency, tool-error, and explicit user-correction rates,
   attributed to the exact runtime fingerprint.
5. **Deep evaluation (daily, release-triggered, or idle-only):** the full E2E,
   model, credential, login, ablation, and independent-judge suites within
   declared cost budgets.

The runtime fingerprint includes at least source SHA, artifact hash, model and
provider version, tool-schema hash, policy/config hash, skill versions, memory
generation, operating system, and deployment environment. Behavior can regress
without a code change; a provider update, configuration change, dependency
outage, skill edit, or corrupted memory must create a new comparison cohort.

The assurance controller applies confidence, minimum-sample, freshness,
hysteresis, and consecutive-failure rules from the contract. One flaky probe
does not create a code change. Missing or stale probes yield `unknown`, not
`healthy`.

Nondeterministic agent behavior is evaluated as a distribution, not a single
transcript: repeated trials, hard postconditions, quorum/majority rules,
latency and tool-error bounds, and confidence intervals are recorded before a
baseline or regression verdict is allowed. The report always identifies the
judge and model versions so evaluator drift cannot masquerade as product drift.

Authorized responses are evidence-sensitive:

- a critical deterministic regression attributable to a release still in its
  probation window triggers the repository's rollback path;
- a critical invariant failure outside probation may quarantine the affected
  capability and create a remediation delivery;
- a persistent, ground-truth behavioral regression adds a finding, reproduction,
  and acceptance behavior to the project model, then proposes a remediation
  slice through the normal verified GitHub-stack pipeline;
- proxy-only or judge-only degradation opens an assurance finding and gathers
  evidence, but cannot change code, merge, deploy, or roll back; and
- an external dependency incident is recorded and monitored without rewriting
  the repository unless evidence identifies a repository-owned defect.

The current `DreamWorker` remains the read-only analysis stage and supplies
outcome trends with explicit signal-quality/readiness semantics. It does not
edit production directly. All remediation code still passes through planning,
functional PR layers, independent verification, GitHub merge, deployment, and
the next probation window.

The monitor also verifies itself: an external supervisor checks assurance
heartbeats, probe freshness, scheduler lag, and the identity of the running
evaluator. A silent or stale verifier degrades the assurance state to `unknown`
and cannot certify the system healthy.

## 6. Merge policy

Auto-merge is an explicit repository policy, not a conclusion made by the
model. Authority applies to a GitHub stack or a contiguous prefix, never to an
arbitrary out-of-order PR. Recommended tiers:

| Tier | Result | Required authority |
|---|---|---|
| 0 | Local verified stack branches only | Repository read/write in isolated worktrees |
| 1 | Submit/update native GitHub stack | Pre-authorized remote branch, PR, and stack writes |
| 2 | Merge eligible stack prefix | Pre-authorized merge policy plus all required gates |
| 3 | Deploy/release | Separate policy; never implied by merge authority |
| 4 | Continuous rollback/quarantine/remediation | Trusted assurance contract plus per-action grants |

Tier 2 eligibility requires all of the following:

- the project's granted authority and frozen delivery manifest permit
  auto-merge for this slice;
- the repository policy allows every layer's computed risk class;
- all layer and global acceptance criteria have evidence;
- every required local and remote check passed on every exact head SHA in the
  prefix;
- no layer in the prefix has an open blocking verifier finding;
- the stack is linear, conflict-free, and current with its trunk;
- every layer is within path, size, dependency, and generated-file limits;
- branch protection remains active; and
- no PR in the prefix is a draft or has an external change request.

A high-risk layer cuts the automatically mergeable prefix at the layer below
it. Layers above that point may still be fully verified and published, but
they cannot merge before the excluded dependency is reviewed and landed.

Tier 3 deployment eligibility additionally requires:

- GitHub reports the intended exact stack or prefix as merged;
- the frozen trunk deployment contract permits the target environment and
  merged scope;
- the configured deployment driver is available and policy-approved;
- the produced artifact is traceable to the observed merged SHA;
- artifact provenance and signature requirements pass;
- deployment credentials are available only to the typed backend;
- the rollout has a bounded timeout and idempotency key; and
- every production deployment has either an automated rollback or an explicit
  repository contract that proves rollback is unnecessary.

Read-only continuous verification requires no mutation grant. Tier 4 automated
responses require all of the following:

- the deployed assurance contract and evaluator identity are known and fresh;
- the probe ran in its declared isolation and budget;
- the evidence meets the action's signal-quality, sample-size, hysteresis, and
  consecutive-failure rules;
- rollback or quarantine targets exactly the affected environment/capability;
- remediation creates a normal delivery run rather than editing live code; and
- a judge or implicit proxy is never the sole evidence for a mutating action.

Recommended initial auto-merge exclusions:

- authentication, authorization, cryptography, secret handling, or sandbox
  policy;
- database migrations that delete or transform existing data;
- CI/release workflow changes;
- dependency additions or provenance changes;
- public API breaking changes;
- deployment, infrastructure, billing, or account changes;
- deployment-contract, updater, health-gate, or rollback-policy changes in the
  same run that would consume them;
- changes exceeding configured file or line thresholds; and
- flaky, skipped, weakened, or deleted verification without a plan criterion
  explicitly authorizing it.

These changes may still reach `pr_ready`; they simply require review to merge.
No autonomous path may disable protection, use an administrator bypass, or
push directly to the protected branch.

## 7. Architecture

### 7.1 `rustykrab-projects`

A new planning-domain crate owns the durable project relationship independently
of delivery execution.

Responsibilities:

- version the project graph and provenance-bearing plan changes;
- store outcomes, requirements, decisions, questions, assumptions, risks,
  milestones, repository observations, and acceptance behaviors;
- derive deterministic human-readable projections and revision comparisons;
- evaluate per-slice readiness without requiring the whole roadmap to settle;
- apply the project's delegated-judgment policy;
- create, revise, authorize, and supersede execution-slice proposals; and
- reconcile delivery and assurance evidence into new project revisions.

It contains planning-domain rules, not model prompting, Axum types, GitHub
commands, or delivery state transitions. The orchestration agent uses this crate
through application services in `rustykrab-runtime`.

### 7.2 `rustykrab-runtime`

First extract turn orchestration and non-HTTP application state from
`rustykrab-gateway`. Delivery workers need to run agent turns without pretending
to be an HTTP request or constructing Axum state.

Responsibilities:

- prepare and execute one agent turn;
- persist the conversation and outcome;
- enforce session capabilities;
- emit structured progress events; and
- expose cancellation and checkpoint hooks.

The gateway and channel loops become runtime clients.

### 7.3 `rustykrab-delivery`

A new application crate owns the long-horizon workflow.

Responsibilities:

- compile and freeze an authorized execution slice into a `DeliveryManifest`;
- build and validate the ordered `StackManifest` plus each layer's work DAG;
- lease work to planner, executor, verifier, and integrator roles;
- checkpoint after every transition;
- collect and invalidate evidence;
- run repair loops within budget;
- evaluate merge policy; and
- publish delivery events and final reports.

It depends on interfaces, not GitHub or shell details.

### 7.4 Workspace isolation and ownership kernel

A small workspace kernel confines and inspects isolated worktrees without
replacing native Git:

- resolve and pin the base commit;
- attach a uniquely named worktree to every branch registered by native
  `gh stack` execution;
- anchor each layer to the exact verified head of the layer below it;
- report status and diff;
- validate that model-created commits touch only allowed paths;
- verify linear ancestry and detect conflicts or foreign modifications;
- retain immutable PR-ready worktrees until the stack is merged or cancelled;
  and
- preserve failed worktrees for diagnosis, then clean them through an explicit
  retention policy.

The executor's filesystem capability is rooted at this worktree. Its Bash
profile permits ordinary local Git work. Remote mutation is available only to a
stack-coordinator or merge-operator profile through approved native `gh stack`
subcommands.

### 7.5 Stack-aware shell policy and command journal

A command policy makes native stack execution the model's only publication and
merge path:

- capability-check and pin the native `gh stack` CLI version;
- assign explicit layer-executor, stack-coordinator, and merge-operator command
  profiles;
- permit native local Git operations inside the assigned worktree;
- deny alternate remote paths such as raw `git push`, `gh pr` mutation, and
  generic `gh api` mutation;
- inject short-lived repository credentials only into an already-approved
  native stack subprocess;
- record intent, invocation, result, and postcondition for every mutating
  command; and
- validate the resulting local and GitHub stack shape against the frozen
  manifest after every mutation.

The model sees and uses normal Bash, Git, and `gh stack`; it does not use a
model-facing source-control wrapper. The controller determines which command
profile and stack boundary are currently authorized, but it does not choose the
model's Git command sequence. Git and GitHub remain the source of truth, so
restart recovery inspects native state before another model turn is scheduled.

### 7.6 Verification kernel and skill runtime

The coded verification kernel runs permitted commands, enforces hard gates,
and stores:

- command, environment policy, start/end time, exit status, and output digest;
- produced reports such as JUnit, coverage, audit, and E2E JSON;
- tested commit SHA and toolchain identity; and
- which acceptance criteria the evidence satisfies;
- the selected verification-skill identities, versions, hashes, and selection
  rationale; and
- the verifier model, tool, repository, and setup fingerprints.

A check result is reusable only while its inputs and commit SHA remain valid.

The skill runtime loads the base-pinned repository verification pack, selects
applicable specialist skills, provides a fresh verifier context, and accepts
structured check proposals and findings. Skills can evolve per repository and
setup, while the kernel retains authority over capability grants, evidence
integrity, mandatory checks, invalidation, and the `verified` transition.

### 7.7 Policy engine

Policy is loaded once from repository configuration and intersected with the
project authority policy and frozen delivery manifest. The narrower result
wins. The engine decides tool grants,
network access, path scope, command allowlists, budgets, PR authority, and
merge, deployment, rollback, quarantine, and remediation eligibility. Every
decision is recorded with its rule and inputs.

### 7.8 `rustykrab-deploy`

A deployment crate parses the trusted repository contract and exposes typed
drivers behind a `DeploymentBackend` interface:

- validate a contract without side effects;
- resolve build and release artifacts for an exact merged SHA;
- create or recover an idempotent deployment attempt;
- observe rollout state;
- execute declared health and version checks;
- promote a staged version; and
- roll back to a recorded last-known-good version.

The initial remote driver is GitHub Actions: observe a named merge-triggered
workflow or dispatch a named workflow, collect its run and artifact identities,
and verify its conclusion. Additional drivers are separate implementations,
not conditionals added to the agent prompt.

The deployment crate receives narrowly scoped credentials after policy grants
the action. It does not expose those credentials, raw deployment APIs, or a
general remote shell to planner, executor, or verifier agents.

### 7.9 Self-deployment supervisor

RustyKrab self-management uses a small, separately installed
`rustykrab-supervisor` process. It has no model provider, tool registry, GitHub
merge authority, or planning logic. Its authority is limited to replacing the
configured RustyKrab installation and restarting its service.

For one self-deployment it must:

1. accept a durable intent containing the expected release, source SHA,
   contract hash, artifact hashes, and current last-known-good version;
2. download or receive the already selected artifact;
3. verify checksum, signature, notarization/attestation, bundle identity, and
   expected source version;
4. preserve the current app and, when required, a compatible data snapshot;
5. stop the daemon, atomically swap the staged app, and restart the
   LaunchAgent or service;
6. poll health plus an endpoint that reports the running source SHA;
7. mark the new version last-known-good only after the stability window; or
8. stop it, restore the prior app/data, restart, and record rollback.

The existing macOS installer already preserves the previous app bundle; the
supervisor turns that passive backup into a verified rollback path. Database
migrations in an autonomous self-deployment must be backward-compatible with
the previous binary or include a supervisor-managed snapshot/restore plan.

The delivery run remains in the shared durable store. After restart, the new
daemon resumes the run, reads the supervisor's receipt, verifies that its own
source SHA matches the deployed intent, and only then records `deployed`.

### 7.10 `rustykrab-assurance`

A continuous-assurance crate owns behavioral contracts, probe scheduling,
runtime fingerprints, baselines, comparison, findings, and response policy.
It is a deterministic controller with typed probe drivers; model calls are
limited to explicitly declared judge or diagnosis steps.

Responsibilities:

- parse, hash, and version the assurance contract;
- schedule cheap probes continuously and expensive suites during idle budget;
- create isolated canary identities and enforce cleanup;
- capture real outcomes and attribute them to runtime fingerprints;
- consume `rustykrab-dream` signal-quality reports rather than duplicating its
  outcome analysis;
- compare rolling windows against release and known-good baselines;
- expire stale evidence and maintain `unknown` / `healthy` / `degraded` state;
- emit typed findings with severity, confidence, reproduction, and ownership;
- request an authorized rollback or capability quarantine; and
- translate a grounded repository defect into a project finding and proposed
  remediation slice.

The scheduler yields expensive work to live traffic, matching the existing
DreamWorker design. Cheap external liveness and deadman checks remain outside
the daemon so the system can detect that RustyKrab or its in-process assurance
worker is no longer running.

## 8. Durable data model

Add store modules and migrations for:

```text
projects
  id, repository, title, status, current_revision, judgment_policy,
  created_at, updated_at

project_revisions
  id, project_id, parent_revision, author_kind, conversation_id,
  source_message_id, summary, created_at

plan_nodes
  id, project_id, node_type, title, body, status, confidence,
  introduced_revision, superseded_revision, provenance

plan_edges
  id, project_id, from_node, relation, to_node,
  introduced_revision, superseded_revision

readiness_assessments
  id, project_id, project_revision, candidate_slice_id, dimensions,
  blockers, recommendation, created_at

slice_proposals
  id, project_id, project_revision, outcome_node, included_nodes,
  excluded_nodes, status, proposed_at, authorized_at

delivery_manifests
  id, slice_proposal_id, manifest, manifest_hash, policy_hash,
  deployment_contract, deployment_contract_hash,
  assurance_contract, assurance_contract_hash,
  repository, base_ref, base_sha, created_at

delivery_runs
  id, manifest_id, status, trunk_ref, trunk_sha, stack_provider, stack_id,
  blocked_reason, created_at, started_at, finished_at, version

delivery_layers
  id, run_id, ordinal, parent_layer_id, objective, acceptance, status,
  branch, worktree, base_sha, head_sha, risk_class, pr_number, pr_url,
  remote_check_state, verification_generation, merged_sha

delivery_work_items
  id, run_id, layer_id, ordinal, objective, acceptance, scope, risk, status,
  lease_owner, lease_expires_at, attempt_count, result_commit

delivery_dependencies
  work_item_id, depends_on_id

delivery_attempts
  id, work_item_id, role, status, conversation_id, started_at,
  finished_at, result, error

delivery_evidence
  id, run_id, layer_id, work_item_id, scope, kind, criterion_id,
  base_sha, head_sha,
  command, exit_code, artifact_path, artifact_hash, created_at

delivery_findings
  id, run_id, verifier_attempt_id, severity, status, path, line,
  description, resolution

verification_skill_snapshots
  id, repository, base_sha, source, skill_id, version, content_hash,
  publisher, contract, status, created_at

verification_skill_selections
  id, run_id, layer_id, generation, skill_snapshot_id, reason,
  mandatory, selected_at

verification_skill_uses
  id, selection_id, attempt_id, verifier_fingerprint, proposed_checks,
  findings, cost, started_at, finished_at

verification_skill_proposals
  id, skill_snapshot_id, dream_cycle_id, delta, evidence, status,
  branch, pr_url, created_at

verification_skill_experiments
  id, proposal_id, corpus_hash, baseline_metrics, candidate_metrics,
  shadow_metrics, decision, created_at

delivery_policy_decisions
  id, run_id, action, decision, rule, inputs_hash, created_at

deployments
  id, run_id, repository, environment, merged_sha, contract_hash,
  driver, status, artifact_id, artifact_hash, previous_version,
  deployed_version, idempotency_key, started_at, finished_at

deployment_checks
  id, deployment_id, ordinal, driver, target, expected, observed,
  status, started_at, finished_at, artifact_path, artifact_hash

deployment_receipts
  id, deployment_id, action, actor, source_sha, installed_version,
  previous_version, status, payload, signature, created_at

assurance_contracts
  id, repository, environment, source_sha, contract_hash, contract,
  status, probation_started_at, probation_finished_at, created_at

runtime_fingerprints
  id, repository, environment, source_sha, artifact_hash, provider,
  model_version, tool_schema_hash, config_hash, skill_hash, memory_generation,
  os, first_seen_at, last_seen_at

assurance_probes
  id, contract_id, probe_key, driver, interval_seconds, signal_class,
  severity, isolation, freshness_seconds, response_policy

assurance_probe_runs
  id, probe_id, fingerprint_id, evaluator_fingerprint, status, repetitions,
  successes, observed, confidence, artifact_path, artifact_hash,
  started_at, finished_at, expires_at

assurance_baselines
  id, repository, environment, probe_key, fingerprint_id, window_start,
  window_end, observations, value, confidence, promoted_at

assurance_findings
  id, repository, environment, fingerprint_id, probe_key, status, severity,
  confidence, baseline_id, reproduction, owner_kind, owner_id,
  first_seen_at, last_seen_at, consecutive_failures

assurance_actions
  id, finding_id, action, policy_decision_id, status, deployment_id,
  remediation_run_id, idempotency_key, created_at, finished_at

delivery_events
  id, run_id, sequence, kind, payload, created_at
```

Use optimistic versioning on run and layer transitions and leases on work
items. Increment `verification_generation` whenever a layer or any ancestor is
rewritten; evidence from older generations cannot satisfy a gate. On restart,
expired leases return to `ready` with the existing worktree and last commit
intact. Unlike delegated chat tasks, an interrupted delivery should be
resumable rather than immediately failed.

Deployment transitions use the same optimistic versioning. The idempotency key
is unique, supervisor receipts are append-only, and `deployed` requires a
successful receipt plus all required health checks for the expected merged
SHA. A rollback receipt prevents the failed version from being selected as
last-known-good by a later run.

Probe evidence expires at its declared freshness boundary. The assurance state
is derived from non-expired required probes and cannot be stored as an
unqualified permanent `healthy` bit. Findings and actions are idempotent, and a
remediation delivery records the originating finding so later outcomes can be
attributed back to the attempted fix.

## 9. API and user surface

Initial API:

- `POST /api/projects` — create a project and its canonical planning
  conversation;
- `GET /api/projects/{id}` — current project revision and planning summary;
- `POST /api/projects/{id}/turns` — continue the planning conversation;
- `GET /api/projects/{id}/projections/{view}` — brief, roadmap, decisions,
  questions, risks, architecture, behavior catalog, or next-slice view;
- `GET /api/projects/{id}/revisions/{revision}` — inspect or compare immutable
  planning revisions and provenance;
- `POST /api/projects/{id}/decisions/{decision}` — accept, reject, revise, or
  delegate a proposed decision;
- `POST /api/projects/{id}/slices/{slice}/authorize` — authorize a proposed
  slice and freeze its internal delivery manifest;
- `POST /api/deliveries` — start a previously authorized slice;
- `GET /api/deliveries/{id}` — state, progress, budget, and current blocker;
- `GET /api/deliveries/{id}/events` — resumable SSE event stream;
- `GET /api/deliveries/{id}/report` — final evidence and policy report;
- `GET /api/deliveries/{id}/deployment` — merged SHA, artifact provenance,
  rollout, health, running version, and rollback state;
- `GET /api/assurance/{repository}/{environment}` — current fingerprint,
  contract, freshness, probation, behavioral state, and probe summary;
- `GET /api/assurance/{repository}/{environment}/findings` — active and
  historical regressions, evidence, actions, and remediation deliveries;
- `POST /api/deliveries/{id}/cancel` — stop future work and terminate the
  current lease;
- `POST /api/deliveries/{id}/resume` — resume only when the existing policy
  already permits the required action.

The primary UI is the canonical project conversation. Alongside it, the user
can inspect the current understanding, roadmap, decisions, open questions,
assumptions, risks, project revision history, and proposed next slice. During
execution it also shows the frozen slice objective, GitHub stack map, per-layer
work graph, live state, incremental diff summary, verification evidence,
remaining budgets, every PR link, GitHub merge result, deployment environment,
artifact, running version, health evidence, rollback result, and exact reason
for any block. The same project timeline shows continuing assurance state,
probe freshness, baseline comparisons, runtime fingerprint, active findings,
quarantine, rollback, and remediation-slice links. It does not need to become an
IDE or expose the internal manifest as a form.

## 10. Implementation phases

### Phase 1 — Durable conversational project model

Add currently failing planning E2E scenarios plus `rustykrab-projects`. Persist
projects, immutable revisions, typed plan nodes and edges, provenance,
decisions, questions, assumptions, and risks. Add deterministic project brief,
roadmap, decision-log, and open-question projections.

**Exit:** a vague project idea can become a versioned project model; a decision
and its source conversation survive daemon restart and context compaction; a
later correction supersedes rather than erases history.

### Phase 2 — Planning orchestration and readiness

Extract `rustykrab-runtime` and connect one canonical project conversation to
the project application service. Add repository observations, material-change
review, delegated-judgment policy, per-slice readiness, and natural-language
next-slice proposals.

**Exit:** the orchestration agent inspects a repository, asks one consequential
question, records its answer, leaves future questions open, and proposes one
traceable slice without asking the user to author a schema.

### Phase 3 — Frozen execution contract and durable controller

Compile an authorized slice into an immutable internal `DeliveryManifest` and
`StackManifest`. Add delivery and layer states, store API, leases,
checkpointing, event log, policy fixtures, restart reconciliation, and a
synthetic controller.

**Exit:** conversational authorization freezes the exact project revision and
authority snapshot; a synthetic three-layer delivery survives daemon restart,
resumes expired work, and cannot skip or misattribute verification.

### Phase 4 — Isolated workspace and deterministic verification

Implement `WorkspaceBackend`, one linear local branch/worktree per layer,
worktree-rooted capabilities, incremental and cumulative verification
manifests, artifact capture, scope checks, ancestor-aware evidence invalidation,
and base-pinned verification-skill snapshots.

**Exit:** a scripted executor can produce three functional local stack layers,
each with a focused diff and verified cumulative head; forbidden paths, broken
intermediate states, and false completion are rejected.

### Phase 5 — Autonomous implementation and repair

Add delivery decomposition and executor roles backed by fresh runtime
conversations. Partition the frozen slice into functional PR layers, materialize
each work DAG, dispatch ready work, persist attempt results, and run bounded
repair loops. Start with one layer at a time; add safe speculation on upper
layers only after restacking semantics are proven.

**Exit:** after conversational slice authorization, the system completes a
multi-item real change as a locally verified stack of focused branches without
intermediate user interaction and resumes correctly after an injected restart.

### Phase 6 — Repository verification skills and independent verification

Add the repository-verifier and initial specialist skill packs, applicability
selection, fresh verifier contexts, ephemeral generated-test isolation,
structured findings, per-layer and global acceptance-to-evidence mapping,
test-quality checks, layer-placement review, repair handoff, and cascading
invalidation/re-run rules.

**Exit:** seeded omissions, fake tests, weakened assertions, scope creep, and
stale evidence prevent `pr_ready`; valid repairs eventually pass.

### Phase 7 — Pull requests and CI

Implement stack-aware Bash profiles and the native-command journal. Have
stack-coordinator model sessions use `gh stack init` / `add` / `submit`,
idempotent PR-stack publication, required-check observation, cascading
rebase/push/sync, and exact parent/head-SHA gating. Deny alternate raw push and
`gh pr` mutation paths.

**Exit:** the system creates a linked native GitHub stack, updates it without
duplicating PRs, shows one focused diff per layer, waits for `CI Pass` on every
layer, and never marks a different head or parent SHA verified.

### Phase 8 — Policy-controlled auto-merge

Add per-layer risk classification, protected-path rules, diff budgets,
eligible-prefix policy, native stack merge/queue integration, partial-merge
recovery, and post-merge confirmation.

**Exit:** an eligible low-risk stack merges without human input; a stack with a
high-risk middle layer lands only its eligible lower prefix, and every excluded
or failing layer remains open or becomes `blocked` with an auditable policy
decision.

### Phase 9 — Repository-defined deployment

Add the deployment-contract schema and hash freezing, `rustykrab-deploy`, the
GitHub Actions driver, artifact provenance, idempotent deployment attempts,
health checks, rollout timeouts, and rollback receipts. Use a fake deployment
driver in deterministic E2E and a fixture workflow for live integration.

**Exit:** after GitHub merges a verified fixture stack, the controller follows
the repository contract, resolves an artifact to the merged SHA, deploys it,
proves the running version healthy, and records `deployed`. A seeded unhealthy
rollout restores the last-known-good version and records `rolled_back` without
user input.

### Phase 10 — Continuous behavioral assurance

Add `.rustykrab/assurance.toml`, `rustykrab-assurance`, typed probe drivers,
runtime fingerprints, expiring evidence, probation, rolling baselines,
DreamWorker outcome integration, signal-quality gates, findings, quarantine,
recent-release rollback, and remediation-delivery creation. Enable outcome
capture by contract for managed deployments and keep expensive evaluations
idle-gated and budgeted.

**Exit:** a deployed fixture continuously transitions from `unknown` through
probation to `healthy`; source, provider, model, configuration, and skill drift
create new fingerprints; stale monitoring returns to `unknown`; a seeded
critical release regression rolls back; and a persistent grounded regression
outside probation produces a new verified remediation PR stack. Proxy-only
evidence never mutates the system.

### Phase 11 — Adaptive verification improvement

Extend Dreaming attribution to exact verification-skill versions and repository
fingerprints. Add read-only missed-defect, false-positive, cost, and flake
analysis; bounded learned-strategy deltas; held-out replay and mutation suites;
skill-improvement PR publication; shadow comparison; probation; and rollback.

**Exit:** a repository verifier misses a seeded ground-truth defect, Dreaming
creates one small improvement PR, held-out replay rejects always-fail behavior,
and a genuinely better candidate promotes after shadow probation without
weakening the prior or platform gates.

### Phase 12 — RustyKrab manages and deploys RustyKrab

Add `rustykrab-supervisor`, signed deployment intents and receipts, an endpoint
reporting the running source SHA, staged app installation, stability windows,
LaunchAgent restart, automatic bundle rollback, and migration compatibility
checks. Make the RustyKrab release path stack-aware: version and changelog
changes belong in the verified stack, and the post-merge workflow builds and
tags the immutable merged SHA exactly once without amending or force-pushing
`main`. Publish the source SHA in artifact metadata. Add a trusted
`.rustykrab/delivery.toml` for this repository that uses that GitHub release
workflow and signed macOS artifact. Add a trusted assurance contract for the
gateway, model/tool loop, credential boundaries, scheduled work, memory, and
each enabled channel.

**Exit:** the running RustyKrab instance plans and produces a functional native
GitHub stack for its own repository, verifies it, asks GitHub to merge it,
observes the release artifact, hands installation to the supervisor, restarts
into the expected SHA, resumes the same durable delivery, passes health checks,
marks itself deployed, and enters continuous probation against the old and new
assurance contracts. A broken candidate automatically returns to the previous
signed bundle and the prior daemon reports the failed rollout.

### Phase 13 — Long-horizon hardening

Add budget accounting, flaky-check classification, queue fairness, workspace
retention, crash and network fault injection, base-branch churn tests, node
execution, deployment concurrency locks, artifact retention, supervisor crash
recovery, rollout fault injection, canary cleanup, monitor deadman tests,
baseline poisoning tests, evaluator drift detection, assurance cost controls,
and operational metrics.

**Exit:** deliveries remain correct across repeated restarts, transient CI
outages, base changes, verifier repair cycles, bounded remote-node failure,
interrupted installation, failed health checks, and rollback restart.
Behavioral assurance remains stable under flaky probes, insufficient samples,
provider drift, monitor restart, and adversarial attempts to weaken its own
contract.

## 11. Required E2E scenarios

### 11.1 Conversational planning

P1. **Vague beginning:** an incomplete project idea becomes a useful brief and
    roadmap without requiring structured user input.
P2. **Material question:** the orchestrator asks one consequential question and
    records the answer as a provenance-bearing decision.
P3. **Restart and compaction:** project understanding, decisions, assumptions,
    and open questions reconstruct after restart and context compaction.
P4. **Repository contradiction:** source inspection challenges an assumption and
    creates a visible project revision.
P5. **Decision correction:** a user correction supersedes a decision without
    erasing the old rationale or source.
P6. **Progressive readiness:** an unresolved future question does not block a
    sufficiently understood current slice.
P7. **Delegated judgment:** a reversible implementation decision is made inside
    standing policy and records its authority basis.
P8. **No inferred authority:** silence cannot authorize a security, destructive
    data, merge, deployment, or recurring-cost decision.
P9. **Projection consistency:** brief, roadmap, decision log, and slice proposal
    derived from one revision agree about scope and outcomes.
P10. **Slice traceability:** every proposed acceptance behavior traces to project
     state, evidence, and the conversation that shaped it.
P11. **Frozen execution:** authorization freezes a revision and authority
     snapshot; later conversation cannot mutate the active delivery.
P12. **Result reconciliation:** delivery evidence updates milestone state,
     challenges assumptions where necessary, and recomputes next-slice readiness.

### 11.2 Delivery, deployment, and assurance

S1. **Stack-only construction:** an execution model cannot create a publication
    branch outside the assigned native stack; `gh stack add` succeeds within the
    manifest boundary.
S2. **Stack-only publication:** raw `git push`, `gh pr create`, `gh pr merge`,
    and generic mutating `gh api` calls are denied while native `gh stack
    submit` succeeds for the assigned repository.
S3. **Native model execution:** a stack-coordinator model uses Bash and native
    `gh stack init` / `add` / `submit`; the journal and postcondition validator
    observe the resulting stack without substituting a model-facing wrapper.
S4. **Credential isolation:** the model can run an approved credentialed stack
    command but cannot print, reuse, or pass the credential to another process.
S5. **Merge-prefix confinement:** a merge-operator model cannot merge beyond the
    deterministically authorized PR even if it supplies a different argument.

1. **Authorized slice to native stack:** a conversationally authorized fixture
   slice becomes three linked
   GitHub PRs, each with focused functionality, a layer-only diff, and evidence
   for its acceptance criteria.
2. **Restart recovery:** terminate the daemon during execution and verification;
   the same run resumes without duplicated commits or lost evidence.
3. **False completion:** the executor calls `task_complete` without the required
   behavior; verification rejects it.
4. **Repair loop:** an initial failing implementation receives structured
   findings, repairs them, reruns invalidated checks, and succeeds.
5. **Scope confinement:** a change to a forbidden path terminates the attempt
   and cannot be committed or pushed.
6. **Test tampering:** deleting or weakening a required test is detected unless
   the frozen delivery manifest explicitly calls for that change.
7. **Stale evidence:** changing one layer after tests pass invalidates that
   layer and every rebased descendant.
8. **Lower-layer repair:** a finding on layer 1 is repaired, layers 2 and 3 are
   cascaded through rebase, and all changed SHAs are reverified.
9. **Trunk movement:** a new trunk commit forces `gh stack rebase`, push, and
   re-verification of the complete affected stack.
10. **Non-interactive divergence:** different local and remote stack shapes
    block without choosing or overwriting either history.
11. **CI failure:** a failing required remote check on any required lower layer
    prevents merging layers above it.
12. **Risk exclusion:** a security-sensitive middle layer can become PR-ready
    but cuts the automatically mergeable prefix below it.
13. **Autonomous stack merge:** an eligible low-risk stack passes all exact-SHA
    gates and merges through trunk branch protection.
14. **Partial merge recovery:** an interrupted stack merge preserves the landed
    prefix, syncs and restacks the suffix, and reverifies it before retrying.
15. **Budget exhaustion:** repeated repair failure ends in `failed` with every
    attempt and artifact available.
16. **Cancellation:** cancellation stops the active lease, prevents remote
    mutation, and leaves a diagnosable retained workspace.
17. **Credential isolation:** executor and verifier cannot read source-control
    or merge credentials.
18. **Trusted deployment contract:** a stack that edits
    `.rustykrab/delivery.toml` cannot use the edited contract during the same
    run.
19. **Exact merge observation:** a deployment cannot start until GitHub's trunk
    contains the intended stack commits and the controller records the
    resulting merged SHA.
20. **Idempotent deployment:** restart the controller repeatedly during rollout;
    only one deployment attempt reaches the backend.
21. **Artifact provenance:** an artifact built from a different SHA or with an
    invalid signature is rejected before installation.
22. **Health rollback:** install a candidate that starts but fails its health
    or version check; the backend restores and verifies the last-known-good
    version.
23. **Self-deployment handoff:** replace a fixture daemon through the external
    supervisor; the new process resumes the same delivery and records its
    expected source SHA.
24. **Self-deployment crash:** kill the supervisor during each stage of swap and
    restart; recovery selects either the complete new version or the complete
    prior version, never a mixed installation.
25. **Migration incompatibility:** a self-update with no backward-compatible
    migration or data restore plan is not deployment-eligible.
26. **Probation contract union:** a candidate that removes a critical probe is
    still evaluated by the pre-deploy contract and cannot weaken its own
    probation gate.
27. **Continuous release regression:** a deterministic canary fails repeatedly
    inside the rollback window; the last-known-good version is restored and
    the failed fingerprint is retained for diagnosis.
28. **Persistent remediation:** a grounded regression outside the rollback
    window creates exactly one project finding and remediation-slice proposal
    with the reproduction and failing evidence, then follows the normal verified
    stack pipeline.
29. **Proxy restraint:** judge-only and implicit-signal degradation creates a
    finding but cannot roll back, quarantine, edit code, merge, or deploy.
30. **Stale monitor:** stop the probe scheduler or external heartbeat; the
    assurance state expires to `unknown` rather than remaining healthy.
31. **Runtime drift:** change only the model, provider, configuration, skill, or
    memory generation; a new fingerprint and comparison cohort are created
    without pretending the code release changed.
32. **Canary isolation:** synthetic tasks cannot see production credentials or
    data, and disposable side effects are cleaned after success, failure, and
    cancellation.
33. **Hysteresis:** one flaky failure does not cause oscillating rollback and
    redeploy; the declared consecutive-failure and cooldown rules hold.
34. **Monitor deadman:** kill RustyKrab and its in-process assurance worker; the
    external supervisor detects stale heartbeats and reports loss of assurance.

### 11.3 Adaptive verification skills

V1. **Repository-specific discovery:** a pinned skill selects an architecture or
    migration check not present in the generic command set.
V2. **Evidence over claim:** a skill reports success but missing hard-gate
    evidence prevents `verified`.
V3. **Self-edit isolation:** a candidate verification-skill edit is judged by
    the trusted base version and applies only to later deliveries.
V4. **Generated-test isolation:** an ephemeral verifier test catches a defect,
    is stored as exact-SHA evidence, and cannot silently mutate production code.
V5. **Improvement attribution:** an escaped ground-truth defect is attributed to
    the exact repository, skill hash, verifier fingerprint, and change class.
V6. **Balanced replay:** a Dreaming proposal that catches more defects by
    rejecting all correct changes fails held-out evaluation.
V7. **Shadow promotion:** a grounded additive strategy passes replay and shadow
    probation before becoming eligibility-affecting.
V8. **Skill rollback:** worse ground-truth outcomes during probation restore the
    previous skill pack without removing the underlying evidence.

## 12. First vertical slice

The first useful demonstration is conversational and intentionally omits code
mutation:

> Start with a vague project idea in an ordinary conversation; inspect a local
> fixture repository; develop two options; record one consequential user
> decision and one delegated reversible decision; preserve both across a forced
> daemon restart and context compaction; leave an irrelevant future question
> unresolved; and propose one bounded execution slice whose outcomes,
> acceptance behavior, assumptions, authority, and provenance the user can
> inspect and authorize without seeing a schema.

The next demonstration authorizes that slice, compiles its internal manifest,
and uses local stack branches while intentionally omitting remote GitHub
mutation and merge. It implements three functional PR layers, survives another
forced restart, verifies every incremental diff and cumulative head, has a
fresh verifier reject one seeded omission in layer 2, repairs it,
cascades/reverifies layer 3, and reconciles the result back into the project
roadmap.

The following demonstration runs `gh stack init` / `add` / `submit` against a
fixture GitHub repository and proves that the same local manifest becomes a
linked native stack without changing the verified layer boundaries. Auto-merge
then becomes a controlled integration feature rather than a substitute for
correctness.

The first complete product demonstration continues from that stack: GitHub
atomically merges it, a trusted fixture deployment contract selects the merged
artifact, the deployment backend promotes it, and health/version checks prove
the running service matches the merge. The assurance controller then completes
probation, continuously runs a deterministic behavior canary, detects a seeded
regression, and creates a new remediation stack. The final self-hosting
demonstration runs the same lifecycle against RustyKrab and delegates process
replacement and external deadman monitoring to `rustykrab-supervisor`.

## 13. Success criteria

The product is ready for unattended use when:

- a user can begin with an incomplete idea, develop it through one coherent
  project conversation, and inspect an evidence-backed current understanding,
  roadmap, decisions, assumptions, open questions, and next-slice proposal;
- the user never has to author an internal spec or YAML file, and future
  uncertainty does not prevent a sufficiently understood near-term slice from
  proceeding;
- after conversational or standing-policy authorization, that slice becomes a
  native GitHub stack of focused, functional, verified PRs, a GitHub merge, and
  a repository-verified deployment without intermediate user interaction;
- every delivery and assurance result reconciles into the project model and
  informs the next planning conversation and slice-readiness assessment;
- every layer is independently understandable and every claimed acceptance
  criterion is backed by evidence on its exact parent/head SHA pair;
- every cumulative layer head passes required repository checks, so any
  partially merged prefix is functional;
- daemon restarts and transient external failures do not lose or duplicate
  work;
- executors cannot widen their filesystem, tool, credential, network, or merge
  authority;
- independent verification catches seeded incomplete and misleading changes;
- repositories and setups can version their own verification skill packs while
  the coded kernel retains evidence integrity and verdict authority;
- a candidate cannot weaken the verification skills, mandatory checks, outcome
  definitions, or held-out corpus used to judge that same candidate;
- Dreaming can propose, replay, shadow, promote, and roll back bounded verifier
  improvements using ground-truth outcomes without rewarding an always-failing
  verifier;
- merge decisions are deterministic, reviewable, and separate from model
  judgment;
- GitHub, rather than a local shell, performs every trunk merge;
- stack-coordinator and merge-operator models use native `gh stack` through
  Bash, while alternate raw publication paths and out-of-scope stack mutations
  are structurally denied;
- repositories determine their deployment driver and success/rollback
  contract through trusted versioned configuration;
- every deployed artifact is bound to the GitHub merge SHA and verified in its
  running environment;
- unhealthy deployments automatically restore and verify a last-known-good
  version;
- every managed deployment has a time-bounded assurance state derived from
  fresh behavioral evidence rather than a permanent health bit;
- code, provider, model, configuration, skill, memory, dependency, and
  environment drift are distinguishable in the runtime fingerprint;
- grounded regressions create reproducible remediation deliveries, while
  insufficient or proxy-only evidence cannot mutate production;
- continuous probes respect isolation, cost, freshness, hysteresis, and
  cleanup contracts;
- RustyKrab can update its own repository and executable without allowing the
  replaced process to supervise or falsely certify its replacement; and
- blocked and failed runs preserve enough structured evidence that a human can
  understand the issue without reconstructing the execution transcript.

## 14. Recommended initial policy

Roll authority out in stages:

1. **Stack-ready:** create verified native GitHub stacks but do not merge.
2. **Merge:** allow eligible low-risk stacks to merge through GitHub after the
   Phase 7 suites are green.
3. **Non-production deploy:** execute trusted repository contracts in staging
   after the Phase 8 provenance and rollback suites are green.
4. **Observe-only assurance:** run behavioral contracts in staging and then
   production, but only record findings until signal quality and isolation are
   proven.
5. **Production deploy:** grant each repository/environment separately after a
   successful staging and observe-only history.
6. **Automatic response:** enable recent-release rollback, narrow quarantine,
   and remediation-stack creation independently, only after their Phase 9
   evidence and hysteresis suites are green.
7. **Self-deploy:** enable RustyKrab's own contract only after the external
   supervisor, migration, restart-resume, and rollback suites are green.

Release, deployment, rollback, quarantine, remediation creation, and
self-replacement remain separate capability grants even after all stages are
enabled.

## 15. GitHub feature contract

This plan relies on GitHub's native stacked-pull-request public preview and the
native `gh stack` command family. The implementation must feature-detect this
contract and fail closed if an installed version lacks a required
non-interactive operation.

Primary references:

- [Stacked pull request rules and merge semantics](https://docs.github.com/en/pull-requests/reference/stacked-pull-requests)
- [Creating stacks with `gh stack`](https://docs.github.com/en/pull-requests/how-tos/create-pull-requests/creating-stacked-pull-requests)
- [Cascading rebase, push, and sync](https://docs.github.com/en/pull-requests/how-tos/create-pull-requests/managing-stacked-pull-requests)
- [Using existing branch chains with native stacks](https://docs.github.com/en/pull-requests/reference/use-other-tools-with-stacked-pull-requests)
