# Operating the v6 work manager

Enable `RUSTYKRAB_WORK_MANAGER=1` on the instance that owns new work. Use its
WebChat (`/`) to give a goal, project, constraints and a clear completion condition.
The manager inspects `work_resources` and uses `work_assign`; the controller
validates, queues and leases a suitable available resource. Work can wait for
capacity, a missing capability or an approval. A conversational acknowledgement
means filed, while the work receipt shows assignment, evidence and verified result.

Keep native execution profiles registered with `--require-max` or
`--require-chatgpt`. Different account profiles can provide distinct capacity;
profiles on one account share subscription limits. Local infrastructure is an
adapter for explicit operations, not an Ollama agent.

## Scheduled work

For example: “Every weekday at 9am Pacific, review project X with an available
Codex runtime and produce a report with cited findings.” The manager creates a
`cron` job with its IANA timezone and execution requirements. At each due time,
the scheduler files a durable item; the controller matches its runtime, tools,
repository and capacity. The same assignment and verification history is visible
in `/monitor.html`, with the schedule's current work link and run history.

`GET/POST /api/schedules`, `GET/DELETE /api/schedules/{id}` and
`POST /api/schedules/{id}/enabled` support authenticated operator clients.
Creation accepts `schedule`, `task`, `timezone`, optional delivery addressing
and `execution` (budget, kind, worker_kind, required_tools, required_mcp_servers,
writable_resources, artifact_refs, constraints and done_when). Requirements must
match registered resources. A code job needs an exact `repo:/absolute/path`
resource. No available matching worker means blocked/waiting work, not permission
to execute through another model. Read operations do not run the scheduler.

Native briefs contain the scheduled instructions and explicit requirements.
They do not include old job outputs. Those remain in authenticated run history.
A native runtime does not automatically inherit the main daemon's integrations
or a local SKILL.md; register an appropriate resource before assigning such work.
Pause prevents future firings; cancel an already filed item separately if needed.
Deletion cancels open orphan firings on the next scheduler pass. Local worker
conversation/skill continuation remains supported independently.

## Constituent services

Set `RUSTYKRAB_SERVICE_RESOURCES` to an operator-owned JSON file outside writable
agent repositories. On macOS, each array entry contains `id`, `role`, `health_url`,
`binary`, `launchd_label`, `plist`, and optional `ensure_running` (default false).
Health must be `http://127.0.0.1:PORT/api/health` or IPv6 loopback on an explicit
port different from the manager. Binary and plist paths must be absolute files.
Only register the intended existing launchd job. Keep config directories private.

Every fifteen seconds, the host checks health with the registered loopback Origin, kernel executable identity and
launchd PID ownership. The dashboard displays the installed binary's version,
probe time and supervision. Ensure running/Restart files a work item; only the
infrastructure adapter performs the fixed launchctl action, serially, and records
fresh health plus ownership before reporting success. Conflicting/non-loopback
listeners or unknown process inspection block lifecycle execution.

`ensure_running: true` permits recovery of a verified absent service. The manager
must hold its controller lock and not be draining. Open actions suppress another
attempt; the retry gap is two minutes with at most three attempts per hour.
Truncated work observations suppress automatic recovery. A failure remains
visible; the manager does not repeatedly restart a running unhealthy service.
Launchd continues supervising the manager itself.

## Private access and migration

Expose the manager's loopback port using Tailscale Serve. Configure automatic browser access with a pinned HTTPS Serve origin and explicit
Tailscale owners (see docs/agent-monitoring.md). Chat and monitoring then use
the verified tailnet identity without a browser token. Revocable device tokens
and one-time pairing remain available as a shared session-storage fallback. Monitoring, resources, schedules and command routes
retain the gateway's bearer and Origin checks. Prometheus is protected too.

Enabling manager mode does not migrate another instance's cron database,
Telegram polling or integrations. Transfer these deliberately, preserving their
execution resource and delivery route, and disable the old schedule owner before
starting the replacement. Keep the main and builder data/queues intact. Never
run two Telegram pollers for one bot or duplicate a legacy cron firing.

## Prefer a local model for lighter work

Set `RUSTYKRAB_LOCAL_RESOURCES` to an absolute operator-owned JSON path:

