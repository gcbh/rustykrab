# RustyKrab's host tools for native Claude Code and Codex, over MCP

Status: proposal, for agreement before any code (2026-10-15). It touches
`external_worker.rs` and `native_resources.rs`, which the anti-fragility
sessions own for their open PRs (#694–#708), so the contract below is
theirs to agree as much as Geoffrey's.

## Why

A native worker runs Claude Code or Codex in its own harness
(`ExternalWorker`), except when a brief needs one of RustyKrab's own tools
(`NativeWorker::uses_host`: any `required_tools`, or a writable resource
that is not a repository). Then it runs the *host half*: the same model as
a JSON next-turn generator inside RustyKrab's loop (`claude -p --tools ""
--max-turns 3 --json-schema …` per turn, `codex_turn.rs` likewise). That
path strips the agent of its own tools and harness for the whole run, to
reach a handful of RustyKrab tools (Gmail, CalDAV, Obsidian, the work
tools). Feather's lesson, and Geoffrey's decision of 2026-10-13 (no forced
toolsets on Claude or Codex), point the other way: run the agent in its
own harness and give it RustyKrab's tools as tools.

## Shape

1. **A run-scoped grant.** When a native run is leased and its brief needs
   host tools, the daemon mints a grant: a random bearer token bound to
   - the work item and run id, and the actor `worker:<name>`;
   - the tools it may call: the brief's `required_tools` (and MCP servers)
     within the worker's host-tool ceiling, nothing else;
   - read-only, when the brief is a review (`read_only_review`): no tools;
   - an expiry at the run's wall deadline, and a call quota.
   The grant is revoked when the run ends, however it ends.
2. **A stdio bridge.** `rustykrab-cli mcp-bridge` speaks MCP over stdio
   (`initialize`, `tools/list`, `tools/call`) and forwards each request to
   the daemon over loopback with the grant (`RUSTYKRAB_MCP_GRANT` in its
   environment, never in argv). Stdio is what both CLIs load the same way:
   Claude Code through `--strict-mcp-config --mcp-config` (already passed,
   empty, today), Codex through `-c mcp_servers.rustykrab.command=…`.
3. **Execution stays in the daemon.** A gateway route resolves the grant,
   lists exactly the granted tools (their real schemas), and runs a call
   with `WORK_RUN_CONTEXT` bound to the grant's item and actor, through the
   same wrappers the host half uses today (`ObservedTool` and the
   integration attestation), so credentials never leave the daemon and
   every call is observed and attested as it is now.
4. **One route per run.** `NativeWorker` sends a brief to the native half
   whenever the grant covers its needs; the host half remains only for
   what the grant cannot carry, and goes once nothing needs it.

## The contract (from the anti-fragility review, 2026-10-15)

- **Policy identity.** The native+MCP route is its own access path. Its
  proof identity includes the CLI or server policy and the grant's tool
  set; a proof for the host route never attests the native route, and
  advertising a tool over MCP adds no authority the worker's ceiling does
  not already give.
- **Scope.** Work item, run, actor and read-only constraints come from the
  grant, never from the agent's arguments.
- **Capacity.** One in-flight call per grant; calls count against the run's
  quota and share the daemon's capacity with every other route.
- **One selection predicate.** The same predicate picks the route for the
  actual run and for its probe.
- **Diagnostics are not proof.** Outcome capture of MCP calls in the run
  log (as #708 captures native outcomes) never becomes operation proof.

Added in review (2026-10-15):

- **The grant stays out of argv and logs.** Claude's MCP config goes in a
  0600 file in the run's scratch directory (`--mcp-config <path>`), not
  inline JSON in `claude`'s argv. The token carries a recognisable prefix
  so `redact.rs` keeps it out of the run log, the session log and #708's
  summaries. The agent can read its own environment, so it can use the
  token directly; that is acceptable only because the token does no more
  than the bridge can: the same grant on the same route.
- **A scoped route.** A grant authorizes the MCP route and nothing else:
  never the owner bearer API or any other gateway route. Loopback only,
  rate-limited per grant, and refused after revocation, mid-call included.
- **Integration policy and attestation hold over MCP.** `integrations.rs`'s
  `allowed()` (reads and dated briefings, nothing outside scope) and the
  rule that a report closes work only with observed calls apply to MCP
  calls exactly as on the host half. The attestation trace is keyed by the
  grant's item and run, so a native report claiming a Gmail action still
  needs an observed call. `writable_resources` stay leased by the
  controller (single writer); the grant carries only what the lease
  reserved.
- **A detectable bridge failure.** The daemon records a per-run "bridge
  connected" event when the bridge's `initialize` and `tools/list` reach
  the route. A native+MCP run whose brief needs host tools and whose
  bridge never connected fails typed (`capability/mcp_unreachable`), not as
  model confusion. That event is the route's first trusted proof of the
  CLI leg; a daemon-side `ToolCallable` probe through the same route (a
  probe-scoped grant) proves the server leg. Until both are observed the
  route's probes stay Unobserved, and a host-route proof never stands in.
- **Selection.** A distinct `ExecutionPath` (`NativeMcp`) in `ProbeScope`,
  so the #707 binding sees a route change and regenerates, and the "flag
  on, grant covers the needs" choice lives in the one shared predicate
  (the successor of `uses_host`) that both the run and its probe use.

## Rollout

1. Grant store and gateway route, with tests: a call outside the grant,
   after expiry, or from a review run is refused.
2. `mcp-bridge`, with an e2e scenario: a scripted native agent calls a
   granted tool through the bridge and its result is attested.
3. `ExternalWorker` passes the bridge to both CLIs behind a flag; the
   selection predicate prefers native+MCP when the flag is on.
4. Measure on real briefs (daily briefing, an integration task), then
   retire the host half for what MCP covers.

## Answered in review

- **First tools:** the work tools (`work_status`, `work_file`, the question
  tools) through the same `WorkBackend` and #698's `read_only_guard`; the
  integrations second, once attestation over MCP is tested.
- **Codex's sandbox:** Codex is believed to start MCP servers itself,
  outside the shell sandbox, but that is unverified: rollout step 2's e2e
  asserts it with a real `codex exec --sandbox workspace-write` (bridge
  connected, a call attested) rather than assuming it.
- **Per run or per invocation:** per run, since a run is one invocation
  (#710); per invocation if resumes ever return.
