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

## Rollout

1. Grant store and gateway route, with tests: a call outside the grant,
   after expiry, or from a review run is refused.
2. `mcp-bridge`, with an e2e scenario: a scripted native agent calls a
   granted tool through the bridge and its result is attested.
3. `ExternalWorker` passes the bridge to both CLIs behind a flag; the
   selection predicate prefers native+MCP when the flag is on.
4. Measure on real briefs (daily briefing, an integration task), then
   retire the host half for what MCP covers.

## Open questions

- Which host tools go first: the work tools alone, or the integrations
  too (Gmail, CalDAV, Obsidian), which carry the attestation rules?
- Codex's MCP support in `codex exec` under `--sandbox workspace-write`:
  whether the bridge's loopback call is allowed, or needs the sandbox's
  network permission.
- Whether the grant should be per run or per invocation, now that a run
  is one invocation (#710).
