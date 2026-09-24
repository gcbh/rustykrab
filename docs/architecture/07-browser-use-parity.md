# Browser-use Parity Boundary

This document compares RustyKrab's browser execution subsystem with
`browser-use/browser-use` at commit
[`fe5ad353`](https://github.com/browser-use/browser-use/tree/fe5ad353091fa2ed5499b94e8fe21094bc2e9e5a).
The comparison is intentionally about browser execution: browser lifecycle,
observation, actions, policy, and recovery. Browser-use's Python agent loop,
model providers, cloud API, telemetry product, and CLI are not browser driver
features and are not being copied into `rustykrab-tools`.

## Driver architecture

RustyKrab remains CDP-native and does not depend on Playwright. Chromiumoxide is
the primary typed CDP client. Current chromiumoxide classifies target type
`iframe` as unknown and polls only page targets, so it cannot attach commands to
site-isolated out-of-process iframe (OOPIF) sessions. RustyKrab fills that gap
with a private raw-CDP bridge that:

1. opens a bounded, short-lived WebSocket to the same browser endpoint;
2. discovers iframe targets belonging to the selected top-level page;
3. attaches with flattened CDP target sessions;
4. snapshots each allowed frame and records its target id in the element ref;
5. routes native mouse, keyboard, and file-input commands back to that target;
6. revalidates its observed URL before and after the action.

This retains Chromium's normal site isolation. The `disableSiteIsolation`
setting remains an opt-in compatibility escape hatch, not the default execution
model.

## Implemented execution parity

| browser-use behavior | RustyKrab implementation |
|---|---|
| CDP browser/profile lifecycle | Managed, attached, and remote CDP profiles; per-profile serialization and recovery |
| Stable page identity | Chrome target ids plus per-session sticky target affinity |
| DOM observation | Bounded DOM-derived snapshots with generation-scoped refs, open shadow roots, same-process frames, and OOPIFs |
| Element actions | Native click, hover, keyboard input, coordinate click, upload, select/options, same-target drag, and bounded wait |
| Navigation actions | Navigate, back, forward, refresh, scroll, scroll-to-text, and wait-for |
| Post-action state | Explicit `applied`/`not_applied`/`unknown` outcome separated from observation status; fresh snapshot returned |
| JavaScript dialogs | Bounded CDP dialog observer with configurable accept/dismiss policy |
| New tabs/popups | Target diff after actions; blocked tabs closed; a single allowed tab may be focused automatically |
| Downloads | Browser-level lifecycle events, sanitized filenames, bounded records, and canonical local-path containment |
| About-blank/empty DOM recovery | Bounded wait, reload, and re-observation before treating the page as empty |
| Navigation restrictions | Pre-request validation and post-render validation across redirects, reads, actions, OOPIFs, and popups |
| Authentication state | Persistent Chrome profile directories and credential-safe field filling |
| CAPTCHA awareness | Provider heuristics in page state; detection only |
| Driver drift visibility | Protocol decoder counters, offending method summary, browser product/protocol, and handler health in status |

The RustyKrab agent and tool registry replace browser-use's controller/agent
layer. Copying that Python orchestration would create two competing planning
loops and would not improve CDP execution reliability.

That replacement is not yet throughput-equivalent. Browser-use asks its model
for up to five ordered actions in one step and executes them sequentially,
aborting the remainder when the URL or focused target changes. RustyKrab's
generic runner accepts several tool calls in one model response, but schedules
them as a parallel tool batch; the browser profile lease then serializes them
in acquisition order rather than declared order. Browser prompts therefore do
not encourage parallel calls, and dependent browser work usually costs one
model round trip per action. A local-model Google Flights evaluation applied
ten browser operations correctly but exhausted a 480-second journey budget
after seventeen model calls. This is an orchestration latency discontinuity,
not a CDP action failure.

## Intentional discontinuities

### Persistent profiles instead of exported storage-state JSON

Browser-use can import and export cookie/local-storage state. RustyKrab keeps
the real Chrome profile directory, which preserves more browser state and is
what makes an existing authenticated production session usable. The tradeoff
is a larger secret-bearing persistence boundary: profile directories must be
protected, and an attached profile exposes the operator's authenticated session
to whatever browser actions are authorized. RustyKrab does not serialize these
secrets into tool output.

### Boundary guards instead of a continuous security watchdog

Browser-use maintains a long-lived navigation watchdog. RustyKrab validates at
every tool boundary and after navigation or action side effects. This prevents
blocked content from being returned and quarantines a detected violation, but
does not prevent a renderer from briefly visiting a blocked URL between tool
calls. Closing that interval requires a persistent browser-level event router
or Fetch-domain request interceptor shared with chromiumoxide. That is the most
important remaining security discontinuity.

### No automatic permission grants

Browser-use grants clipboard and notification permissions by default.
RustyKrab does not. Automatic grants make more sites work, but expand ambient
authority and affect externally owned profiles. Permission management should be
an explicit, origin-scoped operation if added.

### No Playwright browser installer fallback

Browser-use drives the browser through `cdp-use`, but its local-browser
watchdog can still invoke `uvx playwright install chromium` when it cannot find
a browser executable. RustyKrab never imports, launches, or depends on
Playwright. It discovers an installed Chrome, Brave, Edge, or Chromium binary,
or uses the configured executable path. This avoids an implicit network
download and a second browser toolchain, at the cost of a less automatic first
run on machines with no compatible browser installed.

### No HAR, video, or tracing recorder

Browser-use has opt-in HAR and video watchdogs. RustyKrab returns screenshots,
console messages, action stages, protocol health, and fresh page state, but does
not record a full network archive or session video. This reduces disk use and
the risk of persisting credentials, tokens, and personal page content. It also
makes intermittent production failures harder to reconstruct. A redacted,
bounded trace artifact is preferable to enabling these globally.

### CAPTCHA detection, not solving

Browser-use's solver is coupled to its cloud browser service. RustyKrab reports
likely reCAPTCHA, hCaptcha, and Cloudflare Turnstile challenges but does not
claim to solve them. Local automated solving would require an external service,
additional policy, cost controls, and an explicit decision about site terms.

### Local artifact truth

File upload is supported only when paths resolve inside the local workspace.
Completed download paths are reported only for a locally managed browser, or an
explicitly opted-in local attached browser. Remote CDP has no artifact-transfer
channel, so RustyKrab refuses to pretend a path on the remote browser host is a
usable local artifact.

### OOPIF bridge cost and limits

The raw bridge adds a WebSocket connection plus attach/detach round trips to an
OOPIF snapshot or action. This is slower than owning one complete CDP event
router, but keeps the workaround narrow and removable if chromiumoxide gains
iframe-target support. Target association relies on Chrome frame and parent
frame ids. One-level site-isolated frames are live-tested; deeply nested OOPIF
topologies remain a risk. Modifier-key combinations inside OOPIFs and drags
between two different targets are not supported; top-level modifier shortcuts
and same-OOPIF drag are supported.

### Bounded execution and serialized profiles

Actions use fixed stage and total deadlines and are serialized per profile.
This prevents one hung CDP request from blocking the agent indefinitely and
avoids two calls racing the same tab. The costs are limited throughput and an
`unknown` result when the deadline expires after a side effect may have begun.
Unknown actions are deliberately not retried automatically. Successful
mutations include a 100ms renderer-settle interval, matching browser-use's
default gap, so the returned observation is less likely to race a queued event
handler; that interval is a small fixed latency cost on each mutation.

### No ordered multi-action browser step

Browser-use's `max_actions_per_step` defaults to five and its `multi_act`
routine preserves order, freezes the originating selector map, and stops the
sequence on errors, terminal actions, URL changes, or target-focus changes.
RustyKrab does not yet expose an equivalent ordered browser sequence. Reusing
the generic parallel tool batch would be unsafe because several actions could
race one page, and merely serializing the profile lock does not guarantee the
model's declared order. The current one-action-per-dependent-step behavior is
safe and independently observable, but it makes slow local-model inference the
dominant cost on multi-field forms and date pickers. An implementation should
be a bounded browser-owned sequence (maximum five), resolve all refs against
one frozen snapshot, enforce policy and page-change guards after every action,
abort rather than re-target stale remaining actions, and return one final
snapshot plus per-action outcomes.

### Attached-browser side effects

Popup focus, permission grants, and download configuration can affect tabs that
RustyKrab did not create. RustyKrab focuses only one unambiguous policy-approved
popup, does not auto-grant permissions, and keeps attached-browser download
tracking disabled unless explicitly configured. These defaults trade some
convenience for a smaller blast radius.

## Remaining work, ordered by value

1. Add a persistent CDP event router or request interceptor so navigation policy
   is enforced continuously and per-target crash events are retained.
2. Add a bounded, ordered browser action sequence with frozen refs and
   browser-use-compatible abort-on-page-change behavior; do not route it through
   the generic parallel tool executor.
3. Add redacted, bounded diagnostic trace artifacts for failures; keep HAR and
   video opt-in because they can contain credentials and personal data.
4. Define a remote artifact protocol before advertising remote uploads or
   downloads.
5. Expand the OOPIF bridge only where real fixtures prove a gap: nested OOPIFs,
   modifier shortcuts, and cross-target drag are the known cases.
6. Add explicit origin-scoped permission operations if a target workflow needs
   them; do not adopt blanket permission grants.
7. Integrate an external CAPTCHA solver only as a separately authorized service,
   not as an implied local browser capability.

## Verification contract

The relevant live tests launch Chrome and cross real process/network boundaries:

- `live_cross_origin_iframe_snapshot_and_native_actions` serves a cross-origin
  child, proves the parent cannot read it, asserts snapshot refs have an OOPIF
  target id, and independently observes native click, trusted key input,
  text fill, dropdown inspection/selection, and workspace-contained upload.
- `live_native_forms_upload_coordinates_and_send_keys` proves trusted keyboard
  input, dropdown inspection/selection, workspace-contained file upload,
  coordinate clicking, and scroll-to-text against a real renderer.
- The existing dialog, hanging-action, download, browser-recovery, and public
  Instagram tests exercise their named external boundaries and remain ignored
  in the default unit suite because they launch Chrome or require the network.

These tests verify the browser execution layer. They do not prove an
authenticated Instagram submission, a remote-CDP artifact flow, every nested
OOPIF topology, or continuous policy enforcement between tool calls. Those
must remain explicit uncertainty rather than being inferred from unit coverage.
