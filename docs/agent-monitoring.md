# Viewing your systems

Open the work manager's private Tailscale HTTPS address on your phone or computer.
Turn on Tailscale using the account your operator has allowed. Chat and
**Work manager** connect automatically and show “Connected with Tailscale”
plus that account. Navigate between them with the header links. No daemon token,
pairing code, browser credential, or new session is needed for this access mode.

If the connection cannot be verified, the page explains how to connect Tailscale
and offers **Connect** to retry. A rejected identity never receives system data.
Tagged Tailscale devices do not supply user identity; use device pairing there.
**Disconnect this view** hides the browser view and clears its saved access.
It does not stop agents, revoke other clients, or rotate the daemon's master
token. **Connect** opens the view again using your verified identity.

Operators enable this only behind Tailscale Serve, with a pinned private HTTPS
origin and explicit owners:

    RUSTYKRAB_TAILNET_AUTH_ORIGIN=https://machine.tailnet.ts.net:8443
    RUSTYKRAB_ALLOWED_ORIGINS=https://machine.tailnet.ts.net:8443
    RUSTYKRAB_TAILNET_USERS=you@example.com

The CLI listener is always loopback. Auth also checks the actual socket peer,
the pinned Host authority and one allowed Tailscale-User-Login header. Forwarded
IP headers cannot establish this trust. Serve strips incoming identity headers
and supplies the authenticated user; Funnel and tagged-device requests do not
supply user identity. Only local processes on the Serve host are inside this
proxy trust boundary. See [Tailscale's identity-header documentation](https://tailscale.com/docs/features/tailscale-serve#identity-headers).
Empty owner lists fail startup rather than granting access to every tailnet
member or a shared-device recipient. Commands retain mandatory Origin checks.

## Device pairing fallback

When automatic Tailscale access is unavailable, open **Use a pairing code or
access token** on either chat or monitoring. Enter the operator-issued,
five-minute, single-use pairing code and a name for this browser. Both views
share the returned revocable device token in session storage; no token appears
in a URL, DOM text or console. Previously saved chat/monitor tokens are migrated
to that shared session key. Successful Tailscale access removes old token copies
so an expired pasted token cannot prevent automatic connection.

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
After [instance consolidation](instance-consolidation.md), v6 owns the shared
work history, schedules and Telegram polling. Legacy services are disabled and
their protected data and service files remain available for rollback. Their
automatic recovery registrations are removed before v6 resumes. The existing
macOS credential backend remains available for hardware-backed integration
credentials; CLI subscription profiles keep their separate native logins.

For the unified deployment, Serve port 443 routes to v6 on loopback 3311.
A previously used 8443 route can remain as a token/pairing fallback; pin the
canonical port-443 origin for automatic owner access.

Set `RUSTYKRAB_BRIEFING_VAULT` to an absolute private directory under the v6
data directory when the old Obsidian service is unavailable. **Saved briefings**
in monitoring lists dated Markdown reports and opens their content and hash.
The scoped integration tool can only create, read and append dated briefings.
No public file server or second daemon is involved.

Schedule **Run history** shows execution runs, the latest work item and delivery
evidence. A completed execution can still have failed or uncertain delivery.
Telegram acknowledgement evidence includes message IDs and the destination topic.
An attempt without a final acknowledgement is uncertain; do not infer delivery
from an `ok` execution record. Restart behavior remains at most one scheduler
delivery attempt, so ambiguous sends are not automatically replayed.

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
