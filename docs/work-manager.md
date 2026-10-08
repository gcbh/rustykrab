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

Expose the manager's loopback port using Tailscale Serve. Use its existing
revocable device token or one-time pairing; the dashboard keeps tokens in headers
and browser session storage. Monitoring, resources, schedules and command routes
retain the gateway's bearer and Origin checks. Prometheus is protected too.

Enabling manager mode does not migrate another instance's cron database,
Telegram polling or integrations. Transfer these deliberately, preserving their
execution resource and delivery route, and disable the old schedule owner before
starting the replacement. Keep the main and builder data/queues intact. Never
run two Telegram pollers for one bot or duplicate a legacy cron firing.
