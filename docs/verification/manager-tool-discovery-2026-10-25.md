# Full manager tool discovery verification

Base: `74d3e9e08d510ea52a44da272c698aa1572df121` (v6.6.8).
Owner direction: retain a full manager toolset rather than restricting managers
to coordination tools or introducing mandatory specialist profiles.

## Change and boundary

`orchestrate::prepare_agent` now derives manager capabilities from the ordinary
registered, available session catalog. The separate manager allowlist is
removed. `RunOptions.denied_tools`, availability, computer-use/subagent opt-ins
and existing tool-specific enforcement are unchanged. Progressive discovery
remains: `tools_list` exposes matching tools on demand using append or rerender
late binding. The manager prompt permits direct routine work and retains the
existing controller for durable assignments, schedules and service actions.

The runtime's new dependency on `rustykrab-tools` is test-only. It allows the
regression to exercise real discovery and execution rather than simulating the
permission filter. Production dependencies and controller/worker policy are
unchanged.

## Causal regression

Added three runtime tests before changing production behavior:

- `manager_has_the_normal_catalog_with_explicit_denies_and_opt_in_gates`
  checks available direct/coordination grants, unavailable/unregistered tools,
  an explicit deny and both settings of computer/subagent opt-ins.
- `manager_discovers_and_executes_mail_after_stale_credential_history`
  runs `run_agent_with_options`, the real runner, `ToolsListTool` and an inert
  email tool. The conversation includes an old credential call, its denial,
  a stale summary and a fresh subscription question. A scripted provider
  discovers email, executes its search and reports the fixture result. Both
  append and rerender modes are tested; email is absent from the initial tools
  array, and append delivers its definition in discovery output.
- `manager_discovery_and_loading_cannot_restore_an_explicitly_denied_tool`
  uses the same runner with email explicitly denied. Search cannot find it,
  exact-name `ToolsLoadTool` reports it forbidden and no email call executes.
  Both late-binding modes are tested.

On the original policy the first two failed: preparation omitted `fs_write`,
and real discovery returned “No tool matched” for email subscription search.
The explicit-deny negative passed. After removing the role filter, all three
passed (0 failed). The same assertions were retained.

This is deterministic execution evidence, with no real account access, network
or model service. It establishes the access/discovery path; it does not evaluate
live model choices, prove that old context caused the original behavior, or
establish the user's actual Cometeer subscription status.

## Required checks

Formatting, architecture-doc checks and environment documentation passed.
Workspace all-target Clippy passed with `-D warnings`. Full workspace tests
passed: **2460 passed, 0 failed, 39 ignored across 39 targets**.
The workspace checks ran outside the outer Codex sandbox so the existing macOS
Seatbelt fixtures could exercise their actual confinement boundary.

Architecture maintenance includes runtime and CLI crate write-ups, the runtime
generated metrics, `OPINION.md` and the outcome history. README and work-manager
operation guidance describe the new behavior. The architecture-review skill's
mechanical check and the affected capability/discovery seams were reviewed;
this was not a full repository architecture audit.

## Deployment and live outcome

This record describes an implementation and local verification. It does not
claim a merged or deployed repair. Existing live owner/budget holds remain
unchanged, and no execution was requeued or original request retried. The
deleted collaboration automation remains off.