```json
{
  "model": "qwen3.8:27b-mlx",
  "url": "http://127.0.0.1:11434",
  "context_tokens": 32768,
  "repositories": ["/absolute/registered/repository"]
}
```

This adds `qwen-code` at tier zero, independently of the conversational provider.
With `RUSTYKRAB_INTEGRATION_WORKER=1`, it also adds `qwen-integrations` at tier
zero and retains the subscription integration worker at tier one. Both local
roles share one inference slot. Model identity, health, assignment, finalized
usage and verified outcomes appear in the ordinary monitoring view.

Use `worker_kind: any` with the exact registered repository for basic coding.
The local coding worker exposes exec in its isolated worktree; required host
tool names should be omitted when native CLI fallback is desired. Integration
work explicitly requires its host tools and retains read-only Gmail/calendar,
HTTPS research and dated-note restrictions. The controller chooses the cheapest
compatible worker; failure escalation can choose a compatible higher tier. A
busy local worker causes a wait. Complex coding can explicitly require Claude
Code or Codex. Historical approval, budget and decision holds are not released
by registering a model.

Local coding check claims now carry host-observed command_run evidence before
completion. Monitoring distinguishes verified command execution from model
claims; the command record alone does not establish that a test passed.

The local coding brief specifies a per-command bot author for Git commits.
Exec has a sanitized HOME and does not inherit the operator's global Git identity;
the worker must not change shared repository/global Git configuration to commit.


## Work-item discussions and decisions

Open an item in the monitor to read its objective, discussion and pending decisions. The attention inbox counts work items needing direction and links to them. Questions are blockers attached to that item; they are not additional work assignments. Exact unresolved repetitions are consolidated while preserving their history and old notification links. Different questions on the same item retain separate explicit answers.

Use **Post comment** to leave direction or context. User/agent authorship is recorded by the host; comments are available to future agents on the same item and its descendants. Comments do not resume a blocked item, change its acceptance contract or authorize a new resource. Use **Record decision** on the question to answer it; the controller resumes the item when its other blocking questions are resolved. Consent and approval retain their explicit yes/no or approve/reject paths.

Agents can read or comment with work_discuss. In Telegram, use `/comment <work id> <text>`; `/answer <question id> <answer>` records a decision. The next worker receives recent discussion in a bounded prompt, with full native discussion retained in a private file for selective reading. Refresh the discussion to see new updates; drafts are preserved on refresh and errors.

The bounded discussion excerpt prioritizes recent user comments across parent and item history before filling the remaining space with agent updates. Selected entries are presented chronologically; omitted discussion remains available in the frozen snapshot. The 8 KiB total includes framing.


## Choosing how to start work

The monitor now has five separate views: **Attention** for decisions and
failures, **In progress** for queued/assigned/executing/verifying work,
**Results** for completed work and briefings, **Automatic work** for your
reviewed-task dataset, and **Systems** for agents/infrastructure/schedules.
Attention is the default view. Every item still opens its discussion and receipt.

When starting a proposal or approving a task, choose **Start once**, or select
**Allow this exact work template to start automatically in future**. Add your
reason/direction and set the maximum automatic starts per 24 hours (default 3).
The controller captures the actual goal, completion criteria, constraints,
resolved project identity and artifact references, tools, resources, trigger, inputs, preconditions,
review facets and budget. Future work must match those fields exactly. Changes
need another decision. Plans with multiple held items and protected changes
require individual review. One-time decisions supply examples without future
permission; this dataset is not yet semantic model training.

Automatic work lists the examples, scope and who approved them. Download the
versioned JSON dataset or revoke an automatic entry. Revocation affects future
starts; existing work remains tracked. Results show execution work rather than
mistaking an accepted proposal for a completed implementation. Quality metrics
based on closed tasks exclude the blocked backlog.

Telegram supports `/startwork <work id> once [note]` and
`/startwork <work id> automatic [note]`; automatic defaults to 3 starts per day.
Use `/revoke <preference id>` to stop future automatic matching. Browser and
Telegram use the same authenticated controller command. Chat still captures
and files goals; use the resulting work link to inspect and choose its start
permission. Natural-language similarity alone never creates a permission.

Preference notes are recorded in the owning discussion and copied to accepted
implementation comments/constraints. Automatic starts carry the saved direction
with the policy actor; recording notes does not widen tool/resource authority.
