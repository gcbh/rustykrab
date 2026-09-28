# Plan: Control Layer, Named Workers, and Self-Filed Improvements

**Status:** Proposed (for review)
**Date:** 2026-09-24
**Repository:** `gcbh/rustykrab`
**Builds on:** `autonomous-software-delivery.md`,
`conversational-project-planning.md`, `adaptive-verification-skills.md`,
`DREAMING.md`

## 1. Product promise

RustyKrab is a self-evolving control layer. The user tasks it with almost
anything, personal or software, and it is expected to finish the work, to get
better at finishing work, and to bother the user only when it genuinely cannot
resolve something itself. Those three expectations are the contract; everything
below serves them.

### 1.1 Expectations and how they are measured

Each expectation has a metric the system records for itself. The metrics are
the same ones dreaming reads to decide where to improve (section 10), so the
system's definition of "better" is the user's.

| Expectation | Metric | Direction |
|---|---|---|
| Finish what it is given | work items done without user intervention, by kind | up |
| Finish it correctly | verified done versus claimed done; escaped defects | gap to zero |
| Persist before surfacing | resolution rungs exhausted per escalation (section 8) | up |
| Surface rarely, and only when stuck | escalations per completed item; escalations later judged avoidable | down |
| Know what went wrong | failures with a typed error class; unknown-error rate (section 9) | unknown to zero |
| Grow its own capability | capabilities acquired or built that were later used | up |
| Improve on evidence | proposals filed; fraction that moved their target metric | up |
| Stay inside bounds | policy violations, budget overruns, single-writer conflicts | zero |

### 1.2 What the system does

1. Turn a request into durable, typed work items with an objective, a
   definition of done, constraints, and the tools the work needs. A multi-step
   request becomes one graph of items, typed edges and parent links, filed in a
   single planning step and validated in code before any of it runs.
2. Dispatch each item to a named worker of the right kind: a local scoped
   sub-agent, a peer RustyKrab node on the tailnet, or an external coding agent
   such as Claude Code or Codex.
3. Keep sequential, dependent work inside one worker and fan out only work that
   is independent. The graph orders work, waits on the world and fans out; it
   never splits a sequence across workers. Readiness, what a failure, expiry or
   cancellation does to everything downstream, and each parent's roll-up are
   computed in code.
4. Treat every worker result as a claim and verify it from evidence before
   marking anything done.
5. Climb a resolution ladder before asking anyone: retry, repair with new
   information, change worker, acquire the missing capability, build the
   missing tool, improve its own observability, and only then surface.
6. Classify every failure into a typed error; treat an unclassifiable failure as
   a defect in its own observability and file the fix.
7. Ask the user only for decisions outside delegated judgment, for things only
   the user can supply, or when the ladder is exhausted within budget; deliver
   the question through the user's own channels, park the item, resume on the
   answer.
8. Report honestly what was done, what was not, and what is queued, through the
   same channel the work came from.
9. During dreaming, evaluate its own record against the expectations above,
   find where it could improve, and file those findings as proposals a human
   can review.
10. When policy allows, turn an accepted proposal into a work item, execute it
    through the delivery pipeline, and put the result under probation.

RustyKrab is the control layer. Workers do the work. The model owns coherence
and judgment within stated bounds; code owns lifecycle, scheduling, the
resolution ladder, verification and notification.

## 2. Operating assumptions

Each assumption names the evidence behind it. Sources are listed in section 18.

- **Control is code, not a model.** Across seven multi-agent frameworks 41–87%
  of runs failed, mostly on specification, coordination and verification
  (MAST). A small edge model executed free-text deferred intentions at 4–7%
  F1, and 66–70% once a typed store owned the lifecycle (PM-Bench). Anthropic's
  long-running harness lets the agent change only a `passes` flag. The
  delivery plan already takes this position: "the controller verifies the
  commit and changed paths itself."
- **Split only independent work.** Every multi-agent architecture tested lost
  39–70% on sequential planning and gained up to 81% on parallelisable work;
  errors amplified 4–17x (Kim et al.). Dependent steps stay in one worker.
- **The front tool block is fixed for the run; late tools arrive by append.**
  The model does not need its tools up front. qwen3.8 called a tool first seen
  as JSON in a tool result 12 of 12 times, 12 of 12 again among five or ten
  near-miss distractors, and 12 of 12 through a generic `call_tool`; gemma4
  was 72 of 72 until a competing `tools_load` description pulled a third of
  its late-bound calls into a redundant load (section 12). The engine is what
  cares. A change to the tools array rewrites the front of the prompt and
  costs one full re-prefill (6.5 s on gemma4:26b, 35 s on qwen3.8:27b at 7K
  tokens, growing with history); an append costs under a second. So a worker
  starts with the tools its item is known to need, which saves a search round
  trip, and anything found later is delivered as text in a tool result, never
  by re-rendering the tools array. On qwen3.8, which prefills at about 210
  tokens a second, a small starting set with appends on demand beats a large
  set declared just in case: each thousand tokens of schemas costs about five
  seconds whenever that prefix is not already cached.
- **Coding is not reserved for frontier agents; evaluation decides who keeps
  it.** A `code` item may run on a local worker, a peer, Claude Code or Codex.
  The verifier judges each result from evidence, and dreaming keeps a routing
  record per worker and class of coding work (section 10) that moves the
  default tier on verified quality and cost, not on the item's kind. The
  published figures set the prior, not the rule: open-weight 70B-class models
  fully completed 6–7% of realistic office tasks (TheAgentCompany), stock ≤14B
  models flag a missing tool 0–42% of the time, and the 30B MoE class
  RustyKrab runs scores 10.5 (BFCL V4). So local workers start coding on
  small, well-specified slices under probation, and earn wider classes only
  when their verified record says so.
- **Blocked states are typed.** Models' progress reports are unreliable and
  small models confabulate around gaps, so the controller infers state from
  reason codes and evidence, never from prose.
- **The local store is the source of truth; GitHub is the review surface.**
  A scheduler needs leases, dependency edges and readiness; issues have none.
  Humans need a place to review engineering work; issues are that place.
  Personal and research work reaches the user through the channel and the
  CLI and never becomes an issue (section 11).
- **One daemon, Apple Silicon, Ollama by default; peers over Tailscale.**
  Two 18 GB models cannot be resident together in 36 GB, so per-role local
  models mean the same model with different toolsets, or a remote worker.
- **The user delegates judgment in ordinary language**, as the planning plan's
  standing-judgment policy already specifies, and every delegated decision is
  recorded with alternatives and rationale.

## 3. Existing foundation and gaps (at `cd61a88`)

Already in the tree:

