# Agent monitoring

Run the daemon from this checkout and open `/monitor.html` on its gateway
(default `http://127.0.0.1:3000/monitor.html`). The Chat page also links to
Agent monitor. A new remote browser can connect with a one-time pairing code.
An operator runs `rustykrab pair` on the daemon host (for the separate M4
service, `~/.local/bin/rustykrab-agents pair`) and gives the eight-character
code to the user. Enter it under **One-time pairing code**, name the device,
and choose **Pair and connect**. The code expires in five minutes and is
consumed once. The browser exchanges it at the existing `/api/pair` route
and stores its revocable device token in session storage before loading the
monitor; no token is put in a URL or shown in page text. An operator can revoke
the device through `DELETE /api/devices/{id}`. Pairing still requires an
allowed Origin and is rate-limited.

Under **Already have an authentication token?**, a daemon, device, or existing
Chat token can also be used. Tokens stay in session storage and request headers.

The dashboard refreshes every five seconds while visible. It shows registered
agents, actual assignments, controller ticks, missed heartbeats, blocked
reasons, recent events, finalized run usage, verified evidence and the latest
nightly evaluation. Open a work item for its objective, done criteria,
constraints, errors, recovery attempts and evidence. Pending decisions,
consents and plan approvals can be answered there. Credentials use their
existing secure notification page.

## Tailscale HTTPS access

