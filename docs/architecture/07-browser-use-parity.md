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
| Screenshot observation | PNG bytes use the multimodal tool-result channel; viewport screenshots report image/CSS dimensions and recent coordinate clicks scale back to the CDP viewport |
| Element actions | Native click, hover, keyboard input, coordinate click, upload, select/options, same-target drag, and bounded wait |
| Navigation actions | Navigate, back, forward, refresh, scroll, scroll-to-text, and wait-for |
| Post-action state | Explicit `applied`/`not_applied`/`unknown` outcome separated from observation status; fresh snapshot returned |
| JavaScript dialogs | Bounded CDP dialog observer with configurable accept/dismiss policy |
| New tabs/popups | Target diff after actions; blocked tabs closed; a single allowed tab may be focused automatically |
| Downloads | Browser-level lifecycle events, sanitized filenames, bounded records, and canonical local-path containment |
| About-blank/empty DOM recovery | Bounded wait, reload, and re-observation before treating the page as empty |
| Navigation restrictions | Pre-request validation and post-render validation across redirects, reads, actions, OOPIFs, and popups |
| Authentication state | Persistent Chrome profile directories and credential-safe field filling |
| CAPTCHA awareness | Provider heuristics plus opt-in, budgeted model interaction through ordinary visible CDP actions; no token injection or external solver |
| Driver drift visibility | Protocol decoder counters, offending method summary, browser product/protocol, and handler health in status |

The RustyKrab agent and tool registry replace browser-use's controller/agent
layer. Copying that Python orchestration would create two competing planning
loops and would not improve CDP execution reliability.

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

### Local model assistance instead of a cloud solver

Browser-use's `CaptchaWatchdog` does not solve a local challenge itself. It
observes solver-started/solver-finished events supplied by browser-use's cloud
browser proxy and pauses the agent while that external service works. RustyKrab
has no equivalent cloud event source.

RustyKrab instead offers an opt-in `modelCaptchaSolver` experiment. Detection
creates a challenge episode, the vision-capable model receives a screenshot
through the tool-result image channel, and only interactions explicitly marked
`captchaAttempt=true` consume the episode's action and wall-clock budgets. The
interaction remains an ordinary visible CDP click, keyboard action, ref action,
or coordinate click. There is no token extraction/injection, protocol bypass,
or third-party solver API.

This approach can handle simple visual or interactive challenges the model can
understand, and it keeps the execution and policy boundaries uniform. It cannot
match the specialized service's challenge coverage or externally reported
solver state. A `cleared` result requires the challenge marker to be absent in
both the post-action page state and a delayed independent detector probe, but it
still proves only DOM disappearance—not that the origin server accepted the
challenge or that the user's larger task succeeded.

The monitor exposes current challenge state, aggregate counts, and the ten most
recent attempts in browser status. Each attempt emits a structured log with
challenge id, URL origin (never path/query), provider, action, result, elapsed
time, and counters. With `RUSTYKRAB_OUTCOME_CAPTURE=1`, the agent also persists a
separate implicit outcome per attempt, attributable to
`browser:captcha:model-assisted` and the concrete `model:<id>`. The common
session id joins those records to the ordinary turn outcome for downstream
task-success analysis. Using a namespaced tool attribution for the model is a
compatibility compromise with the existing three-kind outcome schema; a future
schema revision should give model identity its own first-class dimension.

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

### Attached-browser side effects

Popup focus, permission grants, and download configuration can affect tabs that
RustyKrab did not create. RustyKrab focuses only one unambiguous policy-approved
popup, does not auto-grant permissions, and keeps attached-browser download
tracking disabled unless explicitly configured. These defaults trade some
convenience for a smaller blast radius.

## Remaining work, ordered by value

1. Add a persistent CDP event router or request interceptor so navigation policy
   is enforced continuously and per-target crash events are retained.
2. Add redacted, bounded diagnostic trace artifacts for failures; keep HAR and
   video opt-in because they can contain credentials and personal data.
3. Define a remote artifact protocol before advertising remote uploads or
   downloads.
4. Expand the OOPIF bridge only where real fixtures prove a gap: nested OOPIFs,
   modifier shortcuts, and cross-target drag are the known cases.
5. Add explicit origin-scoped permission operations if a target workflow needs
   them; do not adopt blanket permission grants.
6. Accumulate per-origin, per-provider model-assistance results and compare them
   with downstream task outcomes before deciding whether an external CAPTCHA
   solver is justified. Any external solver must remain separately authorized,
   cost-bounded, and explicit rather than becoming an implied browser ability.

## Verification contract

The relevant live tests launch Chrome and cross real process/network boundaries:

- `live_cross_origin_iframe_snapshot_and_native_actions` serves a cross-origin
  child, proves the parent cannot read it, asserts snapshot refs have an OOPIF
  target id, and independently observes native click, trusted key input,
  text fill, dropdown inspection/selection, and workspace-contained upload.
- `live_native_forms_upload_coordinates_and_send_keys` proves trusted keyboard
  input, dropdown inspection/selection, workspace-contained file upload,
  coordinate clicking, and scroll-to-text against a real renderer.
- `live_model_captcha_attempts_are_bounded_observed_and_multimodal` launches a
  real Chrome process against a local two-step challenge fixture. It proves that
  the screenshot becomes a non-empty image block, trusted actions share one
  challenge id, attempts are counted, and clearance requires two observations.
- The black-box model scenario `model-grounds-a-monitored-captcha-action` boots
  the real daemon against Ollama, sends Gemma a deterministic 640×400 browser
  image with distractors, and requires the screenshot call, a coordinate in the
  target range, `captchaAttempt=true`, and a final cleared report. It is
  repeatable with `--reps`; the report is the model-quality evidence rather than
  the deterministic browser fixture.
- The existing dialog, hanging-action, download, browser-recovery, and public
  Instagram tests exercise their named external boundaries and remain ignored
  in the default unit suite because they launch Chrome or require the network.

These tests verify the browser execution layer and one controlled Gemma visual-
grounding task. They do not prove a real third-party CAPTCHA solve, an
authenticated Instagram submission, a remote-CDP artifact flow, every nested
OOPIF topology, or continuous policy enforcement between tool calls. Those
must remain explicit uncertainty rather than being inferred from controlled
coverage.

The model scenario deliberately stubs the browser result so model scoring is
deterministic, while the live fixture drives real Chrome with deterministic
actions so browser failures are attributable. No current test joins sampled
Gemma output and a real Chrome CAPTCHA fixture in one loop. The two tests cover
both sides of that boundary independently, but emergent failures at their exact
integration remain unverified. Likewise, the detector recognizes visible
reCAPTCHA, hCaptcha, and Turnstile markers; generic or custom anti-bot pages can
remain undetected until their providers are added from observed evidence.