- `rustykrab-runtime` owns the turn lifecycle outside HTTP (#598), with
  `RunOptions.max_iterations` and `denied_tools`.
- `rustykrab-projects` persists revisioned plans, decisions, questions and
  provenance over REST (#607–#609); eight planning scenarios are `xfail`.
- `delegated_tasks` is a durable peer-task queue (#566/#567): one worker,
  `Queued → Running → Done | Failed | Cancelled`, a `principal`, a
  `hop_budget`, an `allowed_tools` ceiling and a `trace_id`. Results are text.
  A restart fails in-flight tasks.
- The `nodes` tool discovers, lists and sends messages to paired nodes; it
  cannot narrow a delegated task's tools.
- `credential_request` files a durable request, ends the turn cleanly through
  `Tool::blocks_turn()`, and `TaskSource::CredentialFulfilled` resumes the same
  conversation when the credential arrives. This is the one typed blocked state
  that exists.
- `SubagentRunner` runs a fresh, capability-restricted conversation
  synchronously inside the parent's tool call, gated by
  `RUSTYKRAB_ENABLE_SUBAGENTS` and a capability. `AgentDefinition` carries an
  id, description, system prompt, profile and `allowed_tools`; the built-in
  researcher, coder and planner all inherit every tool. Definitions are
  code-only. `sessions_*` are stubs.
- `ActiveToolsRegistry` decides which schemas the model sees; the meta tools
  and a six-tool seed are visible from turn 0 and `activate(conversation_id,
  names)` is the host-side hook. The executor already runs any registered,
  permitted tool by name.
- The cron `TaskQueue` starts fresh conversations, resolves a skill for the
  task, forces a first tool call and tells the model to search memory first.
- Dreaming: outcome capture (P0), idle-gated report-only analysis with stored
  reports (#615), memory consolidation with a real reversal path (#618),
  "verifiable" signals that require the world to have changed (#619), and
  nightly evals that state what the loop still owes (#633). Skill-improvement
  proposals and their review surface are the remaining piece of
  `DREAMING.md`'s table; that document's status line predates #615–#633 and
  should be refreshed as part of this plan.
- The e2e harness boots the real daemon, seeds tools per scenario, and
  represents unbuilt behaviour as `xfail`.

Since `cd61a88` (state at 2026-09-25, from the M1 to M4 cutover and the
migration session's export in `research_notes/M4 migration session/`):

- #661 merged (e316136): batched tool calls re-enter the session task-locals,
  which fixes `tools_list`, `tools_load`, `todo_write` and credential-link
  delivery inside multi-call batches.
- #662 merged (689856c): an empty reply after the `task_complete` reminder
  delivers the prior answer instead of erroring.
- #663 open: removes the `task_complete` reminder, the "Continue." nudge on
  the output limit, and the reflection prompt, with `Tool::blocks_turn`.
- #659 open, verified: install.sh forwards `OLLAMA_TIMEOUT_SECS`,
  `RUSTYKRAB_MAX_CONTEXT_TOKENS` and `RUSTYKRAB_OUTCOME_CAPTURE`.
- Decided, not yet written: every non-leading system message becomes a
  `[System notice]` user turn in the Ollama and OpenAI adapters, as the
  Anthropic adapter already does; thinking off for the compaction summary
  call; a staged-compaction design doc.

Gaps this plan closes:

- no work-item type that spans personal and software work, and no single
  place where work is tracked: cron jobs, peer tasks and planning slices are
  separate queues;
- no worker identity, registry or capability advertisement;
- no executor kind other than a RustyKrab conversation;
- no typed blocked state beyond credentials, and no question router;
- sub-agent toolsets are a ceiling, never a visible set activated up front;
- delegated tasks return text and die on restart;
- the todo list is in-memory, so a follow-up the model files is gone at
  restart;
- nothing that holds work has dependency edges, hierarchy or fan-in, so a
  multi-step request cannot outlive the conversation that holds its plan;
- dreaming has nowhere to put an improvement a human could accept; and
- no evaluation of any of the above exists.

## 4. Work items: one model for personal and software work

A work item is the unit the controller schedules. The same type serves "book
the dentist", "fix the flaky e2e scenario", "build a tool that reads my
bank's export" and "add the probe that would have classified yesterday's
unknown error"; the `kind` decides which workers qualify and which verifier
applies. `capability` items are second-order work filed by the ladder
(section 8); `internal` items are improvements to RustyKrab's own machinery
(section 9); `proposal` items are dreaming's output (section 10).

Items form a graph. Typed edges order them, a `parent` link groups them, and
`inputs_from` carries one item's results into another's brief. A planning step
builds the graph for a multi-step request and files it in one `work_plan` call
(sections 6.1 and 14.1); the controller accepts the whole graph or rejects it
whole with a typed reason, and nothing in it can be leased before acceptance.

```
WorkItem {
  id, kind: personal | code | research | capability | internal | proposal,
  title, objective, done_when,
  constraints: [String],          // one explicit constraint per entry
  decisions_made: [String],       // choices the originating run already made
  artifact_refs: [Ref],           // message ids, paths, URLs, commit SHAs
  required_tools: [String], required_mcp_servers: [String],
  worker_kind: any | local | peer | claude_code | codex,
  parent: Option<WorkItemId>,     // hierarchy (4.2); a parent's status is rolled up
  edges: [Edge],                  // typed, each naming an upstream item (4.1)
  inputs_from: [WorkItemId],      // fan-in (4.3); default: the `blocks` upstreams
  origin_conversation_id: Option<ConversationId>,
  trigger: now | at(time) | on_credential(name) | on_mcp(server) | on_answer(q),
  preconditions: [Check], expires_at: Option<Time>,
  budget: { iterations, tokens, wall_seconds, repairs, rung_budgets },
  ladder: [RungEvent],            // every rung climbed, with its error and outcome
  last_error: Option<Error>,      // section 9
  priority, status, lease: Option<{ worker, since, ttl }>,
  evidence: [Evidence], events: [Event]
}
Edge { depends_on: WorkItemId,    // the upstream; `item` is the item holding the edge
       kind: blocks | waits_for | conditional_on_failure
           | supersedes | discovered_from }
views: depends_on()      = upstreams over blocks | waits_for | conditional_on_failure
       discovered_from() = the item's one discovered_from edge, if any
status: queued | ready | leased | running | blocked(reason) | verifying
      | done | failed | cancelled(reason) | expired
blocked reasons: needs_credential | needs_decision | needs_consent
      | needs_tool | worker_unavailable | budget_exhausted
      | verification_failed | precondition_failed
      | upstream_failed | upstream_expired     // cascade only, with an origin item
cancelled reasons: requested | cascade | superseded   // cascade, superseded: code only
waiting: queued | ready | blocked      active: leased | running | verifying
closed:  done | failed | cancelled | expired          // final, never rewritten
```

Rules:

- **Readiness is computed, not declared.** An item is ready when every
  ordering edge is satisfied (4.1), its trigger has fired, its preconditions
  hold, it has not expired, and every ancestor lets it through (4.2). An item
  with children is never ready: a parent is verified, not leased. This is the
  Beads `bd ready` rule and the Anthropic harness rule, kept in code.
- **The model proposes, code transitions.** Tools let a model file an item or
  a graph, attach evidence, report a typed result or a typed block. No tool
  lets a model set `status`, and none accepts `upstream_failed`,
  `upstream_expired`, `cascade` or `superseded`: only the controller sets
  those, and each one names the item that caused it.
- **Every field a fresh worker needs is typed.** Handoff summaries lose
  conditional constraints first (about 0.57 survival under a short budget) and
  artifact trails score worst after compaction, so constraints, decisions and
  references are separate fields, never prose.
- **Follow-ups carry provenance.** A `discovered_from` edge and
  `origin_conversation_id` let a worker read the originating conversation's
  recall archive, which is already durable and keyed by conversation id.
- **The graph is for ordering, waiting and fan-out, not for sequences.** An
  edge earns its place when the downstream waits on the world (a trigger, a
  date, a person's answer), needs a different worker kind, tool or writable
  resource, sits behind an approval point, joins independent branches, or is
  a plan B (section 6.1). A sequence one worker can finish in one run stays
  one item (section 2); `work_plan` flags the plainest split with a
  `sequential_split` warning (section 14.1), a rejection once Phase 1 has
  measured its false-positive rate, and section 17 watches repair rates on
  chained items.
- **Code slices remain code slices.** A `code` item that belongs to a delivery
  is a projection of the delivery plan's work item, not a competitor; the
  controller leases it the same way and the delivery verifier decides `done`.
  The delivery compiler builds that graph and has already rejected its cycles
  (delivery plan, section 5.1). The import (section 6.1) runs the same
  validation as `work_plan` in one transaction: the slice as a parent, each
  stack layer as a child parent with its acceptance as `done_when` and a
  `blocks` edge on the layer below, and each delivery work item as a `code`
  child of its layer, with `delivery_dependencies` as `blocks` edges. One
  exception is deliberate: the delivery's rebase cascade (delivery plan,
  section 5.3) sends verified layers back to verification when a layer below
  them changes. Projected items therefore take their transitions from the
  delivery controller, not from 4.5, and are the only items whose `done` can
  be re-opened; whether that exception survives is part of the two-controllers
  decision in section 17.

### 4.1 Edges

Each edge is a row (`item`, `depends_on`, `kind`): `item` is the downstream,
`depends_on` the upstream it names. Three kinds order work; two record history.

| Edge | `item` is ready once the upstream is | Upstream `done` | Upstream `failed` | Upstream `expired` | Upstream `cancelled` |
|---|---|---|---|---|---|
| `blocks` | `done` | satisfied | `blocked(upstream_failed)` | `blocked(upstream_expired)` | `cancelled(cascade)` |
| `waits_for` | closed, whatever the outcome | satisfied | satisfied | satisfied | satisfied |
| `conditional_on_failure` | `failed` | `cancelled(cascade)` | satisfied: the plan B runs | `cancelled(cascade)` | `cancelled(cascade)` |
| `supersedes` | no condition | none | none | none | none |
| `discovered_from` | no condition | none | none | none | none |

- **Failure and expiry hold; cancellation propagates.** A held item can still
  be saved by a re-plan or the user's answer, so failure and expiry of a
  `blocks` upstream hold the dependent rather than cancel it. A cancel is an
  instruction, so it propagates. Expiry is not failure, so it never releases a
  plan B; a step that must run whatever happens is a `waits_for` item.
- **A plan B stands in for what it covers.** A `conditional_on_failure` item
  has no other upstream, and downstream items name the step, never its plan B.
  When the step fails and the plan B is released, the step's `blocks` and
  `waits_for` dependents, and `inputs_from` entries naming it, are re-pointed
  to the plan B instead of taking the `failed` column (section 6.4).
- **A planned fallback outranks surfacing.** When an item with a plan B would
  surface because its ladder budgets are spent, or because only the user can
  meet its need, it fails instead and the plan B runs; a `policy` stop still
  surfaces at once (section 8).
- **`supersedes` acts once, at file time**, on the item it names (4.4). An
  upstream in `cancelled(superseded)` never reaches this table, because
  superseding re-points its dependents to the replacement first.
- **A satisfied edge stays satisfied.** Its upstream is closed, and closed is
  final. Edge cascades therefore reach only `queued` and `blocked` items; the
  one cascade that reaches active work is a parent's cancellation or expiry
  (4.2).

Example, a personal request:

```
P  Lisbon trip, 3-6 May         parent; expires_at 1 May
│                               done_when: flight and hotel booked, in the
│                               calendar, Ana told
├─ A  book a flight             personal
├─ B  book a hotel online       personal
├─ F  book the hotel by email   plan B: conditional_on_failure on B
├─ E  add the trip to calendar  blocked by A, B; inputs_from A, B (the default)
└─ T  tell Ana how it went      waits for A, B; inputs_from A, B
```

A and B are independent and may run on different workers; search-then-book
stays inside each. If B is done, F becomes `cancelled(cascade)` without being
leased. If B fails, F runs and E's and T's edges on B move to F. If A fails,
E becomes `blocked(upstream_failed)` with origin A, T still runs once the
hotel branch closes and tells Ana the flight failed, and P then rolls up to
`blocked` naming A: one message, not one per item. If P expires or is cancelled, whatever is open under it ends
`cancelled(cascade)`.

### 4.2 Hierarchy

`parent` groups items under one objective. A child has one parent, and parent
links form a tree whose depth is capped at file time (section 14.1).

- **A parent is never leased** and holds no resources. Work that must follow
  the children (a summary, a final send) is itself a child with edges on its
  siblings.
- **A parent gates its children.** A child is ready only when every ancestor's
  trigger has fired, its preconditions hold, its ordering edges are satisfied
  and no approval is pending on it (section 6.1), as well as its own. A child's
  `expires_at` is capped at its parent's.
- **A parent's status is rolled up by code** from its subtree in the same
  transaction as every child transition, first match wins:

| Subtree | Parent |
|---|---|
| the parent's own gate is shut (trigger, preconditions, pending approval) | `queued`, or `blocked(needs_consent)` for an approval |
| any descendant ready, leased, running or verifying | `running` |
| otherwise any descendant blocked | `blocked(reason)`: a reason that needs the user first, else the earliest, with its origin |
| otherwise open descendants waiting on edges or triggers | `queued` |
| every child closed | `verifying` its own `done_when` from the children's evidence, then `done`, or `blocked(verification_failed)` for its ladder, whose repair is a re-plan under it (section 6.4) |

- **`done_when` decides, not the children's outcomes.** A parent is `done`
  when every child is closed and its own `done_when` verifies; a failed child
  whose plan B booked the hotel does not fail the parent. The report names
  the failed child.
- **A parent's own `cancelled` or `expired`** comes only from its own
  transition or its own edges, and overrides the table. Either one ends every
  open descendant `cancelled(cascade)`, with the parent as origin: waiting ones
  at once; leased or running ones through the `work cancel` path (lease
  revoked, worker stopped at its next step, partial evidence kept).
  `verifying` ones finish verification and keep its verdict, because the work
  already happened and the record must say whether it landed. Closed
  descendants keep their status.
- **A parent's `budget` is an envelope.** Its children's budgets may not sum
  past what it has left, checked at file time; a re-plan under the parent draws
  on the remainder.
- A parent that is `verifying` or closed takes no new children.

### 4.3 Fan-in

`inputs_from` names the items whose results a downstream item needs.

- **Filled at file time.** When the filing omits it, it is the item's `blocks`
  upstreams. Every entry must be an item the downstream is ordered after,
  directly, transitively or through an ancestor's edges, so its results exist
  at lease time; any other entry rejects the filing (`input_unordered`).
- **Copied at lease time.** The controller copies each input's verified
  evidence refs and `artifact_refs`, its closed status, one line of its result
  summary, and for a failed input its error class, into the brief's `inputs`
  block. Pointers, not bodies: the worker opens what it needs from the store
  and the upstream's recall archive. Only verified evidence flows. A
  `waits_for` upstream is an input only when listed, and then carries its
  closed status and error class, so a step that reports what happened can
  report a failure. A parent input contributes its verification record and its
  done children's refs. The block's shape and caps are section 6.3.
- **Recorded on the lease.** The copied set is stored with the lease, so a
  brief can be rebuilt exactly and evaluation can tell what a worker was
  given.
- **Re-pointed with the edges.** When a plan B is released or an upstream is
  superseded, `inputs_from` follows the re-pointed edges.

### 4.4 Filing, cycles and re-planning

Every filing path is validated the same way: `work_plan`, `work_file`, the
ladder's `capability` items, an accepted proposal and the delivery import. A
filing is atomic: accepted whole or rejected whole with a typed reason
(section 14.1), and nothing from a rejected filing exists afterwards.

- **Cycles are rejected at file time**, as the delivery compiler already does
  for a slice ("The controller rejects dependency cycles", delivery plan,
  section 5.1). The check runs on the graph as it would stand after the
  filing, including edges re-pointed by `supersedes`, over every edge kind and
  parent link. For the check a parent depends on each of its children, and
  each child inherits its parent's ordering edges; an ordering edge between an
  item and its own ancestor or descendant is rejected outright. A cycle is
  rejected with `cycle`, naming the items on it, so the planner can repair it
  in the same run.
- **Edges onto existing items.** A filing may add an ordering edge whose
  downstream already exists only while that item is waiting (queued, ready or
  blocked), else `edge_onto_active`; a ready item that gains an unsatisfied
  edge returns to `queued`.
  This is how the ladder makes an item wait on the `capability` item it files
  (section 8). An edge onto an upstream that is already closed resolves at
  once by 4.1; a filing that would create an item already cancelled or held is
  rejected (`dead_filing`) rather than filing dead work.
- **Re-planning supersedes; it never edits.** A filing replaces an item by
  adding a `supersedes` edge from the replacement to it. The target must
  already exist, sit under the filer's root (else `out_of_scope`), and not be
  an ancestor of the replacement:

| Target | Effect |
|---|---|
| `queued`, `ready`, `blocked` | becomes `cancelled(superseded)` naming its replacement; its waiting dependents re-point to the replacement |
| `leased`, `running`, `verifying` | rejected with `supersedes_active`: an active item must fail or finish first |
| closed | rejected with `supersedes_closed`: closed is final. To retry a failed step, file the retry as a new item and supersede the held items directly behind it with replacements that wait on the retry; the rest of the chain re-points and clears (4.5) |

- **Re-pointing** moves every ordering edge and `inputs_from` entry that named
  the target onto the replacement, each move an event on the dependent. The
  target's own upstream edges are dropped; the replacement declares its own.
  `supersedes` and `discovered_from` edges naming the target keep naming it,
  since they are history.
- **Superseding a parent supersedes its subtree.** Each waiting descendant is
  either superseded by an item in the same filing or ends `cancelled(cascade)`;
  an active descendant rejects the filing with `supersedes_active`.
- This is the project model's rule applied to work: a later correction
  supersedes rather than erases history (delivery plan, Phase 1;
  `EdgeRelation::Supersedes` in `rustykrab-projects`).

### 4.5 Cascade

- **Cascade is typed transitions, written by code.** When an item closes, the
  controller applies 4.1 to its dependents and 4.2 to its parent and
  descendants in the same store transaction as the closing transition, so a
  crash never leaves half a cascade (section 6.7).
- **Every cascaded item gets its own event:** `actor: controller`, the new
  status and reason, and `origin`, the root-cause item. `work show` prints the
  origin beside a cascade status (section 14.2).
- **Cancellation cascades through closed items.** A `cancelled(cascade)` item
  is closed, so its own dependents and descendants take 4.1 and 4.2 in turn.
- **Holds cascade through open ones.** `blocked(upstream_failed)` and
  `blocked(upstream_expired)` spread to every item behind the held one through
  `blocks` and `waits_for` edges, all naming the same origin, so `work list`
  shows the whole stalled chain and one failure reads as one cause.
- **The ladder runs first.** An item's own ladder (section 8) runs before it
  fails; nothing downstream moves while it climbs.
- **Holds clear only by change.** Holds are re-derived whenever an item's
  upstream edges change: it stays held only while a `blocks` upstream is
  failed or expired, or a `blocks` or `waits_for` upstream is itself held. So
  a re-plan that re-points the path clears the whole chain behind it.
  Otherwise a held item leaves the hold by being superseded or cancelled, or
  when it or an ancestor expires.
- **One message per parent**, naming the origin and what it holds, not one
  per dependent (section 6.6).
- **Nothing is undone.** Closed stays closed, work a closed item did stays
  done, and a cancelled item comes back only as a new filing.

### 4.6 Aging

Closed items do not stay live forever. Like Beads' compaction of closed
issues ("memory decay"), old closed items shrink to one line.

- **What ages.** A closed item older than a policy window (per kind; for
  example 30 days after it closed) is compacted by code, at idle, into one
  `work_item_archive` row: id, kind, title, parent, closed status and reason,
  worker, cost, closing time, its edges as ids, and a one-line summary built
  from those typed fields. No model is called.
- **What holds it back.** An item ages only when no open item names it through
  an ordering edge, `inputs_from` or `parent`, so a subtree ages together and
  readiness never reads an archived row. A `discovered_from` or `supersedes`
  edge from a live item to an archived one resolves to the archive line.
- **What stays live.** Readiness, the cycle check, live queries, `work ready`
  and `work list` exclude archived rows; `work archive` reads them and `work
  show --graph` prints them as their line (section 14.2). Events and evidence
  are never compacted and stay queryable by id, so section 1.1's metrics and
  every proposal's cited evidence survive.
- **What is lost.** Constraints, decisions, the other brief fields and the
  item's JSON detail go; what happened survives as events and evidence.
  Compaction is lossy by design and not reversible.

## 5. Named workers and the registry

```
Worker {
  name,                       // stable, human-addressable: "pinch", "krabby"
  kind: local | peer | claude_code | codex,
  capabilities: { models: [..], tools: [..], mcp_servers: [..],
                  repos: [..], machine, writable_resources: [..] },
  concurrency, health, cost_tier, last_seen,
  routing_record: { <work class>: { verified_done, claimed_not_verified,
                                    escaped_defects, cost, probation } }
                              // written by evaluation (section 10), read by Match
}
```

Kinds:

- **local**: a `SubagentRunner` conversation on this daemon whose tool block
  is fixed at its first model call, with later tools appended (section 12). Serialised on the single KV
  slot unless measurement in Phase 0 shows the RAM prompt cache makes
  interleaving cheap.
- **peer**: a paired node reached through `delegated_tasks`. The submission
  gains `required_tools` (pre-activated on the node, inside its ceiling) and a
  structured result; the node advertises its capabilities on pairing.
- **claude_code**: Claude Code in headless mode inside an isolated worktree,
  with a tool allowlist, a turn cap, JSON output and the user's own
  subscription. The adapter turns the delivery plan's executor brief into the
  prompt and parses the final JSON.
- **codex**: the same adapter shape over `codex exec`.

No kind owns `code` items. Which worker takes a class of coding work is read
from the routing record, which the evaluation writes from verified results;
`worker_kind` on an item is a constraint the user or policy sets, not a
default the controller assumes from the item's kind.

One result contract for every kind, extending the delivery plan's executor
output:

```
{ "summary", "artifacts": [..], "changed_paths": [..], "commit": "<sha>",
  "checks_run": [..], "known_limits": [..],
  "blocked": { "reason", "detail", "needs": [..] } | null,
  "error": { "class", "subclass", "fingerprint", "detail" } | null,
  "questions": [ { "text", "class", "options" } ],
  "discovered": [ { WorkItemDraft, "edges": [..], "supersedes": WorkItemId | null } ] }
```

Drafts are not filed as they stand. A single independent draft becomes an item
with a `discovered_from` edge; drafts with edges, or one naming a `supersedes`
target under the same root, go through `work_plan` validation as one graph
(section 6.5). Workers never hold `work_plan` themselves.

The controller verifies claims it can verify: the commit exists on the expected
parent, the changed paths match the diff, the named checks ran. A result that
claims more than the evidence shows is recorded as `verification_failed`, not
`done`. Names are assigned by the registry, shown in every channel message and
event, and usable by the user ("give that one to pinch").

## 6. The controller loop

A deterministic loop in `rustykrab-runtime` (or a new `rustykrab-control`
crate), never a model. It schedules a graph, not a list: items joined by typed
edges on `work_item_deps` and by `parent` links, with readiness, cascade and
roll-up computed in code. The graph exists for ordering, waiting on the world,
and independent fan-out, not for parallelising a sequence: dependent steps one
worker can carry stay inside that worker's item (Kim et al.), so a typical
graph is shallow and wide. Planning is not a second loop; a multi-step request
gets one planning item, which this loop runs like any other (6.1).

1. **Select**: take ready items (6.2) by priority, respecting the single-writer
   rule: at most one running item per writable resource (a calendar, a
   mailbox, a repository worktree, a device), checked against every leased or
   running item in the store, whichever subtree it belongs to. Within a
   subtree, validation has already ordered every pair of writers (6.1).
   Nothing reserves a resource across the gap between two items of a chain; a
   sequence that must not be interleaved on one resource is one item. A child
   whose parent has spent its budget is not selected; the parent parks as
   `blocked(budget_exhausted)` and surfaces (6.6).
2. **Match**: choose a healthy worker whose capabilities cover the item's
   `required_tools`, `worker_kind` and resources; prefer the cheapest tier
   whose routing record qualifies it for the item's class of work; a `code`
   item is matched the same way as any other, so a local worker takes it when
   its record says it can; escalate the tier on failed verification, not on
   the item's kind. A planning item matches only a `planner` worker.
3. **Lease** with a TTL; a lease that expires without a heartbeat returns the
   item to `ready` with a repair note. Readiness is re-read inside the lease
   transaction, so a `conditional_on_failure` item is never leased while its
   upstream is live. Leases belong to leaves: a parent holds no lease and no
   resources, and its status is a roll-up (6.2).
4. **Run**: build the brief (objective, done_when, constraints, decisions,
   artifact refs, prior failed evidence, and an `inputs` block from
   `inputs_from`, 6.3), activate tools, start the worker, stream progress
   events.
5. **Reconcile**: verify the result, attach evidence, route `questions`, apply
   the typed transition, and in the same transaction apply its consequences:
   downstream readiness and cascade, then the parent roll-up (6.2), each as an
   event. A single independent `discovered` draft is filed with
   `discovered_from`; drafts that depend on each other, add an edge into an
   open item, or supersede one are validated together as one graph (6.5),
   never filed one at a time.
6. **Climb the ladder** (section 8): classify the error (section 9), then
   retry, repair, switch worker, acquire or build the capability, file the
   improvement, and only then surface. Every rung is an event on the item.
   Inside a graph the item's own ladder runs first, nothing downstream moves
   while it climbs, and a plan B and a re-plan come before surfacing (6.4).
7. **Stall detection**: a progress ledger per item (Magentic-One's pattern);
   no new evidence across N iterations triggers repair rather than more turns.
   The same holds per subtree: a parent with nothing leased, running or ready
   beneath it, and no pending trigger whose source is still open (a future
   time, an open question, an open capability request), is stalled and climbs
   its own ladder (6.4). This is Magentic-One's outer loop, which re-plans on
   a stall, held to the same validation as the first plan.
8. **Notify**: the controller, not the model, appends "Not done: …, queued as
   #N" to the originating reply, and delivers failures, expiries and questions
   through the same channel as successes. People stop tracking what they
   believe a reminder owns, so the controller must own it end to end. A chain
   reports as one message per parent (6.6).
9. **Resume on restart**: leased items are re-checked against evidence and
   either resumed from their last checkpoint or returned to `ready`; nothing is
   failed merely because the daemon restarted, and no graph is re-planned
   because of a restart (6.7).

### 6.1 Building the graph

**When.** A request is filed as one item and runs as one item unless the
draft asks for a plan (`plan: true` on `work_file`: more than one deliverable,
a wait on the world between steps, or independent parts). A planned request
becomes a parent with one planning child; the graph attaches beneath the
parent.

**Who.** `planner` is a local worker definition (the built-in, moved to a file
per section 12) with a tiny toolset and no write access to the world:

```
planner
  tools:     work_plan, work_status, recall_search, memory_search
  writable:  none          model: the resident local model, thinking on
  brief:     the request's typed fields; delegated resources and judgment
             scopes; the caps; open item ids it may reference; a pointer to
             the origin conversation's recall archive
  output:    one accepted work_plan call: items, typed edges, parent links
```

Its definition says, and validation enforces where code can check:

- A step becomes its own item for one of five reasons only: a wait on the world
  (a trigger other than `now`), independent fan-out, a different worker kind
  or writable resource, an approval point, or a plan B. Every other step stays
  inside one item's objective.
- Every item carries an objective, `done_when`, typed constraints,
  `required_tools` and a budget, and optionally a trigger and `expires_at`.
  Parent links group phases; a parent's edges gate its whole subtree.
- A plan B is a `conditional_on_failure` item. Downstream steps name the step,
  never its plan B; the controller re-points them if plan B runs (6.4).

**Why one call.** A graph filed whole is checked whole before anything acts,
so a cycle, an over-budget branch or an impossible step is caught before step
one touches the user's calendar. A worker that files the next item as it goes
has already acted when step three proves impossible, and the rest of its plan
lives as intentions in a small model's context: the PM-Bench failure (4 to 7%
F1 in free text, 66 to 70% with a typed store).

**Validation.** `work_plan` validates inside the call and returns the accepted
ids or every failed check with a typed reason:

| Check | Rejected with |
|---|---|
| every referenced id exists and is visible to the principal | `unknown_ref` |
| no cycle over `blocks`, `waits_for` and `conditional_on_failure` edges and parent links; no such edge between an item and its own ancestor or descendant | `cycle` |
| depth and item count within policy caps (for example 3 levels, 12 items) | `depth_exceeded`, `too_many_items` |
| children's budgets sum to no more than the parent's remaining budget | `over_budget` |
| two items in the subtree that write the same resource are ordered by a path of `blocks` or `waits_for` edges | `single_writer_conflict` |
| no edge split only for sequence: sole downstream of its upstream, sole upstream of its downstream, same worker kind and writable resources, no approval point, downstream trigger `now` | `sequential_split`: a warning event on the plan, not a rejection, until Phase 1 has measured its false-positive rate |
| a `conditional_on_failure` item has no other upstream edge | `plan_b_edges` |
| an ordering edge onto an existing item only while that item is waiting; every `inputs_from` entry an item the downstream is ordered after; no filing of an item already cancelled or held (4.3, 4.4) | `edge_onto_active`, `input_unordered`, `dead_filing` |
| `supersedes` targets are queued, ready or blocked, inside the submitter's subtree (4.4) | `supersedes_active`, `supersedes_closed`, `out_of_scope` |
| one accepted graph per planning run | `already_planned` |

Section 14.1 lists every reason. Rejection is atomic: only the rejection event
is stored. The reasons return in the tool result, so the planner fixes the graph in the same run (an append,
not a re-prefill, section 12). A run that ends without an accepted graph
climbs the planning item's ladder (repair with the typed reasons, then a worker
switch); when that is spent, the request runs as one item on one worker, as it
would have without a planner. No item of a graph is ready before its planning
item is reconciled.

**Approval.** After acceptance and before any lease, policy decides whether the
graph needs the user:

| Trigger | Holds |
|---|---|
| more items than the policy threshold | the whole graph |
| total budget above the policy threshold | the whole graph |
| a writable resource the user has not delegated | that item and what depends on it |
| a `code` item outside an authorized delivery slice | that item and what depends on it |

If any trigger fires, one `needs_consent` question covers the whole graph at
acceptance: one line per item, the triggers, the budget. Held items wait in
`blocked(needs_consent)` (the parent, when the whole graph is held) and the
rest start at once, so the user answers once, early, and the chain never stops
later to ask. The answer approves, declines (held items `cancelled(requested)`,
cascading per 4.5) or amends (a re-plan, 6.5). When standing judgment covers every
trigger, nothing is asked and the delegated decision is recorded (section 7).

**Code work never goes through the planner.** A `code` item in a planner's
graph is a request to the delivery path, held by the rule above. Once the slice
is authorized, the delivery compiler's graph is imported beneath it: one parent
per slice, one child parent per stack layer with `blocks` edges between layers
in `StackManifest` order, and each layer's work DAG as `code` items with
`blocks` edges from the compiler's dependencies. The compiler has already
rejected cycles and out-of-scope work (delivery plan, 5.1); the import repeats
the structural checks (ids, cycles, parent links, caps) only as a bridge
consistency check, and a failure is an `internal` defect, not a re-plan. The
controller never re-orders, splits or merges an imported graph; a layer's
`done_when` is its acceptance, and the delivery verifier decides `done`
(section 4). Unordered writers in one layer's worktree are allowed, since the
compiler partitions scope by files and Select serialises them. A revised slice
is a new import that supersedes the open items it replaces (6.5).

```
"Compare three phone plans and switch me to the cheapest by Friday"
P  parent                          expires_at: Friday
  a  research plan X               fan-out: no edges between a, b and c
  b  research plan Y
  c  research plan Z
  d  pick the cheapest             a, b, c blocks d    inputs_from: a, b, c
  e  switch the carrier            d blocks e          writes: carrier account
  f  draft the switch for the user e conditional_on_failure f
approval: e writes a resource not delegated; one question at acceptance
          holds e and f; a to d start at once
```

### 6.2 Readiness, cascade and roll-up

The rules live in section 4: readiness in 4.1 and 4.2, the parent roll-up
table in 4.2, cascade in 4.5. This loop applies them at fixed points, always
inside the transaction that caused them:

- **Readiness** is recomputed for an item's dependents and its ancestors'
  subtrees whenever a status, edge, trigger, precondition, approval or budget
  changes, and re-read inside the lease transaction (step 3). A parent is
  never ready and never leased.
- **Cascade** runs in the reconcile step (5), on expiry, on cancel and on
  resume (6.7). Each transition it applies to a dependent is an event naming
  the origin item and the cause; holds spread through `blocks` and `waits_for`
  edges and all name the same origin (4.5). Held items are not terminal, so
  nothing cascades past them until a re-plan re-points the chain, the parent
  is cancelled or expires, or the item's own `expires_at` passes.
- **Roll-up** is recomputed for every ancestor on every child transition, in
  the same transaction, from the table in 4.2. A parent whose children are all
  closed enters `verifying`; its verifier checks `done_when` from the
  children's evidence, then `done`, or the parent's ladder (6.4).
- **Cancel and expiry of a parent** revoke a leased child's lease and stop its
  worker (a local run at its next step, an external agent's process ended),
  keep its evidence, and apply `cancelled(cascade)` to every open descendant
  (4.2).
- **Capability items** the ladder files have a `blocks` edge into the item
  that needed them and no parent in the chain, so cancelling a chain does not
  cancel a tool build other work may reuse; an unleased capability item with
  no open dependents is cancelled (section 8).
- The controller never re-opens a closed item; a retry is a new item, so the
  failed attempt and its ladder stay in the record (4.5).

### 6.3 Fan-in: what a downstream brief carries

At lease time the controller copies, from each item in `inputs_from` (by
default the `blocks` upstreams), its verified evidence and artifact refs into
an `inputs` block of the brief:

```
inputs:
  - item: #41 "research plan X"  status: done  worker: pinch
    evidence: [ref, ref]  artifacts: [ref]  summary: one line
  - item: #43 "book table at X"  edge: waits_for  status: failed
    error: capability_gap/credential
  more: #44, #45  (by id through work_status or the item's recall archive)
```

- **The rules are section 4.3**: what is copied, only verified evidence,
  `waits_for` inputs by request only, re-pointing with the edges, and the
  copied set recorded on the lease.
- **Pointers, not prose** (section 12): refs, the closed status and at most
  one line of the upstream's summary, never its transcript. Cognition's
  handoff-loss argument and the constraint-survival figure in section 4 are
  why inputs point into the upstream's full record rather than carry a note
  written for the handoff.
- **Bounded for small models**: a cap per input and a cap on the block
  (policy, for example 8 inputs or about 600 tokens, placeholders Phase 0
  measures). The rest are listed by id and reached through `work_status` and
  the upstream's recall archive, which is durable and keyed by conversation
  id.

### 6.4 A failure inside a chain

The ladder in section 8 runs per item and is unchanged for an item with no
parent. Inside a graph it gains two moves between order 3 and order 4, and
surfacing moves to the parent:

1. **The item's own ladder, orders 0 to 3.** While the item retries, repairs,
   switches worker or parks on a capability item, its downstream items stay
   `queued` and nothing cascades.
2. **Plan B.** If the item has a `conditional_on_failure` downstream, reaching
   order 4 fails the item instead of surfacing: it becomes `failed` with its
   pending question recorded but not sent, and plan B is released. A rung only
   the user can resolve (a credential only the user holds, a consent, a
   decision outside delegated judgment) counts as order 4 here, so plan B runs
   before the user is asked. Plan B stands in for the failed item: its
   outgoing `blocks` and `waits_for` edges are re-pointed to plan B, each as an
   event. If plan B succeeds, the recorded question closes as obsolete
   (section 7). A plan B may have its own plan B, within the depth cap.
3. **Re-plan.** An item whose orders 0 to 3 are spent, with no plan B left and
   no need only the user can meet, becomes `failed` and cascades (4.5). If
   that holds any item, the parent climbs its own ladder, whose order 1 is a
   re-plan: a planning item under the same parent, briefed with the failed
   item's ladder and error class as pointers, which may supersede held and
   queued items (6.5). One re-plan per parent by default. The same rung runs
   when the subtree stalls (step 7) or the parent's `done_when` fails.
4. **Surface once, at the parent**, naming the failed child, the order it
   reached, what plan B and the re-plan did, and what is held behind it (6.6).

Exceptions, decided in code:

- A `policy` stop (scope, single-writer, ceiling) surfaces at once, as section 8
  says; neither plan B nor a re-plan may route around it (the over-persistence
  risk in section 17).
- A need only the user can meet, with no plan B, is not re-planned around: the
  item parks in its typed blocked state (section 7), its downstream items stay
  `queued`, and the parent surfaces the question. Nothing cascades from a
  question not yet answered.
- Expiry is not a failure and has no ladder: the item becomes `expired`, a live
  lease is revoked, and the cascade applies.

### 6.5 Re-planning and `supersedes`

Only four writers change a graph: the planner through `work_plan`, a worker
through drafts in its result, the delivery import, and the controller's own
ladder, cascade and re-pointing. Workers do not hold `work_plan`: the
controller submits a worker's drafts as one graph through the same validation,
and on rejection records the reason and files a planning item with the drafts
and the reason as its brief. A user's amendment is briefed to a planning item
the same way.

- **Targets.** `supersedes` applies only to queued, ready or blocked items
  inside the submitter's subtree (a worker's own parent's, or a planning
  item's parent's).
- **Live targets finish first.** A leased, running or verifying target is never
  interrupted: a filing that supersedes one is rejected whole with
  `supersedes_active` (4.4) and the rejection is recorded on the filing item.
  A worker's drafts that named a live sibling are re-submitted by the parent's
  re-plan (6.4) once that sibling has closed.
- **Closed targets are rejected** (`supersedes_closed`). A retry of a
  failed step is a new item, and the re-plan supersedes the held downstream
  items with replacements that wait on it.
- **Application is one transaction.** Each old item becomes
  `cancelled(superseded)` naming its replacement; its outgoing edges move to
  the replacement and its incoming edges are dropped (the replacement declares
  its own). Superseding a parent supersedes its open subtree and is rejected
  with `supersedes_active` while any descendant is live (4.4). Validation runs on the graph as it will be after
  re-pointing, with the budget checked against the parent's remaining budget.
- **Approval applies again**: a trigger the new graph fires holds only what it
  touches.

### 6.6 Reporting a chain

One message per parent, never one per child; a single item is its own parent,
so step 8 is unchanged for it.

- **When:** on acceptance (one line appended to the originating reply, with
  the approval question if a trigger fired); when a question surfaces; when the
  parent ends `done`, `failed`, `expired` or `cancelled`; and as a digest while
  a chain stays open past a policy window. Child transitions within a short
  window are coalesced; a blocking-now question goes at once, still as the
  parent's message.
- **What:** the roll-up (work items done of total, planning items not
  counted); what is running and on which worker; what is queued and why (a
  trigger, or waiting on #N); what is held and behind which upstream; and for
  each failure the child, the order it reached, its error class, and what plan
  B or the re-plan did.
- **Questions from one subtree go in one message**, each with its ladder, so
  the user answers once (section 8).

```
Phone plan switch (#P): 4 of 6 done. Cheapest is plan Y (#d).
Not done: "switch the carrier" (#e) stopped at order 2a, needs_credential:
carrier login. Plan B "draft the switch for you" (#f) is running on pinch.
Queued: nothing else. Expires Friday.
```

### 6.7 Resume on restart

Every piece of a graph is in the store, so resume reads it back:

- A `work_plan` call is one transaction, so a planning run cut off by a restart
  left an accepted graph or nothing; its planning item is resumed or re-run
  like any leased item.
- Cascade and roll-up are written with the transition that caused them, so
  none is half applied. Before leasing anything the controller still
  re-derives readiness, cascade state and roll-ups from edges and statuses; a
  mismatch is corrected with a `resume` event and files an `internal` item,
  since it is a controller defect (section 9).
- Expiries that passed while the daemon was down apply first, with their
  cascade, so an expired chain does not start one more step; triggers that
  fired while down apply next; overdue items then run one at a time (section
  12.1).
- Leased leaves are re-checked against evidence as in step 9; parents hold no
  leases.
- Notifications are written with their transition and sent from an outbox,
  coalesced per parent, so a restart neither drops a parent's message nor
  sends it twice.

## 7. Questions, blocked states, and standing judgment

Adopt the planning plan's question classes unchanged: blocking now, blocking
later, researchable, defaultable, delegated, obsolete. Only "blocking now"
outside delegated authority reaches the user; the rest are recorded, defaulted
with a recorded default, researched by a `research` item, or closed.

Standing judgment is durable policy in the store, granted in ordinary language
and compiled to a checklist the controller evaluates. Every delegated decision
records alternatives, rationale, why it fell inside authority and how to
revisit it.

Blocked states generalise `credential_request`:

- `needs_credential` routes to the existing credential page and resumes on
  fulfilment;
- `needs_decision` delivers the question and options through the channel and
  resumes on the answer, with the item parked, not the conversation;
- `needs_consent` is a decision with a fixed yes/no and an audit record;
- `needs_tool` is accepted only when the host confirms the tool really is
  unavailable (an MCP server not configured, a binary missing); when the tool
  is merely unloaded the request is rejected with "load it";
- `worker_unavailable` waits for the registry, with a TTL;
- `upstream_failed` and `upstream_expired` are set only by cascade (section
  4.5), never filed by a model, and name their origin item. They are not
  questions: the user hears once about the origin, in its parent's message
  (section 6.6), and they clear when a re-plan re-points the chain.

A model may not file `needs_decision` for a defaultable question; the router
classifies, the model only asks.

## 8. Persistence: orders of resolution before surfacing

The user's requirement is persistence: the system surfaces only when it really
does not know how to resolve its issue. That is implemented as a ladder the
controller climbs for every failure, with a budget per rung, and with each
rung a typed transition that leaves evidence. The model never decides to skip a
rung; the controller does, from the error class (section 9).

| Order | Rung | What happens | Typical budget |
|---|---|---|---|
| 0 | Retry | Same worker, same brief, transient errors only (network, timeout, rate limit) | 2 attempts |
| 1 | Repair | Re-run with the failure evidence, the error class and a diagnosis step; then a different worker kind or model | 2 repairs, 1 worker switch |
| 2a | Acquire a capability | `needs_tool` where the tool exists: load it. Credential, consent, install, compute: file a `capability` item with the matching trigger and park the work | one capability item per gap |
| 2b | Build a capability | No tool exists for the need: file a `capability` item of kind build (a tool, skill, MCP adapter, or worker adapter), execute it through the delivery pipeline, then resume the original item | one build per gap, bounded cost |
| 2c | Request more capability | The gap is capacity (context, model size, a coding agent, GPU on a peer): file a request routed by policy, e.g. escalate the worker kind, add a peer, or ask the user to enable a resource | one request per gap |
| 3 | Improve the system | The failure recurs with the same fingerprint, or could not be classified: file an `internal` item (section 9 and 10) and continue with the best available rung | rate-limited |
| 4 | Surface | Only when a decision is outside delegated judgment, only the user can supply the need, or the budgets above are spent | always allowed |

Rules:

- Second-order moves are first-class work. "Build the tool" and "request the
  capability" are `capability` items with their own lifecycle and
  verification, and the original item gains a `blocks` edge on them (section
  4.4), not a note in a summary. A capability item has no parent in the
  original's chain, so cancelling the chain does not cancel a tool other work
  may reuse. When the capability item is `done` the edge is satisfied and the
  original resumes with the capability activated up front. If it fails, the
  original is `blocked(upstream_failed)` and its ladder continues at the next
  rung.
- Budgets are per item and per order, recorded on the item, and adjustable by
  standing policy ("spend up to N on building tools for personal tasks").
- Every rung climbed is an event with the error class, what was tried and what
  changed. That trace is the evidence dreaming uses to judge whether an
  escalation was avoidable (section 10).
- Persistence is bounded by policy, not by stamina: a rung that would violate
  the single-writer rule, a capability ceiling, a cost ceiling or a
  no-touch scope stops the ladder and surfaces with the reason.
- Surfacing carries the ladder: the message to the user says what was tried at
  each order and what is being asked, so the user answers once.
- Inside a graph the ladder still runs per item, but two moves come between
  order 3 and order 4: the item's `conditional_on_failure` plan B, then one
  re-plan by the parent. Surfacing then happens once, at the parent
  (section 6.4). A policy stop skips both.

## 9. Errors and self-observability

The ladder can only be climbed if failures are well defined. Every failure the
controller sees must map to a typed error, and a failure that cannot be
classified is itself treated as a defect in the system's observability.

```
Error {
  class:     tool | model | capability_gap | environment | verification
           | policy | budget | unknown,
  subclass:  tool: invalid_args | not_found | timeout | upstream_error
             model: format | refusal | hallucinated_tool | loop | empty
             capability_gap: tool | credential | consent | compute | knowledge
             environment: network | disk | permission | process | dependency
             verification: claim_mismatch | check_failed | incomplete
             policy: scope | single_writer | ceiling
             budget: iterations | tokens | wall | repairs,
  fingerprint,          // stable hash of class, subclass, tool, worker kind and
                        // the normalised message, for recurrence counting
  detail, artifact_refs, // where the evidence is
  observed_by            // which probe or check classified it
}
```

Rules:

- **Classification is code first, model second.** Tool results, provider
  responses, exit codes, verifier output and policy checks classify most
  failures deterministically. A local worker runs a bounded diagnosis step only
  for what is left, and its verdict is recorded as `observed_by: diagnosis`
  with lower confidence.
- **`unknown` is a defect.** Each `unknown` error files an `internal` item:
  "add the probe, log line, check or parser that would have classified this",
  with the raw evidence attached. The unknown-error rate is an expectation in
  section 1.1, target zero.
- **Structured events everywhere.** Every worker step, tool call, transition
  and rung emits a typed event with the item, worker, error (if any) and
  evidence refs. The recall archive keeps the bulk; events keep pointers.
- **Fingerprints drive recurrence.** The same fingerprint seen N times across
  items promotes from rung 1 to rung 3: the system stops repairing the symptom
  and files the improvement.
- **The system observes itself the same way.** Controller faults, dreaming
  faults and adapter faults are errors of class `environment` or `unknown` on
  an `internal` item, so the ladder and the metrics apply to RustyKrab's own
  machinery, not only to user work.

## 10. Evaluation: how it finds where to improve

Dreaming already monitors outcomes, analyses them off-cycle, consolidates memory
reversibly and knows which signals are ground truth. This plan gives it the
expectations in section 1.1 as its objective and the following criteria as the
places it looks. Each criterion names its evidence and the kind of item it
files.

| Criterion | Evidence | Files |
|---|---|---|
| Expectation regressions | a metric in section 1.1 moving the wrong way, by kind or worker | `proposal` |
| Avoidable escalations | a surfaced item whose answer a lower rung could have produced (the user chose a recorded default, or supplied something a probe could have found) | `proposal` or `internal` |
| Recurring fingerprints | the same error fingerprint across items or workers | `internal` |
| Unknown errors | any `unknown` classification | `internal` (observability) |
| Capability gaps | `needs_tool` blocks, capability requests, and builds that were later reused | `capability` (pre-build the tool) or `proposal` (add it to a definition's default set) |
| Wasted rungs and plan shape | repairs that never changed the outcome; retries on non-transient errors; `work_plan` rejections and `sequential_split` warnings by reason; items per completed request; supersedes per root | `internal` (ladder policy) or `proposal` (planner definition) |
| Verification misses | escaped defects; claim-versus-verified mismatches per worker | `proposal` (verifier or brief) |
| Cost and latency | tokens, wall time and prefill per completed item by worker kind, against the cheapest kind that succeeded | `proposal` (routing) |
| Coding quality by worker | on `code` items, per worker and class of work: verified done against claimed done, escaped defects, review rejections, repairs before acceptance, and cost; compared across the kinds that took the same class | routing record update; `proposal` (routing) when the default tier for a class should move either way |
| Skill outcomes | the existing per-skill outcome records and their signal class | `proposal` (skill delta) |

Gates and shape:

- **Source**: a proposal is filed only from verifiable or explicit signals,
  never from implicit or judge-only evidence. That is the gate
  `SignalClass::is_ground_truth()` already encodes. `internal` items for
  unknown errors are the one exception, because the evidence is the raw
  failure itself.
- **Shape**: observed failure or opportunity, the expectation it serves, the
  affected skill, prompt, tool, adapter, policy or code path, evidence and
  counterexamples, expected movement of the named metric, risk and rollback
  condition, and the evaluation that could falsify it. This is section 9.3 of
  the verification-skills plan, applied to everything.
- **Review surface**: every proposal is projected to a GitHub issue with a
  `rustykrab-proposal` label and the evidence attached. A human accepts,
  declines or amends on the issue; the decision syncs back as an event on the
  item. `internal` observability items below a cost threshold may execute
  without review under standing policy, since they add measurement rather than
  change behaviour.
- **Execution**: an accepted proposal becomes a `code` item (or a skill or
  prompt change routed through the existing stage-then-promote path), executed
  by whichever worker the routing record qualifies for that class of change,
  under the delivery plan's verification, and promoted through the verification-skills plan's tiers with probation and
  rollback. Promotion requires the named metric to move on replay or in the
  probation window; a proposal that did not move its metric is recorded as
  such, which is itself evidence for the next cycle.
- **Rate and scope limits**: proposals per day, one open proposal per subject,
  no proposal may touch policy, credentials, the controller, the ladder budgets
  or its own measurement without the highest review tier.
- **Routing is a learned policy, decided at evaluation.** The system does not
  decide up front that coding belongs to frontier agents. Every `code` result
  is verified from evidence regardless of who produced it; the verdict, the
  repairs it took and the cost are written to the producing worker's routing
  record for that class of work. A class starts on the cheapest worker under
  probation with a small slice budget; the controller escalates a single item
  when verification fails (section 8), and dreaming moves the class's default
  tier only from the accumulated record, in either direction: a local worker
  whose verified rate on a class holds earns larger slices of it, and a
  frontier worker whose cost is not buying quality loses the default. A
  routing move is a `proposal` like any other, with the metric it expects to
  move and a rollback condition.

## 11. Where work is tracked: the store as truth, issues as review surface

Every kind of work, personal or software, is tracked in one place: the
`work_items` table and its companions in the local store. Everything else is
a view of it, a projection of it, an output of it, or state the controller
does not read.

| Place | What it holds | Direction |
|---|---|---|
| `work_items`, `work_item_deps`, `work_item_events`, `work_item_evidence` | every item of every kind: fields, typed edges, parent, lease, ladder, evidence, events | source of truth; only the controller transitions it |
| `work_item_archive` | one-line summaries of closed items older than the policy window | local only; written by aging, read by `work archive` |
| CLI, REST and channel commands (`rustykrab work …`, Telegram) | the user's view: ready, list, show, graph, plan previews, questions, reports | read-only view; a user command (cancel, approve, answer) enters as a typed event the controller applies |
| GitHub Issues (or Linear) | `code`, `proposal`, `internal` and `capability` build items | one-way projection out; labels and approval comments sync back as typed events; nothing else flows back |
| Pull requests | the output of `code` items, including builds and accepted proposals | produced by the delivery plan's stacks; the item links the PR, the PR body carries the item's evidence |
| `scheduled_jobs`, `delegated_tasks`, `projects` | cron firings, peer submissions, planning slices | each row gains a `work_item_id`, so it is an item the controller schedules, not a parallel queue |
| The `todo_write` list | a worker's steps within one conversation | in-conversation scratchpad, not tracking; a step that must outlive the turn is filed with `work_file` |
| A worker's own state (Claude Code tasks, a Beads database in a worktree, a peer's local queue) | whatever the worker uses to organise itself | invisible to the controller; only the typed result (section 5) counts |

Projection rule: only what a human reviews as engineering is projected.

| Kind | Projected to issues | Reaches the user through |
|---|---|---|
| `code` | yes, with its PR | issue, PR, a channel summary |
| `proposal` | yes, labelled `rustykrab-proposal` (section 10) | issue; a channel notice when filed |
| `internal` | yes, labelled `rustykrab-internal`, including items that run without review under policy | issue |
| `capability`, build | yes: a tool, skill, MCP adapter or worker adapter being built | issue, then PR |
| `capability`, acquire or request | no: loading a tool, a credential, consent, an install, compute | the channel, when it needs the user |
| `personal` | never | Telegram and the CLI |
| `research` | never | Telegram and the CLI |

Rules:

- **Projection never carries a local-only item.** A projected item whose
  parent, edge or `inputs_from` is a `personal` or `research` item shows it as
  an opaque reference (`local:#N`), never its title, objective or evidence.
- **Projected fields.** Title, objective, `done_when`, status (roll-up status
  for a parent), worker, evidence links, the parent as a sub-issue and edges as
  references where the surface supports them. The issue never decides
  readiness.
- **One way.** Decisions taken on the issue (labels, approval comments) sync
  back as typed events. Hand edits to projected fields are overwritten on the
  next projection; the decision vocabulary is the only way to act from the
  issue.
- **Issues are not the queue**: no leases, no dependency semantics beyond
  references, rate limits, and a network dependency for a loop that must run
  offline. Beads-style semantics (ready queue, typed edges, `discovered_from`,
  aging) stay local.
- **The user is never forced through GitHub.** `rustykrab work ready | list |
  show | plan` and the channel commands (section 14) cover every kind, so a
  personal task is filed, followed and closed without an issue existing.

Open decision for review: GitHub Issues versus Linear as the review surface.
The projection rule and fields are the same; only the adapter differs.

## 12. Small-model constraints baked in

From the September 2026 measurements on this machine:

- **A fixed front block, known tools first.** `AgentDefinition` gains `tools`
  and `mcp_servers` (visible from turn 0) distinct from `allowed_tools` (the
  ceiling). The runner activates them, with the item's `required_tools`,
  before the first model call, and the tools array does not change for the
  rest of the run. This is a cache rule, not a capability rule: it keeps the
  prefix stable and saves a search round trip for needs the host already
  knows. Keep the starting set small on slow-prefill models; qwen3.8 pays
  about five seconds per thousand tokens of schemas on every uncached prefill,
  so a tool that is only possibly needed is left to the append path.
- **File-based definitions.** `~/.rustykrab/agents/<name>.md` (or the data
  dir) with front-matter for tools, MCP servers, profile, model preference and
  writable resources, loaded like `SKILL.md`. The three built-ins move to
  files.
- **Late needs are appended, not re-rendered.** Late binding works on both
  default models: qwen3.8 called a tool first seen as JSON in a tool result 12
  of 12 times with no distractors, 12 of 12 among five or ten near-misses, and
  12 of 12 through a generic `call_tool` (late binding experiment,
  2026-09-24). `tools_load` returns the requested schemas as text in its
  result and records them as callable without changing the tools array;
  compaction folds appended tools into the front block because it re-prefills
  anyway. Search and load become one contract: gemma4 obeyed the `tools_load`
  description over the result's framing on a third of late-bound calls when
  both existed. Until the append path ships in Phase 2, today's `tools_load`
  changes the tools array and pays the full re-prefill; that is an
  implementation gap, not a limit of the model.
- **Not yet measured.** The search step was scripted in every trial, so
  whether a model decides to search on its own is open; a real catalog of
  about 10K tokens of schemas, and thinking off, were not tested. Phase 0
  measures the base rate of mid-run loads before either path is tuned.
- **The host validates catalogs.** Both models substituted a near-miss when the
  catalog lacked the target (a one-day forecast for current weather; a
  shipment list for a tracking number). A search result is labelled "found"
  only when the host's match is plausible; otherwise the model is told nothing
  matched.
- **Context budgets are per worker, and small.** Usable context for ≤12B
  models collapses well below their allocation on non-literal retrieval
  (NoLiMa), so worker briefs are short, references are pointers, and bulk
  material stays in the recall archive.
- **Thinking stays on** for local workers; every measurement here was taken
  with it on.
- **Routing by evidence, not by hope.** Local workers take triage, summaries,
  personal tasks, verification of simple checks, research, and the classes of
  coding work their routing record has earned; a `code` item starts on the
  cheapest worker the record qualifies, the verifier decides whether the
  result stands, and a failing local run escalates the worker kind rather than
  retrying the same model. The published small-model figures are the prior
  for the record, not a rule that coding skips local workers.

### 12.1 Lessons from the M4 cutover (2026-09-25)

The move of the primary daemon from the M1 to the M4 produced measurements
that bear directly on this plan. Sources: the migration session's export and
the reproduction notes in `research_notes/Late tool binding experiment/`.

- **One portable conversation shape, no per-model code paths.** Runner
  notices appended as `Role::System` after an assistant turn are folded to the
  top of the prompt by Ollama's Qwen 3.5-family renderer (the template rejects
  non-leading system messages), so the model never sees the nudge last, the
  cached prefix is invalidated, and with an earlier mid-history system message
  qwen3.8 returns nothing at all. The same nudge as a user turn works every
  time and touches only the prompt suffix. gemma4 renders mid-turn system
  messages in place. Rule for workers: one leading system message, notices as
  `[System notice]` user turns, model differences only as capability or
  profile data, and a test that no request carries a non-leading system
  message.
- **Completion nudges are net-negative.** On the M1 (gemma4, Aug 1 to Sep 25,
  about 304 tool-using turns) the `task_complete` reminder fired 289 times;
  224 of them (78%) followed a reply the runner's own classifier had already
  called Complete, after which 122 turns called more tools and only 71
  restated the answer. Honest "can't find it" answers were pushed into
  flailing. On qwen3.8 the reminder produced the empty response. #663 removes
  it; the controller in this plan should treat a Complete classification as
  the end of a worker's turn and verify the result, not nag for a signal.
- **Compaction on local models must be staged, not one-shot.** Field practice
  compacts once at a threshold (Claude Code about 13K below the limit, Codex
  at 90%, Gemini CLI at 50% keeping the newest 30% verbatim). Measured work
  (JetBrains' Complexity Trap, a 2026 CliffCompaction study) finds masking or
  truncating old tool outputs matches LLM summarisation at about half the
  cost, that summaries of summaries degrade, and that thinking does not help a
  summary. On this hardware the binding cost is the re-read after compaction
  rewrites early context: about 84 s for 16K tokens at 190 tok/s. The agreed
  direction: cap tool outputs on entry (full output to the recall archive),
  compact at idle time and pre-warm the cache, mask old tool outputs before
  summarising, summarise with thinking off and a user-role instruction, keep
  a verbatim tail, use hysteresis, and truncate without a model as the
  emergency path. The code's "0.85 per the RLM paper" is a reference-library
  default, not a paper result, and should be re-derived from measurement.
- **Memory, not tokens, is the scheduler's constraint on a 36 GB Mac.** With
  two overdue cron jobs and a user turn interleaved, qwen3.8 grew from 18.5 GB
  to 33 GB resident and swap reached 10 GB, because the MLX engine keeps paged
  prefix-cache snapshots per conversation and interleaved sessions evict each
  other's prefix. `OLLAMA_NUM_PARALLEL=1` is already the case and does not
  help. The dispatcher must serialise local work per model and never overlap
  scheduled jobs with an interactive turn; overdue jobs must run one at a
  time after a restart.
- **Credentials do not follow the store.** Only the master key crossed; Gmail
  and CalDAV credentials lived solely in the M1's Keychain and had to be
  re-entered. A worker on another machine therefore either receives
  provisioned credentials or must route credential-bearing calls to the node
  that holds them; the registry's `capabilities` should say which.
- **Lock state is a worker availability signal.** The 5.3.2 daemon cannot
  read its master key while the screen is locked past the grace period and
  restarts in a loop until unlocked (down 05:42Z to 14:58Z on the first
  night). Until the after-first-unlock keychain class ships, a laptop worker
  is unavailable whenever it is locked, and the registry's health check
  should know that.

## 13. Durable data model

New tables in `rustykrab-store`, with `Store::run_migrations`
(`CREATE TABLE IF NOT EXISTS` blocks, additive `ALTER TABLE` for existing
tables, as `projects` and `delegated_tasks` already do):

- `work_items` (section 4 fields; JSON columns for lists, including
  `inputs_from`; edges are rows in `work_item_deps`, not JSON). The columns the
  controller filters on are scalar: `kind`, `status`, `status_reason` (set for
  `blocked` and `cancelled` only), `status_origin` (the root-cause item of a
  cascade status), `priority`, `parent` (nullable; a subtree is archived in one
  transaction, section 4.6), `trigger_at` (the `at(time)` trigger, else null),
  `expires_at`, `closed_at` (set by the transition into a closed status), and
  two control columns no model sees: `plan_id` (the accepted `work_plan` that
  filed the item) and `held_by` (the approval question holding it, section
  6.1).
- `work_item_deps` (`item`, `depends_on`, `kind`), primary key over all three;
  `kind` is `blocks | waits_for | conditional_on_failure | supersedes |
  discovered_from`. `item` references `work_items(id)` and goes with it;
  `depends_on` is not a foreign key, because a `supersedes` or
  `discovered_from` edge may outlive its target's live row, while section 4.6
  never archives an item that an open item orders after. Re-pointing (section
  4.4) rewrites `depends_on` in place; the event log keeps the old value.
- `work_item_events` (append-only: item, at, kind: transition | cascade |
  re-point | rung | lease | resume, from, to, actor, reason, the direct
  `upstream` and the root-cause `origin` for cascades and re-points, evidence
  ref), `work_item_evidence` (item, kind, ref, hash, verified_by). Both key on
  the item id without a cascading foreign key, so they outlive compaction.
- `work_outbox` (id, parent item, origin item, channel, body, created_at,
  delivered_at): notices written in the transaction that caused them and
  delivered from here, coalesced per parent, so a restart neither drops nor
  repeats one (section 6.7).
- `work_item_archive` (id, kind, title, parent, closed status and reason,
  worker, cost, closed_at, archived_at, one-line summary, edges JSON): one row
  per compacted item, written in the transaction that deletes its `work_items`
  row.
- `work_plans` (id, root, filed_by item, rationale, approval question, the
  policy that required it, created_at): one row per accepted `work_plan` call,
  which `work plan <id>` previews (section 14.2). A rejected call writes no row, only a rejection
  event with its reason on the filing item.
- `workers` (name, kind, capabilities JSON, health, last_seen, cost_tier,
  routing_record JSON keyed by work class), `leases` (item, worker, since,
  ttl, heartbeat, inputs JSON: the `inputs_from` ids and evidence refs copied
  into the brief, section 4.3).
- `questions` (item, class, text, options, delivered_via, answer, answered_at)
  and `judgment_policies` (scope, text, compiled checks, granted_at).
- `proposals` is a `kind` of work item plus a `proposal_evidence` join to
  `outcome_records` and `dream_reports`, so a proposal cites the records that
  justified it.
- `delegated_tasks` gains `work_item_id`, `required_tools` and a
  `result_json` column, and stops failing tasks on restart once leases exist.
- `projects` links its slices' work items through `work_item_id`; each item
  imported from a delivery records the `delivery_work_items` or
  `delivery_layers` id it mirrors, so a re-import is idempotent.
- `scheduled_jobs` gains `work_item_id`: a firing is an item with
  `trigger: at(time)` that the dispatcher serialises with everything else
  (section 12.1), not a conversation started outside the controller.

Indexes, named for the query they serve, as the store already names them:

| Index | Serves |
|---|---|
| `idx_work_items_ready` on `work_items (status, priority)` | Select: ready items by priority |
| `idx_work_item_deps_upstream` on `work_item_deps (depends_on, kind)` | on every transition: the dependents to re-check, cascade or re-point |
| `idx_work_items_parent` on `work_items (parent, status)` | roll-up, cancel-all, the aging subtree check |
| `idx_work_items_trigger` on `work_items (status, trigger_at)` and `idx_work_items_expiry` on `work_items (status, expires_at)` | the timer sweep for `at(time)` triggers and expiry |
| `idx_work_items_closed` on `work_items (closed_at)` where `closed_at` is not null | the aging sweep |
| `idx_work_item_events_item` on `work_item_events (item, at)` | `work show`, origins, and the section 1.1 metrics, live or archived |
| `idx_work_item_evidence_item` on `work_item_evidence (item)` | fan-in at lease time, verification |
| `idx_work_item_archive_closed` on `work_item_archive (kind, closed_at)` | `work archive list` and long-window reads |
| `idx_work_outbox_pending` on `work_outbox (created_at)` where `delivered_at` is null | the notifier's pending queue |

Readiness is re-evaluated on transitions, not polled. When an item changes
status, or an edge is added or re-pointed, the controller reads the dependents
through `idx_work_item_deps_upstream` and the parent through `parent`, applies
sections 4.1 and 4.2 in the same transaction, and marks the dependents whose
conditions all hold `ready`; the timer sweep covers time triggers and expiry.
The primary key of `work_item_deps` answers the reverse question, what an item
waits on, for `work show` and the cycle check.

Unreadable values parse to the conservative case, as `delegated_tasks` already
does (`TaskStatus::parse`): an unknown status reads as `failed` and an unknown
edge kind as `blocks`, so a row the controller cannot interpret never becomes
work that runs early.

## 14. API, tools, and user surface

Model-facing tools, all with strict schemas and host validation:

- `work_file` files a discovered item (draft fields only, plus `plan: bool` to
  ask for a planning step, section 6.1; the host fills provenance and rejects
  a draft whose `required_tools` are merely unloaded);
- `work_plan` files a whole graph of items, edges and parent links in one
  atomic call (section 14.1);
- `work_status` reads the items an agent may see, with their parent, edges and
  roll-up status;
- `result_report` returns the typed result contract at the end of a worker run
  (replacing free-text completion for workers);
- `ask_user` files a typed question with a class and options;
- `capability_request` generalises `credential_request` and shares its
  park-and-wake;
- `workers_list` shows names and capabilities to the orchestration
  conversation only.

REST under `/api/work`, `/api/workers`, `/api/questions`, with SSE progress
events; `POST /api/work/plan` (the `work_plan` contract and validator, for the
CLI, importers and tests), `GET /api/work/{id}/graph` and
`GET /api/work/archive`. Cascade events carry the `origin` item. CLI:
`rustykrab work ready|list|show|plan|approve|reject|cancel|archive`,
`rustykrab workers`, `rustykrab worker add claude_code --name pinch --repos …`.
Channel commands mirror the CLI subset a phone needs (section 14.2).

### 14.1 `work_plan`

```
work_plan {
  root: WorkItemId | Tmp,       // an existing item (new items without a
                                // parent become its children), or the temp
                                // id of the one new item with no parent
  items: [ {
    tmp: Tmp,                   // client-side id, unique within the call
    parent: Option<WorkItemId | Tmp>,
    kind, title, objective, done_when, constraints, decisions_made,
    artifact_refs, required_tools, required_mcp_servers, worker_kind,
    trigger, preconditions, expires_at, budget,     // section 4 fields
    inputs_from: [WorkItemId | Tmp]
  } ],
  edges: [ { item: WorkItemId | Tmp,
             kind: blocks | waits_for | conditional_on_failure
                 | supersedes | discovered_from,
             depends_on: WorkItemId | Tmp } ],
  rationale: String             // one line, shown in the plan preview
}
-> accepted { root: WorkItemId, ids: { Tmp: WorkItemId },
              held: [WorkItemId], policy: Option<PolicyId>,
              warnings: [ { check: sequential_split, items: [WorkItemId] } ] }
 | rejected { failed: [ { reason, offending: [WorkItemId | Tmp], detail } ] }
reason: unknown_ref | duplicate_tmp | invalid_item | cycle | depth_exceeded
      | too_many_items | over_budget | single_writer_conflict | plan_b_edges
      | edge_onto_active | input_unordered | dead_filing | supersedes_active
      | supersedes_closed | out_of_scope | kind_not_allowed | already_planned
      | rate_limited
```

An edge reads as its `work_item_deps` row: `{ item: b, kind: blocks,
depends_on: a }` means a blocks b; `{ item: new, kind: supersedes,
depends_on: old }` means new replaces old. Existing items are referenced by
real id, new ones by temp id.

The controller validates the whole call before writing anything; any failing
check rejects the graph, nothing is stored, and every failed check returns
with its reason, so the planner can fix the graph in one pass.

| Check | Rejected with |
|---|---|
| every ref is a temp id in this call or an item the caller may see; temp ids are unique | `unknown_ref`, `duplicate_tmp` |
| no cycle over all edge kinds and parent links together | `cycle`, offending = the temp ids on the cycle |
| parent tree depth and item count within the policy caps | `depth_exceeded`, `too_many_items` |
| children's budgets sum within the root's budget (or an existing parent's remaining budget) | `over_budget` |
| no two unordered items in the graph write the same resource | `single_writer_conflict` |
| no `blocks` link joins steps that belong in one worker: the downstream has no other upstream, the upstream no other downstream, and the downstream adds no kind, trigger, precondition, worker constraint or writable resource the upstream lacks | `sequential_split`: a warning recorded as an event on the plan and counted by dreaming (section 10), a rejection once Phase 1 has measured its false-positive rate |
| a `conditional_on_failure` item has no other upstream edge | `plan_b_edges` |
| an ordering edge onto an existing item only while that item is `queued`, `ready` or `blocked` | `edge_onto_active` |
| every `inputs_from` entry is an item the downstream is ordered after (section 4.3) | `input_unordered` |
| no item is filed that is already cancelled or held (section 4.4) | `dead_filing` |
| one accepted graph per planning run | `already_planned` |
| no item of kind `code`: code graphs come only from the delivery compiler | `kind_not_allowed` |
| `supersedes` targets only `queued`, `ready` or `blocked` items under the caller's root | `supersedes_active` (target leased, running or verifying), `supersedes_closed` (target closed), `out_of_scope` |
| supersedes on the same subtree within the policy rate | `rate_limited` |
| each item passes `work_file`'s draft checks | `invalid_item` |

An accepted graph is written in one transaction. When policy requires
approval, the root carries one `needs_consent` question, asked at acceptance.
Only the items that fired a trigger are held (`held` in the result; the whole
graph for an item-count or budget trigger); the rest may be leased at once;
approval releases the held items, and rejection cancels them
(`cancelled(requested)`) with cascade per section 4.5 (section 6.1). Callers:
the orchestration conversation and the `planner` definition. A running worker
never calls `work_plan`: its `result_report` carries drafts, each of which may
name one `supersedes` target under the same root, and the controller submits
them through this validator as one graph (section 6.5). The delivery import
(section 4, "Code slices remain code slices", and section 6.1) calls the same
validator and is the only path by which a `code` graph enters: one parent item
per slice, one child parent per stack layer with the layer's acceptance as
`done_when` and `blocks` edges between layers, and a `code` child per delivery
work item under its layer with `delivery_dependencies` as `blocks` edges.

### 14.2 Views and commands

`rustykrab work show <id> --graph` prints the tree by parent: kind, status
(roll-up with counts for a parent), edges in words, `inputs_from`, and for a
cascade status the origin item. Archived items appear as their one-line
summary.

```
#41 Plan the Lisbon trip             personal  blocked   1/4 done; origin #43
├─ #42 Find flight options           research  done
├─ #43 Find hotels near the venue    research  failed    rungs: retry, repair, switch worker
├─ #44 Book flight and hotel         personal  blocked(upstream_failed) <- #43
│      blocked by #42 #43; inputs from #42 #43
└─ #45 Add the trip to the calendar  personal  blocked(upstream_failed) <- #43
       blocked by #44
```

`rustykrab work plan <id>` previews a plan awaiting approval under root
`<id>`: the same tree plus each item's budget, worker kind, trigger and
writable resources, the budget total against the root's, the planner's
rationale line and the policy that required approval. `work approve <id>`
answers the root's `needs_consent` question and releases the held items;
`work reject <id> [reason]` cancels the held items (`cancelled(requested)`),
cascading per section 4.5, and records the reason. `rustykrab work
archive [list | show <id> | search <text>]` reads `work_item_archive`;
`work ready` and `work list` never include archived items.

| CLI | Channel | Notes |
|---|---|---|
| `work ready`, `work list` | `/work` | open items; parents shown as one roll-up line |
| `work show <id> --graph` | `/work <id>` | the tree to depth 2, one line per item |
| `work plan <id>` | pushed preview | sent when a plan needs approval, with approve and reject buttons where the channel supports them |
| `work approve <id>`, `work reject <id> [reason]` | `/approve <id>`, `/reject <id> [reason]` | same effect as the buttons |
| `work cancel <id>` | `/cancel <id>` | cascades to open children; the reply lists what was cancelled and what had already finished |
| `work archive search <text>` | `/archive <text>` | one-line summaries |

## 15. Verification scenarios

Added as `xfail` first, in the existing e2e harness:

1. A personal task is filed, leased to a local worker with its toolset
   pre-activated, completed, and reported through Telegram with evidence.
2. A coding task is dispatched to a `claude_code` worker in a worktree; the
   controller verifies the commit and changed paths; a result whose claimed
   paths differ from the diff is marked `verification_failed`.
3. A worker reports `needs_decision`; the question reaches the user's channel;
   the answer resumes the item, not the conversation; a defaultable question
   never reaches the user.
4. The daemon restarts mid-lease; the item resumes from its checkpoint or
   returns to `ready`; nothing is marked failed by the restart alone.
5. A worker stalls; the progress ledger triggers repair; the second attempt
   uses the first attempt's evidence.
6. Two items that write the same calendar are never running together.
7. Dreaming emits a proposal from a verifiable signal only; it appears as a
   labelled issue; acceptance on the issue creates a `code` item; an implicit
   signal produces no proposal.
8. A peer node receives `required_tools`, activates them before its first
   prefill, and returns a structured result.
9. An item expires; the user is told; nothing runs.
10. gemma4 and qwen3.8 each complete the distractor-catalog matrix through
    RustyKrab's own `tools_load` append path at 12/12, with the tool block
    unchanged across the run (measured by the Ollama provider's fingerprint
    log).
11. `work_file` with `required_tools` that are registered but unloaded is
    rejected with "load it"; with an unconfigured MCP server it is accepted as
    `needs_tool`.
12. A proposal that would modify the controller, a policy or its own
    measurement is refused below the highest review tier.
13. A worker fails on a tool that does not exist; the ladder files a
    `capability` build item, the tool is built and verified, the original item
    resumes with it activated, and the user is never asked.
14. A failure no classifier recognises is recorded as `unknown`, an `internal`
    item is filed with the raw evidence, and after it lands the same failure
    is classified on replay.
15. A surfaced question whose answer was a recorded default is flagged by
    dreaming as an avoidable escalation and produces a proposal.
16. Each expectation metric in section 1.1 is computed nightly from stored
    events, and a regression produces a proposal naming the metric.
17. A `code` item is leased to a local worker; the controller verifies its
    commit and changed paths and marks it done; the worker's routing record
    for that class improves. A second `code` item of the same class fails
    verification twice on the local worker, escalates to `claude_code`, passes,
    and dreaming files a routing proposal citing both records rather than the
    controller changing the default on its own.
18. A multi-step personal request ("plan the Lisbon trip") is routed to the
    `planner` worker, which starts with `work_plan`, `work_status`,
    `memory_search` and `recall_search` visible. Its first `work_plan` call
    contains a cycle through a parent link; the controller rejects the whole
    graph with `cycle` and the offending temp ids, and no item from it exists
    in the store. The planner's corrected call is accepted with real ids, and
    only then is any item leased.
19. Validation bounds over-decomposition: a twelve-item plan for a three-step
    errand is rejected with `too_many_items`; two `blocks`-chained steps that
    add nothing to each other are accepted with a `sequential_split` warning
    recorded as an event (a rejection once its false-positive rate is
    measured); children
    whose budgets exceed the root's are rejected with `over_budget`; a `code`
    item in a planner's graph is rejected with `kind_not_allowed`. Each
    rejection is an event dreaming can count.
20. In a chain a, b, c (a blocks b, b blocks c) under parent P, b fails. The
    ladder runs on b first (retry, repair, worker switch, each an event on b)
    and nothing downstream changes while it runs. When b's budgets are spent,
    c and every item transitively blocked by b become
    `blocked(upstream_failed)` with origin b; a stays `done`; P's roll-up is
    `blocked` naming b. The user receives one message, however many items are
    blocked, naming P, b, the rung b reached and the items waiting on it.
21. Item b is `conditional_on_failure` on a. When a fails after its ladder, b
    becomes ready and runs before anything is surfaced; b succeeds and the
    user receives only the final report. In a second run a succeeds, and b
    ends `cancelled(cascade)` without ever being leased.
22. Item c is blocked by a and b and declares `inputs_from: [a, b]`; a and b
    are `research` items that each attach a document. c is not ready until
    both are done; at lease time its brief carries both items' evidence and
    artifact refs as typed `artifact_refs`, and nothing from a sibling it did
    not name.
23. Parent P is cancelled with one child done, one running and two queued,
    one of which has a child of its own. The queued children and the
    grandchild end `cancelled(cascade)` without being leased; the running
    child's lease is revoked and it ends `cancelled(cascade)` with its partial
    evidence kept; the done child keeps its status and evidence; the reply
    lists what was cancelled and what had already finished.
24. Item a expires. b, blocked by a, becomes `blocked(upstream_expired)` with
    origin a; c, which `waits_for` a, becomes ready, since expiry is terminal;
    the user is told once, naming a. When parent P expires, its open children
    end `cancelled(cascade)` with P's expiry as origin and nothing under P
    runs.
25. A worker holding b in the chain a, b, c, d finds c and d are wrong and
    returns drafts in its `result_report` in which c2 and d2 supersede them,
    c2 blocked by b; the controller submits them as one graph. c and d end
    `cancelled(superseded)` and c2 runs when b is done. A draft that
    supersedes an item another worker is running is refused whole with
    `supersedes_active` and nothing changes; a further re-plan
    of the same subtree beyond the policy rate is refused with
    `rate_limited`.
26. A `code` slice is imported from the delivery compiler (a recorded
    `StackManifest` fixture until Phase 7): one parent item for the slice, one
    child parent per stack layer with the layer's acceptance as `done_when`
    and `blocks` edges between layers, a `code` child per delivery work item
    under its layer, and `blocks` edges from `delivery_dependencies`. No
    `work_plan` call is made
    and the planner is not invoked; items become ready in the delivery DAG's
    order; a manifest with a cycle is rejected whole with `cycle`.
27. A `personal` item ("book the dentist") and a `research` item run to
    completion and the GitHub adapter's call log shows no issue for either;
    a `proposal` from dreaming appears as a `rustykrab-proposal` issue, a
    `capability` build appears as an issue, and a credential acquisition does
    not. A proposal whose `inputs_from` includes a `personal` item shows it
    only as `local:#N`. A hand edit to a projected title is overwritten on
    the next projection.
28. With 500 closed items seeded, those older than the policy window are
    compacted to one-line summaries in `work_item_archive`; `work list`
    returns only open and recent items; `work archive search` finds a
    compacted item by its summary. No open item is compacted, nor a closed
    item an open item names in `inputs_from`; an open item's
    `discovered_from` link to a compacted item resolves to its summary.
29. A plan over the policy's approval threshold (total budget, item count, or
    an item with a side effect the policy names, such as a payment or a
    message to a third party) is accepted with the triggering items in
    `held`; the preview reaches the user's phone; the held items wait in
    `blocked(needs_consent)` while unheld siblings run; `/approve` releases
    them and `/reject` cancels them with cascade. A plan under the threshold, or covered by delegated judgment,
    is leased at once with the decision recorded, and the user is not asked.
30. A scheduled job fires while an interactive turn runs on the same local
    model; the firing is a work item with `trigger: at(time)` that waits and
    runs after the turn. After a restart, two overdue jobs run one at a time.
    Both appear in `work list`, and no cron path starts a conversation outside
    the controller.
31. A `claude_code` worker that keeps its own task list and a Beads database in
    its worktree returns a result with one `discovered` draft; exactly one
    item is filed, with `discovered_from` set, and nothing from the worker's
    own tracker reaches the store.
32. The daemon restarts in the middle of a graph, with one child leased, one
    held behind a failed sibling and the parent's message pending. Readiness,
    cascade and roll-up re-derive to the same state, nothing is re-planned,
    the leased child resumes or returns to `ready` as in scenario 4, and the
    parent's message is sent exactly once from `work_outbox` (section 6.7).

## 16. Implementation phases

### Phase 0 — Measure

Count mid-run tool-set changes from the Ollama provider's fingerprint log and
outcome capture; run the sub-agent path through the model suite; re-measure
KV interleaving cost with the RAM prompt cache on; run the distractor matrix
through RustyKrab's own `tools_load`. Give the graph caps their first values
from recorded requests: depth, items per plan, inputs per brief and the
`inputs` block's token size, and re-plans per parent (placeholders in sections
6.1 and 6.3 until then).

**Exit:** a base rate for mid-run loads, a measured cost for parent/child
prompt switching, a baseline for scenario 10, and first values for the graph
caps, all in `docs/measurements/`.

### Phase 1 — Work items and a controller skeleton

Add the tables in section 13, the readiness computation, leases, the event
log, the error taxonomy and classifiers, the ladder with per-rung budgets,
`work_file`, `work_status`, `result_report`, and a dispatcher that runs only
`local` workers. `scheduled_jobs` gains `work_item_id`, so a cron firing is an
item the dispatcher serialises with interactive turns (section 12.1).
Scenarios 1, 6, 9, 11, 14 and 30 turn green.

Once the single-item path is green, the graph lands in the same phase, since
it is store and controller code: typed edges on `work_item_deps` with cycle
rejection on every write; cascade transitions (`blocked(upstream_failed)`,
`blocked(upstream_expired)`, `cancelled(cascade)`, `cancelled(superseded)`),
applied only after the upstream item's ladder is spent; `parent` with computed
roll-up and cancel and expiry cascades; `inputs_from` in brief assembly; aging
into `work_item_archive`; the `work_plan` tool and validator; the delivery
import against a recorded manifest fixture; and `work show --graph`,
`work plan` and `work archive` on the CLI; the `work_outbox` notifier; and
`sequential_split` as a warning event. Scenarios 19 to 26, 28 and 32 turn
green, 19 driven by a scripted caller until the planner exists.

**Exit:** a personal task filed in one conversation is completed by a scoped
local worker in another, survives a restart in the queue, and its outcome is
reported through the originating channel by the controller. A three-item
chain whose middle item fails climbs the ladder on that item before anything
cascades, and the user gets one message naming the origin and the rung; a
graph with a cycle is rejected whole with nothing stored; `work list` stays
bounded after aging; `sequential_split` warnings have a measured
false-positive rate, which decides whether it becomes a rejection.

### Phase 2 — Toolsets defined ahead of time

`AgentDefinition.tools` and `mcp_servers`, file-based definitions,
`activate` before the first prefill, the append path for `tools_load`, the
merged search-and-load contract, host-validated catalogs, MCP tools grouped by
server. Scenario 10 turns green.

**Exit:** both default models pass the distractor matrix through RustyKrab with
no tool-block change after turn 0; a coder definition starts with its
filesystem and runtime tools visible.

### Phase 3 — Worker registry and the first external worker

The `workers` table, names, capability advertisement, health, cost tiers, and
the `claude_code` adapter (headless, worktree, allowlist, JSON result), then
`codex`, and the routing record on `workers`. Scenarios 2, 17 and 31 turn
green.

**Exit:** a `code` item runs on a named Claude Code worker, its commit and
paths are verified by the controller, and a false claim is caught. Scenario
13 turns green: the ladder's build rung uses this worker to add a tool.

### Phase 4 — Questions, blocked states and standing judgment

The question router and classes, `ask_user`, `capability_request`, the
standing-judgment policy, park-and-resume for items, the notification rules.
The `planner` definition: a file-based definition (Phase 2) with `work_plan`,
`work_status`, `memory_search` and `recall_search` visible, to which the
orchestration conversation routes multi-step `personal` and `research`
requests; plan-approval thresholds as standing judgment; plan previews pushed
to the channel with approve and reject. Scenarios 3, 5, 18 and 29 turn green.

**Exit:** a blocking question reaches the user's phone, its answer resumes the
item, a defaultable question is answered by policy and recorded, and a stalled
worker is repaired without user involvement. A multi-step personal request is
planned into a graph the controller accepts, previewed on the phone only when
policy requires it, and run to completion without further questions.

### Phase 5 — Peer workers over the tailnet

`work_item_id`, `required_tools` and structured results on
`delegated_tasks`, capability advertisement on pairing, leases instead of
restart-failure, the `peer` worker kind. Scenarios 4 and 8 turn green.

**Exit:** a work item runs on another machine's model with its tools activated
up front, returns a typed result, and survives a restart on either side.

### Phase 6 — Proposals from dreaming

The expectation metrics of section 1.1 computed nightly, the evaluation
criteria of section 10, the `proposal` kind, the ground-truth gate, the GitHub
projection and back-sync with the projection rule by kind (section 11) and
opaque references for local-only items, rate and scope limits, the path from
an accepted proposal to a `code` item. Scenarios 7, 12, 15, 16 and 27 turn
green. `DREAMING.md`'s status is refreshed.

**Exit:** RustyKrab files an improvement it can justify from verifiable
outcomes, the user accepts it on GitHub, and the accepted item is executed by
an external worker under verification. No `personal` or `research` item has
been projected.

### Phase 7 — Integration with delivery

Code items become projections of delivery work items; verification, PR stacks
and merge policy come from the delivery plan; the controller leases to the
delivery roles. The delivery import runs against the live compiler instead of
the fixture, `projects` links its slices through `work_item_id`, and scenario
26 is re-run live.

**Exit:** one work item traced from a channel message to a merged, verified
pull request with the item's evidence in the PR body, and the slice's parent
item's roll-up matching the delivery run's state throughout.

### Phase 8 — Self-management under policy

RustyKrab files, executes and deploys improvements to itself through the
supervisor path in the delivery plan's Phase 12, with the scope limits of
section 10 enforced.

**Exit:** the delivery plan's Phase 12 exit, initiated by a proposal RustyKrab
filed for itself.

## 17. Risks and open questions

- **Sequential work gets split anyway.** Mitigation: the planner files the
  whole graph in one `work_plan` call, validation flags a `blocks` link between
  steps that belong in one worker (`sequential_split`, a warning until
  measured), and the dispatcher never fans out items that share a dependency
  chain. Watch: repair rates on chained items, and `sequential_split` warnings
  per planner run.
- **Handoff loss.** Typed constraints, decisions and references; artifact
  pointers rather than summaries; workers read the origin archive. Watch:
  ticket-fidelity probes from the research notes.
- **Dropped or stale follow-ups.** Code-owned lifecycle, TTLs, expiry
  notifications, preconditions re-checked at lease time. Watch: expiry counts.
- **Confident wrong results.** Verification from evidence; partial credit;
  `verification_failed` is a normal state. Watch: claimed-versus-verified
  mismatch rate per worker.
- **Prompt-injection blast radius.** Workers hold credentials and can act. The
  capability ceiling, `ALWAYS_DENIED`, hop budgets, the single-writer rule,
  worktree isolation, and the external adapters' permission modes all apply;
  proposals cannot touch policy or the controller below the top tier.
- **Cost.** Multi-agent systems use roughly 15x the tokens of a chat. Budgets
  per item, cheapest-qualifying worker, and escalation only on failure.
- **Local coding quality.** A local worker on a `code` item can produce a
  plausible change that is wrong, and small models confabulate around gaps.
  Mitigation: every result is verified from evidence before it counts, slices
  start small under probation, verification failure escalates the item, and
  the default tier for a class moves only through a routing proposal with a
  rollback condition. Watch: verified-versus-claimed and escaped defects per
  worker on `code` items, and the cost per verified item by tier.
- **Over-persistence.** A system told never to give up can spend a budget
  building tools nobody wanted, or act beyond scope to avoid asking. Per-rung
  budgets, the policy stops in section 8, and dreaming's wasted-rung criterion
  bound it; the surfaced message always shows what the ladder spent.
- **Over-decomposition by small models.** A local planner emits twelve items
  for a three-step errand, paying a brief, a lease and a verification per item
  and losing constraints at every handoff. Mitigation: item-count and depth
  caps in `work_plan` validation, `sequential_split` flagging "sequential
  stays in one worker", code work never planned by the planner, and dreaming's
  wasted-rung criterion extended to plan shape (rejections by reason, items
  per completed request). Watch: items per completed request by kind;
  `too_many_items` rejections and `sequential_split` warnings per week.
- **Graph bloat and stale subtrees.** Items filed for a request the user has
  moved on from sit blocked indefinitely and crowd `work list`. Mitigation: a
  policy default `expires_at` on planned roots, expiry cascading to open
  children, aging of closed items into `work_item_archive`, and `work list`
  excluding the archive. Watch: open items by age; blocked items older than
  the policy window.
- **Supersede loops.** A worker re-plans the same subtree on every run,
  cancelling and re-filing without progress. Mitigation: a per-subtree
  supersede rate (`rate_limited`), replacements charged against the root's
  budget, refusal against leased or running items (`supersedes_active`), and
  the recurring-fingerprint criterion applied to re-plans. Watch: supersedes
  per root; roots with more superseded than done items.
- **Cascade storms hiding the root cause.** One failed upstream blocks forty
  items, and the user sees forty messages or a roll-up that says only
  "blocked". Mitigation: cascades wait for the origin's ladder, every cascade
  status and event names its origin item, notifications group by origin, and
  roll-ups report the origin rather than a count. Watch: messages per origin
  failure; cascade events with no origin (target zero).
- **Approval fatigue from plan previews.** If every plan asks, the user
  approves without reading and approval stops meaning anything. Mitigation:
  previews only above policy thresholds (budget, item count, named side
  effects), delegated judgment covering routine plans with the decision
  recorded, and dreaming's avoidable-escalation criterion applied to approvals
  granted unchanged. Watch: previews per week; approvals given unchanged.
  Decision for review: the default thresholds.
- **Personal data on the review surface.** A projected item could carry a
  personal item's title or evidence into an issue. Mitigation: `personal` and
  `research` items are never projected, local-only items appear as
  `local:#N`, and scenario 27 checks the adapter's call log. Watch:
  projection audit failures (target zero).
- **Machine limits.** One resident local model; per-role models are remote or
  external. The RAM prompt cache changes the interleaving cost; Phase 0
  measures it before the scheduler assumes anything.
- **Two plans, one controller.** The delivery plan's controller and this one
  must be the same code. Decision for review: implement this plan's controller
  as the general case in `rustykrab-runtime` and have `rustykrab-delivery`
  specialise it, or the reverse. The same decision settles one semantic gap:
  the delivery's rebase cascade re-opens verified layers, while closed is
  final for every other item (section 4).
- **Graph decisions for review.** Approval timing: at acceptance, so the user
  answers once and early (section 6.1), versus when a held item would run,
  which carries evidence but stops the chain. Whether expiry should release a
  plan B: the plan says no, a plan B answers failure and a step that must run
  regardless is a `waits_for` item (section 4.1). The caps (depth 3, 12 items,
  8 inputs or about 600 tokens, one re-plan per parent) are placeholders until
  Phase 0. `sequential_split` false rejections: it stays a warning until Phase
  1 measures it.
- **Model-shaped harness rules.** The `task_complete` reminder and system-role
  notices were tuned on gemma4 and broke on qwen3.8 within a day of the switch.
  Mitigation: the portable conversation shape in section 12.1, per-model
  behaviour tests in the e2e model suite before any model change, and no rule
  that assumes a model will answer a nudge.
- **Review surface.** GitHub Issues versus Linear; the projection rule by kind
  (section 11) is the same for both.
- **Naming.** Names are cheap and useful; persistent personalities are not in
  scope. Decision for review: whether a worker name carries a persona prompt.
- **Scope.** The delivery construction plan lists about 35 pull requests before
  autonomous implementation; this plan adds roughly 20 more before Phase 6. It
  is a multi-quarter programme even with agents doing the typing.

## 18. Evidence and sources

Research and measurements behind sections 2 and 12, on the author's machine:

- `reports/Tool deferral for small agents.md` (report) and
  `research_notes/Tool deferral for small agents/` (six note files);
- `research_notes/Local executors append after prefix/local_executors.md`;
- `research_notes/M4 migration session/m4-migration-session-export.md`, the
  migration session's own export (2026-09-25), with the M1 notice counts and
  the compaction research summary;
- `research_notes/Late tool binding experiment/results.md` and the JSONL runs
  (gemma4:26b and qwen3.8:27b-mlx, 2026-09-24).

Primary sources cited above: Cemri et al., "Why Do Multi-Agent LLM Systems
Fail?" (MAST, 2025); Kim et al., multi-agent architecture study (2025/2026);
Zhao & Wu, PM-Bench (2026 preprint); Anthropic, "Effective harnesses for
long-running agents" (2025) and "Advanced tool use" (2025); TheAgentCompany
(2024); BFCL V4 leaderboard (2026); NoLiMa (2025); Wang et al. on handoff
boundary metadata (2026 preprint); Factory, "Evaluating compression" (2025);
Beads (steveyegge/beads); Magentic-One (Microsoft, 2024); Cognition, "Don't
build multi-agents" (2025).