Keep the gateway bound to loopback and publish it through
[Tailscale Serve](https://tailscale.com/docs/reference/tailscale-cli/serve).
Inspect existing routes first with `tailscale serve status --json`. Preserve
those routes and use a separate HTTPS port when another daemon already owns 443.
On macOS the CLI may be `/Applications/Tailscale.app/Contents/MacOS/Tailscale`.

For a separate service listening on port 3311, set its environment before
starting it (replace the hostname with this machine's Tailscale DNS name):

```sh
RUSTYKRAB_PORT=3311
RUSTYKRAB_ALLOWED_ORIGINS=https://machine.your-tailnet.ts.net:8443
RUSTYKRAB_PUBLIC_URL=https://machine.your-tailnet.ts.net:8443
```

Then configure the unused Serve port:

```sh
tailscale serve --bg --https=8443 http://127.0.0.1:3311
```

Open `https://machine.your-tailnet.ts.net:8443/monitor.html` from a device
connected to the tailnet with access to this node and port. Pair that browser
with a code from this service, or use an existing token. The exact HTTPS origin,
including the port, must be allowed by the daemon. The dashboard's same-origin GET proof works through
Serve; action requests continue to require the allowed Origin. No public
Funnel is needed. Remove only this route with `tailscale serve --https=8443 off`.

A remote CLI or collector uses that HTTPS base URL as `RUSTYKRAB_GATEWAY_URL`
and this service's own token as `RUSTYKRAB_AUTH_TOKEN`; the CLI supplies Origin.
A separate instance needs its own `RUSTYKRAB_DATA_DIR`, workspace, encryption
key and token. It observes its own registered fleet and work; starting it does
not import another daemon's work or discover unrelated Claude/Codex sessions.

The local M4 installation uses `com.gcbh.rustykrab.agents`, loopback port 3311,
and `https://m4-32gb.tail84017e.ts.net:8443/monitor.html`. Its data and private
service environment live under `~/.local/share/rustykrab-agents` (directory
0700, environment and token files 0600). It starts at login and restarts after
a crash. `~/.local/bin/rustykrab-agents monitor --check` uses its own token and
loopback endpoint. The bearer token is in that data directory's `auth-token`
file; do not copy the service environment, which also contains the encryption
key. This local build keeps the shared macOS Keychain disabled to isolate it
from the installed main daemon. Credential integration writes require a
separately configured secure backend; unavailable integrations remain absent.
Planning uses the selected Claude Max CLI profile. Execution uses isolated
Claude Max and Codex ChatGPT CLI workers; local/Ollama execution is disabled.
Native workers edit only their configured repositories in isolated worktrees.

To stop the agent service use
`launchctl bootout gui/$(id -u)/com.gcbh.rustykrab.agents`. Remove its
`~/Library/LaunchAgents/com.gcbh.rustykrab.agents.plist` to prevent the next
login from starting it. Retain the data directory and encryption key for
recovery. The main daemon and its port-443 Serve route are independent.

## Terminal and automation

```sh
rustykrab monitor
rustykrab monitor --json
rustykrab monitor --json --check
```

`--check` exits 0 for healthy observations and 1 for degraded or critical
ones. Authentication and connection errors also exit nonzero. It uses the
same gateway URL and authentication resolution as `work` and `worker`.

| Endpoint | Purpose |
|---|---|
| `GET /api/monitor?limit=200&events=50` | JSON observation; limits clamp to 500 items and 200 events |
| `GET /api/monitor/metrics` | Prometheus exposition for collection by an existing monitoring service |
| `GET /api/work/events` | Existing durable event stream, resumable at the snapshot's `event_cursor` |

Both monitor endpoints require the daemon bearer token. Automated clients
send an accepted `Origin`; a browser GET may instead prove same-origin access
through Fetch Metadata and a matching referrer. Commands still require Origin.
For a Prometheus scraper, use its authorization credentials file and a matching Origin header.
Point it to this daemon's host and port with `metrics_path: /api/monitor/metrics`. Keep the credentials file outside the repository.

Metrics include controller runs, failed ticks and last completed tick;
work counts by state, archives, pending questions and notifications; exact
active assignments and health per agent; and judged results, repairs, tokens
and wall time per worker and work class. `rustykrab_monitor_alert` carries a
severity and stable code. A collector should also alert on its own failed
scrape (`up == 0`), since an unreachable daemon cannot report its own outage.

## What the observation proves

Counts and assignments cover every durable row. Displayed work is bounded
and prioritizes active work; truncation is explicit. A snapshot's work rows,
counts, evidence and event cursor share one read transaction. Worker and loop
observations are taken separately and can change while a run finishes.

Viewing the monitor never schedules work, checks a model, changes a lease,
or updates an agent's last-check timestamp. A completed claim counts as
verified only through the controller's existing verifier. Run and summary
records may remain unverified after completion; the dashboard labels them as
such. Agent result counters describe the registry's routing history, which
can lag current work. Token and wall
totals cover finalized run records, so active usage may still be pending.
An empty evaluation sample reads as “No observations.”

Normal approval waits, planned notification delays, shutdown drain and a
second daemon waiting on the controller lock are informational. Repeated
tick failures, stale ticks, expired heartbeats, missing leaf leases and
wall-budget overruns are faults. An overdue notice warns after five minutes
past its due time. Connection loss visibly marks the last dated observation
as disconnected. Missing or stale worker health checks warn after two minutes.

## Verification

The real-router tests enforce authentication, origin and read-only behavior.
Fault tests distinguish stale ticks and worker checks, lock handoff, lease loss, wall budgets
and normal waits. Store tests verify bounded display with complete counts,
verification evidence and event-cursor replay. The scripted end-to-end
scenario `control/agent-monitor-verification-metrics-and-cli` runs work through
the real daemon and checks its verified evidence, agent, released lease,
CLI health exit, metrics and served dashboard assets.

The broader model and compaction measurements are separate suites. Scripted
execution proves wiring and lifecycle; it does not measure a real model's
reasoning quality. PR delivery and self-deployment remain the control plan's
later integration phases.


## Continuing a project with another runtime

Create the durable project through `/api/projects`, including its exact
`repository_path`, intent, constraints and decisions. New work on that repository
automatically uses its unique project; when multiple projects share a repository,
bind the work explicitly:

```json
"artifact_refs": [{"kind": "project", "value": "<project UUID>"}]
```

Children inherit the parent binding. Each newly leased worker receives the
current immutable planning revision, work objectives/constraints/decisions,
questions/answers and verified progress. A coding run starts from the completed
project's verified commit chain, so a fresh Claude or Codex CLI sees the files
already built. Archived work still contributes through durable receipts and
verification evidence. Unverified partial directories are retained for diagnosis;
their files do not become an accepted code base.

The Project handoffs section shows the revision delivered to each displayed run,
its code base, inherited producing agents and the supplied progress counts. Open
the work item for its exact context evidence and verification record. A correction
creates a new project revision for later runs; old receipts stay immutable.
Missing commits, divergent verified branches, ambiguous project identity and
contexts larger than 128 KiB park work with a recorded resolution question.
The controller never changes the repository's HEAD during a handoff.

Unfinished attempts carry controller-owned run/workspace/branch pointers and
classified failure records in project context. These let the successor inspect
partial work without accepting it as verified code. Model-written workspace
artifacts cannot supply these pointers or repository scope. The continuation
base still excludes every unfinished or falsely reported result.
