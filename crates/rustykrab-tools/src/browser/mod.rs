//! CDP-native browser execution inspired by browser-use's reliability model.
//!
//! Provides a comprehensive browser control surface with:
//! - Multi-profile browser management (isolated Chrome instances)
//! - Browser lifecycle (status/start/stop)
//! - Tab management (tabs/open/close/focus) addressed by Chrome target ID
//! - DOM snapshots with generation-scoped element refs, including OOPIFs
//! - Native CDP actions (click/type/press/hover/select/drag/upload)
//! - Screenshot, navigate, evaluate, console, PDF, scroll
//! - SSRF protection and cookie security

pub mod actions;
pub mod adaptive;
mod captcha;
pub mod config;
pub mod downloads;
pub mod fetcher;
pub mod manager;
mod oopif;
mod policy;
pub mod selectors;
pub mod snapshot;
pub mod stealth;

use async_trait::async_trait;
use base64::Engine;
use chromiumoxide::cdp::browser_protocol::network::Cookie;
use chromiumoxide::cdp::browser_protocol::page::{
    GetNavigationHistoryParams, NavigateToHistoryEntryParams,
};
use chromiumoxide::page::ScreenshotParams;
use rustykrab_core::types::ToolSchema;
use rustykrab_core::{Error, Result, SandboxRequirements, Tool, ToolError};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

use adaptive::AdaptiveStore;
use manager::BrowserManager;
use snapshot::{SnapshotMode, SnapshotOptions, SnapshotStore};

const MAX_CONTENT_BYTES: usize = 50 * 1024; // 50KB cap for page content

#[derive(Debug)]
struct NavigationObservation {
    status: &'static str,
    outcome: &'static str,
    readiness: &'static str,
    browser_degraded: bool,
    elapsed_ms: u64,
    reason: Option<String>,
}

#[derive(Debug, Clone)]
struct ScreenshotGeometry {
    image_width: f64,
    image_height: f64,
    viewport_width: f64,
    viewport_height: f64,
    page_url: String,
    captured_at: Instant,
}

fn scale_screenshot_point(x: f64, y: f64, geometry: &ScreenshotGeometry) -> Result<(f64, f64)> {
    if x < 0.0
        || y < 0.0
        || x > geometry.image_width
        || y > geometry.image_height
        || geometry.image_width <= 0.0
        || geometry.image_height <= 0.0
        || geometry.viewport_width <= 0.0
        || geometry.viewport_height <= 0.0
    {
        return Err(Error::ToolExecution(ToolError::invalid_input(
            "click coordinates fall outside the latest compatible screenshot",
        )));
    }
    Ok((
        x * geometry.viewport_width / geometry.image_width,
        y * geometry.viewport_height / geometry.image_height,
    ))
}

/// Issue `Page.navigate` and observe document readiness under one absolute
/// deadline. A committed but slow page is different from a target session that
/// cannot answer even `document.readyState`; callers use that distinction to
/// decide whether to preserve or rebuild the CDP session.
async fn navigate_with_deadline(
    page: &chromiumoxide::Page,
    url: &str,
    deadline: tokio::time::Instant,
) -> Result<NavigationObservation> {
    let started = std::time::Instant::now();
    match tokio::time::timeout_at(deadline, page.goto(url)).await {
        Ok(Ok(_)) => {}
        Ok(Err(e)) => {
            let browser_degraded = !matches!(
                e,
                chromiumoxide::error::CdpError::ChromeMessage(_)
                    | chromiumoxide::error::CdpError::Url(_)
            );
            return Ok(NavigationObservation {
                status: "failed",
                outcome: if browser_degraded {
                    "unknown"
                } else {
                    "not_applied"
                },
                readiness: "navigate_error",
                browser_degraded,
                elapsed_ms: started.elapsed().as_millis() as u64,
                reason: Some(format!("Page.navigate failed for '{url}': {e}")),
            });
        }
        Err(_) => {
            return Ok(NavigationObservation {
                status: "unknown",
                outcome: "unknown",
                readiness: "navigate_response_timeout",
                browser_degraded: true,
                elapsed_ms: started.elapsed().as_millis() as u64,
                reason: Some(
                    "Page.navigate did not return before the navigation deadline; the target session may be unresponsive"
                        .to_string(),
                ),
            })
        }
    }

    let mut renderer_responded = false;
    while tokio::time::Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let probe_budget = remaining.min(Duration::from_millis(750));
        if let Ok(Ok(result)) =
            tokio::time::timeout(probe_budget, page.evaluate("document.readyState")).await
        {
            renderer_responded = true;
            let ready: String = result.into_value().unwrap_or_default();
            if ready == "interactive" || ready == "complete" {
                return Ok(NavigationObservation {
                    status: "loaded",
                    outcome: "applied",
                    readiness: if ready == "complete" {
                        "complete"
                    } else {
                        "interactive"
                    },
                    browser_degraded: false,
                    elapsed_ms: started.elapsed().as_millis() as u64,
                    reason: None,
                });
            }
        }
        tokio::time::sleep(Duration::from_millis(75)).await;
    }

    Ok(NavigationObservation {
        status: "committed",
        outcome: "applied",
        readiness: "deadline_exceeded",
        browser_degraded: !renderer_responded,
        elapsed_ms: started.elapsed().as_millis() as u64,
        reason: Some(if renderer_responded {
            "navigation committed, but the document did not become interactive before the deadline"
                .to_string()
        } else {
            "navigation committed, but the target renderer did not answer readiness probes"
                .to_string()
        }),
    })
}

async fn navigate_history_entry(
    page: &chromiumoxide::Page,
    delta: i64,
    navigation_policy: &config::SsrfPolicy,
) -> Result<Value> {
    let history = tokio::time::timeout(
        Duration::from_secs(3),
        page.execute(GetNavigationHistoryParams::default()),
    )
    .await
    .map_err(|_| Error::ToolExecution("navigation-history lookup timed out".into()))?
    .map_err(|error| {
        Error::ToolExecution(format!("failed to read navigation history: {error}").into())
    })?;
    let wanted = history.current_index + delta;
    let Some(entry) = history.entries.get(wanted.max(0) as usize) else {
        return Ok(json!({
            "status": "no_history_entry",
            "outcome": "not_applied",
            "direction": if delta < 0 { "back" } else { "forward" },
            "retry_safe": true,
        }));
    };
    policy::validate_requested(&entry.url, navigation_policy)
        .await
        .map_err(|error| Error::ToolExecution(error.into()))?;
    let target_url = entry.url.clone();
    tokio::time::timeout(
        Duration::from_secs(5),
        page.execute(NavigateToHistoryEntryParams::new(entry.id)),
    )
    .await
    .map_err(|_| Error::ToolExecution("history navigation timed out".into()))?
    .map_err(|error| Error::ToolExecution(format!("history navigation failed: {error}").into()))?;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
    while tokio::time::Instant::now() < deadline {
        let ready = tokio::time::timeout(
            Duration::from_millis(750),
            page.evaluate("document.readyState"),
        )
        .await
        .ok()
        .and_then(std::result::Result::ok)
        .and_then(|value| value.into_value::<String>().ok())
        .is_some_and(|state| state == "interactive" || state == "complete");
        if ready {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let guard = policy::enforce_page(page, navigation_policy).await;
    if guard["status"] == "blocked" {
        return Ok(json!({
            "status": "blocked",
            "outcome": "not_applied",
            "direction": if delta < 0 { "back" } else { "forward" },
            "target_url": target_url,
            "navigation_guard": guard,
            "retry_safe": false,
        }));
    }
    Ok(json!({
        "status": "navigated",
        "outcome": "applied",
        "direction": if delta < 0 { "back" } else { "forward" },
        "url": manager::probe_page_url_once(page).await.unwrap_or(target_url),
        "title": manager::probe_page_title_once(page).await.unwrap_or_default(),
        "navigation_guard": guard,
        "retry_safe": false,
    }))
}

async fn page_has_agent_content(page: &chromiumoxide::Page) -> Option<bool> {
    tokio::time::timeout(
        Duration::from_millis(750),
        page.evaluate(
            r#"Boolean(document.body && (
                document.body.innerText.trim().length > 0 ||
                document.body.querySelector('input,button,a,select,textarea,canvas,svg,img,video,[role],[onclick]')
            ))"#,
        ),
    )
    .await
    .ok()
    .and_then(std::result::Result::ok)
    .and_then(|value| value.into_value::<bool>().ok())
}

async fn wait_for_agent_content(page: &chromiumoxide::Page, budget: Duration) -> Option<bool> {
    let deadline = tokio::time::Instant::now() + budget;
    loop {
        match page_has_agent_content(page).await {
            Some(true) => return Some(true),
            None => return None,
            Some(false) if tokio::time::Instant::now() >= deadline => return Some(false),
            Some(false) => tokio::time::sleep(Duration::from_millis(250)).await,
        }
    }
}

/// browser-use retries pages whose HTTP navigation succeeds but yields no
/// agent-visible DOM. Preserve that behavior under an explicit bounded report
/// so a legitimate empty document is not silently mistaken for a driver bug.
async fn recover_empty_page(page: &chromiumoxide::Page) -> Value {
    match page_has_agent_content(page).await {
        Some(true) => return json!({ "status": "not_empty", "reloaded": false }),
        None => {
            return json!({
                "status": "unverified",
                "reloaded": false,
                "reason": "the renderer did not answer the content probe"
            })
        }
        Some(false) => {}
    }

    match wait_for_agent_content(page, Duration::from_secs(3)).await {
        Some(true) => return json!({ "status": "recovered_after_wait", "reloaded": false }),
        None => {
            return json!({
                "status": "unverified",
                "reloaded": false,
                "reason": "the renderer stopped answering while waiting for content"
            })
        }
        Some(false) => {}
    }

    let reload = tokio::time::timeout(Duration::from_secs(5), page.reload()).await;
    match reload {
        Ok(Ok(_)) => {}
        Ok(Err(error)) => {
            return json!({
                "status": "reload_failed",
                "reloaded": false,
                "reason": error.to_string(),
            })
        }
        Err(_) => {
            return json!({
                "status": "reload_timed_out",
                "reloaded": false,
                "reason": "empty-page reload exceeded 5 seconds",
            })
        }
    }

    match wait_for_agent_content(page, Duration::from_secs(5)).await {
        Some(true) => json!({ "status": "recovered_after_reload", "reloaded": true }),
        Some(false) => json!({
            "status": "empty_after_reload",
            "reloaded": true,
            "reason": "the HTTP page still has no text or actionable DOM after one reload"
        }),
        None => json!({
            "status": "unverified_after_reload",
            "reloaded": true,
            "reason": "the renderer did not answer the post-reload content probe"
        }),
    }
}

fn remaining_millis(deadline: tokio::time::Instant) -> u64 {
    deadline
        .saturating_duration_since(tokio::time::Instant::now())
        .as_millis()
        .min(u64::MAX as u128) as u64
}

/// Browser automation tool using Chrome DevTools Protocol.
///
/// CDP-native browser execution with browser-use-compatible action semantics:
/// - Multiple named browser profiles, each an isolated Chrome instance
/// - Browser lifecycle management (status/start/stop)
/// - Tab control (tabs/open/close/focus) by stable Chrome target ID
/// - Accessibility-tree snapshots with element refs for actions
/// - Snapshot-scoped interactions (click ref s4-12, type ref s4-5 "hello")
/// - Native CDP mouse/keyboard/file-input actions, redirect guards, popup
///   handling, dialog handling, and bounded renderer recovery
///
/// Configure via `~/.rustykrab/browser.json` or environment variables:
/// - `CHROME_CDP_URL`: Override default CDP URL
/// - `CHROME_CDP_PORT`: Override default CDP port
/// - `CHROME_EXECUTABLE`: Override browser binary path
/// - `BROWSER_HEADLESS=1`: Run in headless mode
/// - `BROWSER_NO_SANDBOX=1`: Disable Chrome sandbox
pub struct BrowserTool {
    manager: BrowserManager,
    snapshot_store: SnapshotStore,
    adaptive_store: AdaptiveStore,
    captcha_monitor: captcha::CaptchaMonitor,
    screenshot_geometry: Arc<Mutex<HashMap<String, ScreenshotGeometry>>>,
    /// Read access to stored credentials, for `fill_credential`.
    ///
    /// Optional so a browser built without a store still works for
    /// everything else; the action reports itself unavailable rather than
    /// the tool disappearing.
    secrets: Option<rustykrab_store::GuardedSecrets>,
}

/// Resolve the action the caller meant.
///
/// `fill_credential` is the only ref-based operation that lives at the
/// top level; every other one -- click, type, fill, press -- is an
/// `actAction` under `act`. A model holding a ref from a snapshot
/// reaches for `act` + `actAction: "fill_credential"`, which is where
/// the pattern says it should be, and was told the sub-action was
/// invalid.
///
/// Observed, not hypothesised: a trial did exactly this, was refused
/// twice, and fell back to `fill` with the credential's *key name* as
/// the text -- typing `web_..._username` and `web_..._password` into
/// the login form and failing the sign-in. The tool's shape turned a
/// correct intention into a wrong action, so accept the spelling the
/// model already reaches for.
/// The browser tool's parameter schema, as a free function so tests can
/// assert on what the model is actually told without building a tool.
fn schema_parameters() -> serde_json::Value {
    json!({
        "type": "object",
        "properties": {
            "action": {
                "type": "string",
                "enum": [
                    "status", "start", "stop", "profiles",
                    "downloads",
                    "tabs", "open", "close", "focus",
                    "navigate", "back", "forward", "refresh", "snapshot", "act", "click_coordinates", "send_keys", "screenshot",
                    "content", "evaluate", "scroll",
                    "scroll_to_text",
                    "console", "cookies", "pdf",
                    "fetch", "stealth_fetch", "select", "wait_for",
                    "fill_credential"
                ],
                "description": "Action to perform. Required companion fields: open/navigate/fetch/stealth_fetch -> url; close/focus -> targetId; act -> ref + actAction; fill_credential -> ref; evaluate -> expression. Act sub-actions have additional requirements documented on actAction."
            },
            "field": {
                "type": "string",
                "description": "Which part of the login to fill (fill_credential action): 'username' or 'password'. Defaults to 'password'."
            },
            "profile": {
                "type": "string",
                "description": "Browser profile name (default: configured default profile)"
            },
            "url": {
                "type": "string",
                "description": "Required for open, navigate, fetch, and stealth_fetch"
            },
            "targetId": {
                "type": "string",
                "description": "Opaque Chrome target identifier from the 'tabs' or click result. Used by close/focus/navigate/snapshot/act/screenshot/content/evaluate; never infer it from tab order."
            },
            "ref": {
                "type": "string",
                "description": "Complete snapshot-scoped element ref (e.g., 's4-12' or 's4-e12'). Required for act and fill_credential; never reuse a ref after receiving newer page_state/snapshot output."
            },
            "actAction": {
                "type": "string",
                "enum": ["click", "type", "fill", "press", "hover", "select", "drag", "upload", "options", "wait", "fill_credential"],
                "description": "Required when action='act'; every act also requires ref. Companion fields: type/fill -> text; press -> key; select -> value; drag -> targetRef; upload -> path or paths. options inspects a native select. fill_credential uses field='username' or 'password' and never text, so the stored secret does not pass through you."
            },
            "text": {
                "type": "string",
                "description": "Required for actAction='type' or 'fill'"
            },
            "x": {
                "type": "number",
                "minimum": 0,
                "description": "Viewport x coordinate for click_coordinates. Use coordinates only from a screenshot whose coordinate_actions_compatible field is true."
            },
            "y": {
                "type": "number",
                "minimum": 0,
                "description": "Viewport y coordinate for click_coordinates. Use coordinates only from a screenshot whose coordinate_actions_compatible field is true."
            },
            "key": {
                "type": "string",
                "description": "Required for actAction='press' (e.g., 'Enter', 'Tab', 'Escape')"
            },
            "keys": {
                "type": "string",
                "description": "Required for send_keys. Sends trusted CDP keyboard input to the focused element; supports text, special keys, and combinations such as Control+A or Meta+Enter."
            },
            "value": {
                "type": "string",
                "description": "Required for actAction='select'"
            },
            "targetRef": {
                "type": "string",
                "description": "Required target element ref for actAction='drag'"
            },
            "path": {
                "type": "string",
                "description": "For actAction='upload': one existing non-empty file inside the configured RustyKrab workspace."
            },
            "paths": {
                "type": "array",
                "items": {"type": "string"},
                "minItems": 1,
                "description": "For actAction='upload': existing non-empty files inside the configured RustyKrab workspace."
            },
            "clear": {
                "type": "boolean",
                "description": "Clear field before typing (default: true for fill, false for type)"
            },
            "captchaAttempt": {
                "type": "boolean",
                "description": "Mark this act/click_coordinates/send_keys call as one model-assisted CAPTCHA interaction. Valid only while a CAPTCHA is independently detected and modelCaptchaSolver is enabled. This opt-in activates bounded attempt monitoring; never mark ordinary page actions."
            },
            "expect_download": {
                "type": "boolean",
                "description": "For actAction='click': wait for and report a browser download lifecycle event (default: false)."
            },
            "download_timeout_ms": {
                "type": "integer",
                "minimum": 0,
                "maximum": 30000,
                "description": "Maximum time to wait for an expected download to finish (default: 10000, maximum: 30000)."
            },
            "selector": {
                "type": "string",
                "description": "CSS selector — use 'ref' from snapshot instead when possible. For screenshot element targeting or snapshot scoping"
            },
            "expression": {
                "type": "string",
                "description": "Required when action='evaluate'; JavaScript to evaluate"
            },
            "format": {
                "type": "string",
                "enum": ["text", "html", "ai", "aria"],
                "description": "Content format (text/html for content action; ai/aria for snapshot mode)"
            },
            "full_page": {
                "type": "boolean",
                "description": "Full page screenshot (default: false)"
            },
            "direction": {
                "type": "string",
                "enum": ["down", "up", "bottom", "top"],
                "description": "Scroll direction"
            },
            "amount": {
                "type": "integer",
                "description": "Scroll amount in pixels (default: 500)"
            },
            "domain": {
                "type": "string",
                "description": "Cookie domain filter (cookies action)"
            },
            "timeout_ms": {
                "type": "integer",
                "description": "Timeout in milliseconds (wait/navigate, default: 10000)"
            },
            "interactive": {
                "type": "boolean",
                "description": "Snapshot: only include interactive elements (default: false)"
            },
            "compact": {
                "type": "boolean",
                "description": "Snapshot: compact output format (default: false)"
            },
            "depth": {
                "type": "integer",
                "description": "Snapshot: max tree depth (default: 50). Modern SPAs nest interactive elements 25-40 levels deep; raise this if a snapshot returns no/too few elements."
            },
            "highlight": {
                "type": "boolean",
                "description": "Snapshot: paint numbered overlay boxes on each ref so a subsequent screenshot shows the labels (default: false). Overlays auto-clear on the next snapshot."
            },

            "method": {
                "type": "string",
                "enum": ["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD"],
                "description": "HTTP method for 'fetch' (default: GET)"
            },
            "body": {
                "type": "string",
                "description": "Raw request body for 'fetch'"
            },
            "json": {
                "description": "JSON body for 'fetch' (object/array/value sent as application/json)"
            },
            "form": {
                "type": "object",
                "description": "Form-encoded body for 'fetch'",
                "additionalProperties": {"type": "string"}
            },
            "extra_headers": {
                "type": "object",
                "description": "Extra headers for 'fetch'/'stealth_fetch'/'navigate'",
                "additionalProperties": {"type": "string"}
            },
            "cookies": {
                "type": "object",
                "description": "Cookies map for 'fetch' (sent as Cookie header)",
                "additionalProperties": {"type": "string"}
            },
            "user_agent": {
                "type": "string",
                "description": "Custom User-Agent for 'fetch' or 'stealth_fetch'"
            },
            "impersonate": {
                "type": "string",
                "description": "Browser pack to impersonate: chrome, firefox, safari, edge (also accepts versioned variants like 'chrome131')"
            },
            "stealthy_headers": {
                "type": "boolean",
                "description": "fetch: send a coordinated browser-like header pack (Sec-Ch-Ua, Sec-Fetch-*, Accept-Language, etc.)"
            },
            "follow_redirects": {
                "type": "boolean",
                "description": "fetch: follow redirects (default: true)"
            },
            "max_redirects": {
                "type": "integer",
                "description": "fetch: redirect limit (default: 10)"
            },
            "retries": {
                "type": "integer",
                "description": "fetch: retry count on transport failure (default: 0)"
            },
            "proxy": {
                "type": "string",
                "description": "fetch/stealth_fetch: proxy URL (e.g. http://user:pass@host:8080)"
            },
            "verify_tls": {
                "type": "boolean",
                "description": "fetch: verify TLS certificates (default: true)"
            },

            "wait_selector": {
                "type": "string",
                "description": "navigate/stealth_fetch/wait_for: CSS selector to wait for"
            },
            "wait_selector_state": {
                "type": "string",
                "enum": ["attached", "detached", "visible", "hidden"],
                "description": "wait_selector state (default: visible)"
            },
            "network_idle": {
                "type": "boolean",
                "description": "navigate/stealth_fetch/wait_for: wait for the network to be idle (no new requests for ~500ms)"
            },
            "solve_cloudflare": {
                "type": "boolean",
                "description": "navigate/stealth_fetch: best-effort wait for Cloudflare challenge to clear"
            },
            "block_webrtc": {
                "type": "boolean",
                "description": "stealth_fetch/navigate: block WebRTC to prevent IP leaks"
            },
            "hide_canvas": {
                "type": "boolean",
                "description": "stealth_fetch/navigate: add noise to canvas/WebGL fingerprints"
            },
            "disable_resources": {
                "type": "boolean",
                "description": "stealth_fetch/navigate: don't load images/fonts/media (faster)"
            },
            "block_images": {
                "type": "boolean",
                "description": "stealth_fetch/navigate: block image loads only"
            },
            "hide_webdriver": {
                "type": "boolean",
                "description": "stealth_fetch/navigate: hide navigator.webdriver and other automation tells (default: true)"
            },

            "html": {
                "type": "string",
                "description": "select: parse this static HTML body instead of querying the live tab"
            },
            "css": {
                "type": "string",
                "description": "select: CSS selector. Supports Scrapling pseudo-selectors ::text and ::attr(name)"
            },
            "xpath": {
                "type": "string",
                "description": "select: XPath query (live mode only — requires an active tab)"
            },
            "find_by_text": {
                "type": "string",
                "description": "select: filter matches by text (substring or regex when 'regex' is true)"
            },
            "regex": {
                "type": "boolean",
                "description": "select: treat find_by_text as a regex (default: false)"
            },
            "limit": {
                "type": "integer",
                "description": "select: max number of matches to return (default 500, hard cap 500)"
            },
            "include_html": {
                "type": "boolean",
                "description": "select: include each match's outerHTML"
            },
            "auto_save": {
                "type": "boolean",
                "description": "select: store fingerprints of the matches under 'auto_save_id' for adaptive matching later"
            },
            "auto_match": {
                "type": "boolean",
                "description": "select: if the selector returns nothing, locate closest matches by similarity to fingerprints saved under 'auto_save_id'"
            },
            "auto_save_id": {
                "type": "string",
                "description": "select: identifier for the saved fingerprint set"
            },
            "auto_match_threshold": {
                "type": "number",
                "description": "select: minimum similarity (0-1) to accept an adaptive match (default 0.6)"
            },

            "delay_ms": {
                "type": "integer",
                "description": "wait_for/stealth_fetch: extra delay in ms after other waits resolve"
            }
        },
        "required": ["action"],
        "allOf": [
            {
                "if": {
                    "properties": { "action": { "enum": ["open", "navigate", "fetch", "stealth_fetch"] } },
                    "required": ["action"]
                },
                "then": { "required": ["url"] }
            },
            {
                "if": {
                    "properties": { "action": { "enum": ["close", "focus"] } },
                    "required": ["action"]
                },
                "then": { "required": ["targetId"] }
            },
            {
                "if": {
                    "properties": { "action": { "const": "act" } },
                    "required": ["action"]
                },
                "then": { "required": ["ref", "actAction"] }
            },
            {
                "if": {
                    "properties": {
                        "action": { "const": "act" },
                        "actAction": { "enum": ["type", "fill"] }
                    },
                    "required": ["action", "actAction"]
                },
                "then": { "required": ["text"] }
            },
            {
                "if": {
                    "properties": {
                        "action": { "const": "act" },
                        "actAction": { "const": "press" }
                    },
                    "required": ["action", "actAction"]
                },
                "then": { "required": ["key"] }
            },
            {
                "if": {
                    "properties": {
                        "action": { "const": "act" },
                        "actAction": { "const": "select" }
                    },
                    "required": ["action", "actAction"]
                },
                "then": { "required": ["value"] }
            },
            {
                "if": {
                    "properties": {
                        "action": { "const": "act" },
                        "actAction": { "const": "drag" }
                    },
                    "required": ["action", "actAction"]
                },
                "then": { "required": ["targetRef"] }
            },
            {
                "if": {
                    "properties": { "action": { "const": "fill_credential" } },
                    "required": ["action"]
                },
                "then": { "required": ["ref"] }
            },
            {
                "if": {
                    "properties": { "action": { "const": "send_keys" } },
                    "required": ["action"]
                },
                "then": { "required": ["keys"] }
            },
            {
                "if": {
                    "properties": { "action": { "const": "evaluate" } },
                    "required": ["action"]
                },
                "then": { "required": ["expression"] }
            }
        ]
    })
}

fn effective_action<'a>(action: &'a str, args: &serde_json::Value) -> &'a str {
    if action == "act" && args["actAction"] == "fill_credential" {
        "fill_credential"
    } else {
        action
    }
}

impl BrowserTool {
    pub fn new() -> Self {
        Self {
            manager: BrowserManager::from_config(),
            snapshot_store: SnapshotStore::new(),
            adaptive_store: AdaptiveStore::new(),
            captcha_monitor: captcha::CaptchaMonitor::default(),
            screenshot_geometry: Arc::new(Mutex::new(HashMap::new())),
            secrets: None,
        }
    }

    /// Build a tool against an explicit config.
    ///
    /// [`Self::new`] loads `~/.rustykrab/browser.json`, which ties it to the
    /// operator's real Chrome profile and its live sessions. Callers that
    /// need an isolated browser — tests, embedders driving a throwaway
    /// profile — supply their own config here.
    pub fn with_config(config: config::BrowserConfig) -> Self {
        Self {
            manager: BrowserManager::new(config),
            snapshot_store: SnapshotStore::new(),
            adaptive_store: AdaptiveStore::new(),
            captcha_monitor: captcha::CaptchaMonitor::default(),
            screenshot_geometry: Arc::new(Mutex::new(HashMap::new())),
            secrets: None,
        }
    }

    /// Let the browser type a stored credential without the model ever
    /// holding it.
    pub fn with_secrets(mut self, secrets: rustykrab_store::GuardedSecrets) -> Self {
        self.secrets = Some(secrets);
        self
    }

    /// Validate action-specific arguments before touching a browser process.
    ///
    /// The schema exposes these requirements to the model and runner. This
    /// runtime guard also protects direct `Tool::execute` callers and rejects
    /// empty strings, which JSON Schema's `required` keyword cannot detect.
    fn validate_action_args(action: &str, args: &Value) -> Result<()> {
        fn require_non_empty(args: &Value, field: &str, action: &str) -> Result<()> {
            if args
                .get(field)
                .and_then(Value::as_str)
                .is_some_and(|value| !value.trim().is_empty())
            {
                return Ok(());
            }
            Err(Error::ToolExecution(ToolError::invalid_input(format!(
                "browser action '{action}' requires non-empty '{field}'"
            ))))
        }

        if args["captchaAttempt"].as_bool() == Some(true) {
            let supported = matches!(action, "click_coordinates" | "send_keys")
                || (action == "act"
                    && matches!(
                        args["actAction"].as_str(),
                        Some("click" | "type" | "fill" | "press" | "select")
                    ));
            if !supported {
                return Err(Error::ToolExecution(ToolError::invalid_input(
                    "captchaAttempt=true is limited to act/click, act/type, act/fill, act/press, act/select, click_coordinates, and send_keys",
                )));
            }
        }

        match action {
            "open" | "navigate" | "fetch" | "stealth_fetch" => {
                require_non_empty(args, "url", action)?;
            }
            "close" | "focus" => require_non_empty(args, "targetId", action)?,
            "act" => {
                require_non_empty(args, "ref", action)?;
                require_non_empty(args, "actAction", action)?;
                match args["actAction"].as_str().unwrap_or_default() {
                    "type" => require_non_empty(args, "text", "act/type")?,
                    "fill" => require_non_empty(args, "text", "act/fill")?,
                    "press" => require_non_empty(args, "key", "act/press")?,
                    "select" => require_non_empty(args, "value", "act/select")?,
                    "drag" => require_non_empty(args, "targetRef", "act/drag")?,
                    "upload" => {
                        let has_path = args["path"]
                            .as_str()
                            .is_some_and(|value| !value.trim().is_empty());
                        let has_paths = args["paths"]
                            .as_array()
                            .is_some_and(|values| !values.is_empty());
                        if !has_path && !has_paths {
                            return Err(Error::ToolExecution(ToolError::invalid_input(
                                "act/upload requires 'path' or a non-empty 'paths' array",
                            )));
                        }
                    }
                    "click" | "hover" | "options" | "wait" | "fill_credential" => {}
                    other => {
                        return Err(Error::ToolExecution(ToolError::invalid_input(format!(
                            "unknown act action '{other}'. Available: click, type, fill, press, hover, select, drag, upload, options, wait, fill_credential"
                        ))));
                    }
                }
            }
            "fill_credential" => require_non_empty(args, "ref", action)?,
            "click_coordinates" => {
                if args["x"].as_f64().is_none() || args["y"].as_f64().is_none() {
                    return Err(Error::ToolExecution(ToolError::invalid_input(
                        "click_coordinates requires numeric 'x' and 'y'",
                    )));
                }
                if args["x"].as_f64().unwrap_or(-1.0) < 0.0
                    || args["y"].as_f64().unwrap_or(-1.0) < 0.0
                {
                    return Err(Error::ToolExecution(ToolError::invalid_input(
                        "click_coordinates requires non-negative coordinates",
                    )));
                }
            }
            "scroll_to_text" => require_non_empty(args, "text", action)?,
            "send_keys" => require_non_empty(args, "keys", action)?,
            "evaluate" => require_non_empty(args, "expression", action)?,
            "wait_for" => {
                let has_condition = args["wait_selector"]
                    .as_str()
                    .is_some_and(|value| !value.trim().is_empty())
                    || args["network_idle"].as_bool().unwrap_or(false)
                    || args["solve_cloudflare"].as_bool().unwrap_or(false)
                    || args["delay_ms"].as_u64().is_some();
                if !has_condition {
                    return Err(Error::ToolExecution(ToolError::invalid_input(
                        "browser action 'wait_for' requires at least one of: wait_selector, network_idle, solve_cloudflare, delay_ms",
                    )));
                }
            }
            "status" | "start" | "stop" | "profiles" | "downloads" | "tabs" | "snapshot"
            | "screenshot" | "content" | "scroll" | "console" | "cookies" | "pdf" | "select"
            | "back" | "forward" | "refresh" => {}
            other => {
                return Err(Error::ToolExecution(ToolError::invalid_input(format!(
                    "unknown browser action '{other}'"
                ))));
            }
        }
        Ok(())
    }

    /// Resolve the profile name from args, falling back to the default.
    fn resolve_profile<'a>(&'a self, args: &'a Value) -> &'a str {
        args["profile"]
            .as_str()
            .unwrap_or(&self.manager.config().default_profile)
    }

    /// Build a snapshot store key from conversation + profile + target.
    ///
    /// Refs are capabilities over a page element. Keeping them conversation-
    /// scoped prevents one concurrent agent from acting on another agent's
    /// most recent snapshot merely because both share a Chrome profile/tab.
    fn store_key(session: &str, profile: &str, target_id: Option<&str>) -> String {
        match target_id {
            Some(tid) => format!("{session}:{profile}:{tid}"),
            None => format!("{session}:{profile}:active"),
        }
    }

    /// Key for per-conversation browser state.
    ///
    /// One Chrome profile is shared by every concurrent run, so the pinned
    /// tab has to be scoped per conversation or two agents browsing at once
    /// would steer each other's page. Outside a runner scope (CLI, tests)
    /// there is one caller and one browser, so a shared key is correct.
    fn session_key() -> String {
        rustykrab_core::active_tools::with_session_context(|ctx| ctx.conversation_id.to_string())
            .unwrap_or_else(|| "global".to_string())
    }

    /// Actions that operate on a page, and so need one resolved.
    const PAGE_ACTIONS: &'static [&'static str] = &[
        "navigate",
        "back",
        "forward",
        "refresh",
        "snapshot",
        "act",
        "click_coordinates",
        "send_keys",
        "fill_credential",
        "screenshot",
        "content",
        "evaluate",
        "scroll",
        "scroll_to_text",
        "console",
        "cookies",
        "pdf",
        "select",
        "wait_for",
    ];

    /// Read actions whose whole purpose is to observe page content. Landing
    /// one of these on a blank tab is a failure, not an empty result.
    const READ_ACTIONS: &'static [&'static str] =
        &["snapshot", "content", "evaluate", "screenshot", "pdf"];

    /// Resolve the target this call addresses: explicit `targetId`, else the
    /// session's pinned tab if it is still live.
    async fn resolve_target(
        &self,
        action: &str,
        profile: &str,
        session: &str,
        args: &Value,
    ) -> Option<String> {
        if let Some(tid) = args["targetId"].as_str() {
            return Some(tid.to_string());
        }
        if !Self::PAGE_ACTIONS.contains(&action) {
            return None;
        }
        let pinned = self.manager.sticky_target(session, profile)?;
        match self.manager.target_is_live(profile, &pinned).await {
            // Definitely gone: the tab was closed out from under us. Drop the
            // pin rather than failing every subsequent action on it.
            Some(false) => {
                self.manager.clear_sticky_target(session, profile);
                None
            }
            // Live, or the browser could not answer in time. Keep the pin —
            // a slow Chrome is not a reason to lose the page mid-task.
            _ => Some(pinned),
        }
    }

    /// Resolve the page for a read action, refusing to report a blank tab
    /// as an empty page.
    async fn page_for(
        &self,
        action: &str,
        profile: &str,
        session: &str,
        target_id: Option<&str>,
    ) -> Result<chromiumoxide::Page> {
        let page = self.manager.get_page(profile, target_id).await?;
        // Bounded: a page that will not answer is not evidence of a blank
        // tab, so an inconclusive probe skips the guard rather than
        // rejecting a read the model may well be able to complete.
        if let Some(url) = manager::probe_page_url_once(&page).await {
            let navigated = self.manager.sticky_target(session, profile).is_some();
            Self::reject_blank_read(action, &url, navigated)?;
        }
        let guard = policy::enforce_page(&page, &self.manager.config().ssrf_policy).await;
        if guard["status"] == "blocked" {
            return Err(Error::ToolExecution(
                format!(
                    "browser navigation policy blocked '{}' before '{action}': {}",
                    guard["url"].as_str().unwrap_or("unknown URL"),
                    guard["reason"].as_str().unwrap_or("policy rejected URL")
                )
                .into(),
            ));
        }
        Ok(page)
    }

    /// Re-check the URL after a page operation and before returning any data.
    /// A page can navigate itself while a read is in flight, so preflight
    /// validation alone is not a sufficient confidentiality boundary.
    async fn guard_page_output(
        &self,
        action: &str,
        session: &str,
        profile: &str,
        target_id: Option<&str>,
        page: &chromiumoxide::Page,
    ) -> Result<Value> {
        let guard = policy::enforce_page(page, &self.manager.config().ssrf_policy).await;
        if guard["status"] == "blocked" {
            self.snapshot_store
                .clear(&Self::store_key(session, profile, target_id))
                .await;
            return Err(Error::ToolExecution(
                format!(
                    "browser navigation policy blocked '{}' after '{action}': {}",
                    guard["url"].as_str().unwrap_or("unknown URL"),
                    guard["reason"].as_str().unwrap_or("policy rejected URL")
                )
                .into(),
            ));
        }
        Ok(guard)
    }

    async fn validate_requested_url(&self, url: &str) -> Result<()> {
        policy::validate_requested(url, &self.manager.config().ssrf_policy)
            .await
            .map_err(|error| Error::ToolExecution(error.into()))
    }

    async fn observe_captcha(&self, key: &str, page_url: &str, captcha_state: &Value) -> Value {
        let config = self.manager.config();
        self.captcha_monitor
            .observe(
                key,
                &captcha::safe_origin(page_url),
                captcha_state,
                config.model_captcha_solver,
                config.effective_captcha_max_attempts(),
                config.effective_captcha_timeout(),
            )
            .await
    }

    async fn begin_captcha_attempt(
        &self,
        args: &Value,
        key: &str,
        page: &chromiumoxide::Page,
    ) -> Result<Option<captcha::AttemptStart>> {
        if args["captchaAttempt"].as_bool() != Some(true) {
            return Ok(None);
        }
        let config = self.manager.config();
        if !config.model_captcha_solver {
            return Err(Error::ToolExecution(ToolError::permission_denied(
                "model-assisted CAPTCHA interaction is disabled; set modelCaptchaSolver=true in browser.json to enable bounded, monitored attempts",
            )));
        }
        let page_url = manager::probe_page_url_once(page).await.unwrap_or_default();
        let captcha_state = snapshot::detect_captcha(page).await;
        Ok(Some(
            self.captcha_monitor
                .begin_attempt(
                    key,
                    &captcha::safe_origin(&page_url),
                    &captcha_state,
                    config.effective_captcha_max_attempts(),
                    config.effective_captcha_timeout(),
                )
                .await,
        ))
    }

    fn captcha_rejection(action: &str, mut metadata: Value) -> Value {
        if let Value::Object(object) = &mut metadata {
            object.insert("action".into(), Value::String(action.to_string()));
            object.insert(
                "action_outcome".into(),
                Value::String("not_applied".to_string()),
            );
            object.entry("elapsed_ms").or_insert(json!(0));
            object.entry("clearance_confirmations").or_insert(json!(0));
        }
        json!({
            "status": "captcha_attempt_rejected",
            "outcome": "not_applied",
            "retry_safe": metadata["retry_safe"],
            "browser_degraded": false,
            "action": action,
            "captcha_attempt": metadata,
        })
    }

    async fn finish_captcha_attempt(
        &self,
        ticket: Option<captcha::AttemptTicket>,
        action: &str,
        page: &chromiumoxide::Page,
        outcome: &mut Value,
    ) {
        let Some(ticket) = ticket else {
            return;
        };
        let post_state = outcome["page_state"]["captcha"].clone();
        let action_outcome = outcome["outcome"].as_str().unwrap_or("unknown").to_string();
        let confirmation = if action_outcome == "applied" {
            tokio::time::sleep(Duration::from_millis(350)).await;
            snapshot::detect_captcha(page).await
        } else {
            json!({"detected":false,"providers":[],"status":"unverified"})
        };
        let attempt = self
            .captcha_monitor
            .finish_attempt(ticket, action, &action_outcome, &post_state, &confirmation)
            .await;
        if let Value::Object(object) = outcome {
            object.insert("captcha_attempt".into(), attempt);
        }
    }

    async fn map_screenshot_coordinates(
        &self,
        key: &str,
        page: &chromiumoxide::Page,
        x: f64,
        y: f64,
        require_fresh_screenshot: bool,
    ) -> Result<(f64, f64, Value)> {
        let geometry = self.screenshot_geometry.lock().await.get(key).cloned();
        let page_url = manager::probe_page_url_once(page).await.unwrap_or_default();
        let usable = geometry.filter(|geometry| {
            geometry.page_url == page_url
                && geometry.captured_at.elapsed() <= Duration::from_secs(60)
        });
        let Some(geometry) = usable else {
            if require_fresh_screenshot {
                return Err(Error::ToolExecution(ToolError::invalid_input(
                    "captcha coordinate actions require a compatible viewport screenshot from the current page within the last 60 seconds",
                )));
            }
            return Ok((
                x,
                y,
                json!({
                    "status": "unavailable",
                    "input_space": "css_viewport_fallback",
                    "dispatched_x": x,
                    "dispatched_y": y,
                }),
            ));
        };
        let (mapped_x, mapped_y) = scale_screenshot_point(x, y, &geometry)?;
        Ok((
            mapped_x,
            mapped_y,
            json!({
                "status": "scaled_from_recent_screenshot",
                "input_x": x,
                "input_y": y,
                "image_width": geometry.image_width,
                "image_height": geometry.image_height,
                "viewport_width": geometry.viewport_width,
                "viewport_height": geometry.viewport_height,
                "dispatched_x": mapped_x,
                "dispatched_y": mapped_y,
            }),
        ))
    }

    fn captcha_action_error(action: &str, error: Error) -> Value {
        json!({
            "status": "captcha_action_failed",
            "outcome": "not_applied",
            "retry_safe": true,
            "browser_degraded": false,
            "action": action,
            "error": error.to_string(),
            "page_state": null,
            "page_state_status": "failed",
        })
    }

    fn validated_upload_paths(args: &Value) -> Result<Vec<String>> {
        let mut requested = Vec::new();
        if let Some(path) = args["path"].as_str() {
            if !path.trim().is_empty() {
                requested.push(path.to_string());
            }
        }
        if let Some(paths) = args["paths"].as_array() {
            for path in paths {
                let path = path.as_str().ok_or_else(|| {
                    Error::ToolExecution(ToolError::invalid_input(
                        "every upload path must be a string",
                    ))
                })?;
                if path.trim().is_empty() {
                    return Err(Error::ToolExecution(ToolError::invalid_input(
                        "upload paths cannot be empty",
                    )));
                }
                requested.push(path.to_string());
            }
        }
        requested.sort();
        requested.dedup();
        if requested.is_empty() {
            return Err(Error::ToolExecution(ToolError::invalid_input(
                "act/upload requires at least one file",
            )));
        }

        requested
            .into_iter()
            .map(|path| {
                let canonical = crate::security::validate_path(&path)
                    .map_err(|error| Error::ToolExecution(error.into()))?;
                let metadata = std::fs::metadata(&canonical).map_err(|error| {
                    Error::ToolExecution(
                        format!("cannot inspect upload file '{path}': {error}").into(),
                    )
                })?;
                if !metadata.is_file() {
                    return Err(Error::ToolExecution(ToolError::invalid_input(format!(
                        "upload path is not a regular file: '{path}'"
                    ))));
                }
                if metadata.len() == 0 {
                    return Err(Error::ToolExecution(ToolError::invalid_input(format!(
                        "upload file is empty: '{path}'"
                    ))));
                }
                Ok(canonical.display().to_string())
            })
            .collect()
    }

    /// Enforce policy on tabs created by one browser action and optionally
    /// focus the sole allowed popup, matching browser-use without guessing
    /// when several tabs appear together.
    async fn guard_new_tabs(
        &self,
        profile: &str,
        session: &str,
        before: Option<HashSet<String>>,
    ) -> Value {
        let Some(before) = before else {
            return json!({
                "status": "unverified",
                "reason": "could not capture the pre-action target set"
            });
        };
        // Let Target.attachedToTarget propagate through chromiumoxide.
        tokio::time::sleep(Duration::from_millis(75)).await;
        let after =
            match tokio::time::timeout(Duration::from_secs(3), self.manager.target_ids(profile))
                .await
            {
                Ok(Ok(ids)) => ids,
                Ok(Err(error)) => {
                    return json!({ "status": "unverified", "reason": error.to_string() })
                }
                Err(_) => {
                    return json!({
                        "status": "unverified",
                        "reason": "post-action target listing timed out"
                    })
                }
            };
        let mut new_targets: Vec<String> = after.difference(&before).cloned().collect();
        new_targets.sort();
        let mut allowed = Vec::new();
        let mut blocked = Vec::new();
        let mut unverified = Vec::new();

        for target in new_targets {
            let page = match self.manager.get_page(profile, Some(&target)).await {
                Ok(page) => page,
                Err(error) => {
                    unverified.push(json!({ "targetId": target, "reason": error.to_string() }));
                    continue;
                }
            };
            let guard = policy::enforce_page(&page, &self.manager.config().ssrf_policy).await;
            match guard["status"].as_str() {
                Some("allowed") => allowed.push(target),
                Some("blocked") => {
                    self.snapshot_store
                        .clear(&Self::store_key(session, profile, Some(&target)))
                        .await;
                    let closed = self.manager.close_tab(profile, &target).await.is_ok();
                    blocked.push(json!({
                        "targetId": target,
                        "url": guard["url"],
                        "reason": guard["reason"],
                        "closed": closed,
                    }));
                }
                _ => unverified.push(guard),
            }
        }

        let focused_target = if self.manager.config().auto_focus_new_tabs && allowed.len() == 1 {
            let target = &allowed[0];
            if self.manager.focus_tab(profile, target).await.is_ok() {
                self.manager.set_sticky_target(session, profile, target);
                Some(target.clone())
            } else {
                None
            }
        } else {
            None
        };

        json!({
            "status": if allowed.is_empty() && blocked.is_empty() && unverified.is_empty() {
                "no_new_tabs"
            } else {
                "processed"
            },
            "allowed_targets": allowed,
            "blocked_targets": blocked,
            "unverified_targets": unverified,
            "focused_target": focused_target,
        })
    }

    /// Reject a read action that resolved to a blank tab.
    ///
    /// Chrome is launched with an `about:blank` startup tab that never goes
    /// away, so a read can land on it and return a perfectly well-formed
    /// empty result. That reads to the model as "this page has nothing on
    /// it" and it stops using the browser, rather than as "you are not
    /// looking at the page you navigated".
    fn reject_blank_read(action: &str, url: &str, navigated: bool) -> Result<()> {
        if !Self::READ_ACTIONS.contains(&action) || !manager::is_blank_url(url) {
            return Ok(());
        }
        let detail = if navigated {
            "the tab you navigated is no longer available, so this resolved to a blank tab"
        } else {
            "no page has been navigated in this session yet"
        };
        Err(Error::ToolExecution(
            format!(
                "'{action}' resolved to a blank tab ('{url}') — {detail}. \
                 Call action 'navigate' with the URL you want to read, then retry."
            )
            .into(),
        ))
    }

    /// Rebuild a degraded browser connection and invalidate every capability
    /// minted by the old profile instance. The profile lease held by
    /// `execute` makes this atomic with respect to other browser tool calls.
    async fn recover_degraded_profile(&self, profile: &str) -> Value {
        self.snapshot_store.clear_profile(profile).await;
        match tokio::time::timeout(
            Duration::from_secs(25),
            self.manager.recover_profile(profile),
        )
        .await
        {
            Ok(Ok(value)) => value,
            Ok(Err(e)) => json!({ "status": "failed", "error": e.to_string() }),
            Err(_) => json!({ "status": "failed", "error": "browser recovery exceeded 25s" }),
        }
    }
}

impl Default for BrowserTool {
    fn default() -> Self {
        Self::new()
    }
}

/// Truncate a string to at most `max_bytes` bytes, respecting UTF-8 boundaries.
fn truncate_utf8(s: &str, max_bytes: usize) -> (&str, bool) {
    if s.len() <= max_bytes {
        return (s, false);
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    (&s[..end], true)
}

fn png_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    const PNG_SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";
    if bytes.len() < 24 || &bytes[..8] != PNG_SIGNATURE || &bytes[12..16] != b"IHDR" {
        return None;
    }
    let width = u32::from_be_bytes(bytes[16..20].try_into().ok()?);
    let height = u32::from_be_bytes(bytes[20..24].try_into().ok()?);
    (width > 0 && height > 0).then_some((width, height))
}

/// Mask a cookie value for security: hide the entire value to prevent
/// exposure of predictable session token prefixes.
fn mask_cookie_value(value: &str) -> String {
    if value.is_empty() {
        return String::new();
    }
    format!("***({} chars)", value.len())
}

#[async_trait]
impl Tool for BrowserTool {
    fn name(&self) -> &str {
        "browser"
    }

    fn description(&self) -> &str {
        "Browse and scrape the web. Three fetch modes plus interactive control of \
         Chrome via DevTools Protocol (CDP only; no Playwright). \
         Fetch modes: \
         fetch — pure HTTP request with browser-like header packs (impersonate=chrome|firefox|safari|edge), stealthy_headers, custom user-agent, proxy, retries, redirects; \
         stealth_fetch — full browser navigation with anti-bot patches (block_webrtc, hide_canvas, disable_resources), wait_selector, network_idle, solve_cloudflare, and returns rendered body; \
         select — CSS or XPath selector engine over either provided html or the active tab DOM, with Scrapling pseudo-selectors ::text and ::attr(name), find_by_text (regex or substring), and adaptive auto_save/auto_match across DOM changes. \
         Browser control: \
         status/start/stop — lifecycle; \
         profiles — list profiles; \
         downloads — list browser-observed download records and validated local paths; \
         tabs/open/close/focus — tab management; \
         navigate/back/forward/refresh — navigation and history (navigate supports wait_selector, wait_selector_state, network_idle, solve_cloudflare); \
         wait_for — wait for selector / network idle / fixed delay; \
         snapshot — DOM-derived interaction state with element refs, including site-isolated cross-origin frames; \
         act — interact by ref (click/type/fill/press/hover/select/drag/upload/options/wait); \
         click_coordinates — click native screenshot coordinates; send_keys — send trusted CDP keyboard input to the focused element; scroll_to_text — find and reveal visible text; \
         fill_credential — type a STORED credential into a field by ref, without \
         ever seeing its value: pass 'ref' and 'field' ('username' or 'password'). \
         Always sign in with this. Never use act/type for a password: you do not \
         have the value, and typing a placeholder or the credential's own name just \
         fails the login. If nothing is stored yet the error names the exact key — \
         ask for it with credential_request under that name, then retry; \
         screenshot/content/evaluate/scroll/console/cookies/pdf. Screenshots are delivered through the model's multimodal image channel. Use screenshot coordinates only when coordinate_actions_compatible=true; element and full-page screenshots are not viewport coordinate maps. \
         Snapshots report likely CAPTCHA providers. When captcha_monitor says model_solver_enabled=true, inspect a screenshot and mark only challenge-specific act/click_coordinates/send_keys calls with captchaAttempt=true. RustyKrab bounds and records those interactions; stop on cleared, budget_exhausted, not_detected, or unknown. After action_failed, inspect state and continue only with a materially corrected action while budget remains. This is visible interaction, not token injection or a bypass API. \
         Cookies persist across calls. Use snapshot + act for reliable element interaction. \
         Each act returns its outcome plus page_state; clicks also return the current tabs \
         and any JavaScript dialogs they accepted. Set expect_download=true on a download \
         click to wait for Chrome's completion event; a click alone is not download proof. \
         If outcome is \"unknown\", the browser \
         was recovered but the action may already have happened — inspect the new state \
         and never repeat the action blindly. \
         If act returns status \"new_snapshot\", the page state moved on: the response \
         contains the current snapshot under \"snapshot\" — pick a ref from it and call \
         act with that ref. The previous_ref in the response is no longer valid. \
         Use fetch when JS isn't required, stealth_fetch when it is."
    }

    fn sandbox_requirements(&self) -> SandboxRequirements {
        SandboxRequirements {
            needs_net: true,
            ..SandboxRequirements::default()
        }
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: self.name().to_string(),
            description: self.description().to_string(),
            parameters: schema_parameters(),
        }
    }

    async fn execute(&self, args: Value) -> Result<Value> {
        let action = args["action"].as_str().ok_or_else(|| {
            Error::ToolExecution(ToolError::invalid_input("missing 'action' parameter"))
        })?;

        Self::validate_action_args(action, &args)?;

        let action = effective_action(action, &args);

        if !self.manager.config().enabled {
            return Err(Error::ToolExecution(
                "browser subsystem is disabled. Set browser.enabled=true in config.".into(),
            ));
        }

        let profile = self.resolve_profile(&args).to_string();
        let session = Self::session_key();

        // The manager intentionally shares a long-lived authenticated Chrome
        // profile. Serialize its tool calls so navigation/recovery cannot race
        // another conversation that is using the same process.
        let _profile_lease = if action == "fetch" {
            None
        } else {
            Some(self.manager.acquire_profile_lease(&profile).await)
        };

        // Decide which page this call addresses once, here: an explicit
        // `targetId` wins, otherwise the tab this session last navigated,
        // provided it is still open. Every page action below reads
        // `target_id`, so this is the single place "which page?" is answered
        // — previously each one re-resolved it and could land on a different
        // tab than the call before.
        let resolved_target = self.resolve_target(action, &profile, &session, &args).await;
        let target_id = resolved_target.as_deref();

        // Register the raw-CDP endpoint used only for site-isolated iframe
        // targets. Keeping it with the snapshot state lets automatic
        // post-action snapshots use the bridge without coupling actions to the
        // browser manager.
        let needs_live_page = Self::PAGE_ACTIONS.contains(&action)
            && !(action == "select" && args["html"].as_str().is_some());
        if needs_live_page {
            let _ = self.manager.get_browser(&profile).await?;
            let websocket_url = self.manager.websocket_address(&profile).await?;
            self.snapshot_store
                .register_oopif_context(
                    &Self::store_key(&session, &profile, target_id),
                    websocket_url,
                    self.manager.config().ssrf_policy.clone(),
                )
                .await;
        }

        match action {
            // ── Lifecycle ──────────────────────────────────────────
            "status" => {
                let mut status = self.manager.status(&profile).await;
                let status_target = args["targetId"]
                    .as_str()
                    .map(ToOwned::to_owned)
                    .or_else(|| self.manager.sticky_target(&session, &profile));
                let key = Self::store_key(&session, &profile, status_target.as_deref());
                let config = self.manager.config();
                let captcha_status = self
                    .captcha_monitor
                    .status(
                        &key,
                        config.model_captcha_solver,
                        config.effective_captcha_max_attempts(),
                        config.effective_captcha_timeout(),
                    )
                    .await;
                if let Value::Object(object) = &mut status {
                    object.insert("captcha_monitor".into(), captcha_status);
                }
                Ok(status)
            }

            "start" => self.manager.start(&profile).await,

            "stop" => self.manager.stop(&profile).await,

            "profiles" => Ok(self.manager.profiles().await),

            "downloads" => {
                let _ = self.manager.get_browser(&profile).await?;
                self.manager.list_downloads(&profile).await
            }

            // ── Tab management ─────────────────────────────────────
            "tabs" => {
                // Auto-start if needed
                let _ = self.manager.get_browser(&profile).await?;
                self.manager.tabs(&profile).await
            }

            "open" => {
                let url = args["url"].as_str().ok_or_else(|| {
                    Error::ToolExecution("'open' requires 'url' parameter".into())
                })?;
                self.validate_requested_url(url).await?;
                let _ = self.manager.get_browser(&profile).await?;
                let opened = self.manager.open_tab(&profile, url).await?;
                if let Some(tid) = opened["targetId"].as_str() {
                    let page = self.manager.get_page(&profile, Some(tid)).await?;
                    let guard =
                        policy::enforce_page(&page, &self.manager.config().ssrf_policy).await;
                    if guard["status"] == "blocked" {
                        let closed = self.manager.close_tab(&profile, tid).await.is_ok();
                        return Ok(json!({
                            "status": "blocked",
                            "outcome": "not_applied",
                            "targetId": tid,
                            "navigation_guard": guard,
                            "closed": closed,
                            "retry_safe": false,
                            "profile": profile,
                        }));
                    }
                }
                // A freshly opened tab is where this session is now working.
                if let Some(tid) = opened["targetId"].as_str() {
                    self.manager.set_sticky_target(&session, &profile, tid);
                }
                Ok(opened)
            }

            "close" => {
                let tid = target_id.ok_or_else(|| {
                    Error::ToolExecution("'close' requires 'targetId' parameter".into())
                })?;
                let closed = self.manager.close_tab(&profile, tid).await?;
                if self.manager.sticky_target(&session, &profile).as_deref() == Some(tid) {
                    self.manager.clear_sticky_target(&session, &profile);
                }
                Ok(closed)
            }

            "focus" => {
                let tid = target_id.ok_or_else(|| {
                    Error::ToolExecution("'focus' requires 'targetId' parameter".into())
                })?;
                let focused = self.manager.focus_tab(&profile, tid).await?;
                // Focusing a tab is a statement about where to work next.
                self.manager.set_sticky_target(&session, &profile, tid);
                Ok(focused)
            }

            // ── Navigation ─────────────────────────────────────────
            "navigate" => {
                let url = args["url"].as_str().ok_or_else(|| {
                    Error::ToolExecution("'navigate' requires 'url' parameter".into())
                })?;
                self.validate_requested_url(url).await?;

                let _ = self.manager.get_browser(&profile).await?;
                let page = self.manager.get_page(&profile, target_id).await?;

                // Apply stealth before navigating so patches affect the new
                // document. Network-level overrides (UA, extra headers) are
                // a no-op when their args are absent.
                let stealth_opts = stealth::StealthOptions::from_args(&args);
                let _ = stealth::apply_network_overrides(&page, &stealth_opts).await;
                // Install DOM patches via evaluate_on_new_document so they
                // run before the target page's own JS — this is what hides
                // `navigator.webdriver` from frameworks that read it on load.
                let _ = stealth::install_stealth_on_new_document(&page, &stealth_opts).await;

                let timeout_ms = args["timeout_ms"].as_u64().unwrap_or(10_000);
                let deadline =
                    tokio::time::Instant::now() + Duration::from_millis(timeout_ms.max(1));
                let navigation = navigate_with_deadline(&page, url, deadline).await?;

                if navigation.browser_degraded {
                    let recovery = self.recover_degraded_profile(&profile).await;
                    return Ok(json!({
                        "status": navigation.status,
                        "outcome": navigation.outcome,
                        "readiness": navigation.readiness,
                        "reason": navigation.reason,
                        "elapsed_ms": navigation.elapsed_ms,
                        "browser_degraded": true,
                        "retry_safe": false,
                        "recovery": recovery,
                        "profile": profile
                    }));
                }
                if navigation.outcome == "not_applied" {
                    return Ok(json!({
                        "status": navigation.status,
                        "outcome": navigation.outcome,
                        "readiness": navigation.readiness,
                        "reason": navigation.reason,
                        "elapsed_ms": navigation.elapsed_ms,
                        "browser_degraded": false,
                        "retry_safe": true,
                        "profile": profile
                    }));
                }

                let mut navigation_guard =
                    policy::enforce_page(&page, &self.manager.config().ssrf_policy).await;
                if navigation_guard["status"] == "blocked" {
                    self.snapshot_store
                        .clear(&Self::store_key(&session, &profile, target_id))
                        .await;
                    return Ok(json!({
                        "status": "blocked",
                        "outcome": "not_applied",
                        "navigation_outcome": navigation.outcome,
                        "readiness": navigation.readiness,
                        "elapsed_ms": navigation.elapsed_ms,
                        "browser_degraded": false,
                        "retry_safe": false,
                        "navigation_guard": navigation_guard,
                        "profile": profile,
                    }));
                }

                // Apply DOM-level stealth patches (post-navigation).
                if remaining_millis(deadline) > 0 {
                    let _ = tokio::time::timeout_at(
                        deadline,
                        stealth::apply_stealth(&page, &stealth_opts),
                    )
                    .await;
                }

                let mut wait_results = serde_json::Map::new();
                if let Some(sel) = args["wait_selector"].as_str() {
                    let state = stealth::WaitState::parse(
                        args["wait_selector_state"].as_str().unwrap_or("visible"),
                    );
                    let ok = if remaining_millis(deadline) == 0 {
                        false
                    } else {
                        stealth::wait_for_selector(&page, sel, state, remaining_millis(deadline))
                            .await?
                    };
                    wait_results.insert("wait_selector".into(), Value::Bool(ok));
                }
                if args["network_idle"].as_bool().unwrap_or(false) {
                    let ok = if remaining_millis(deadline) == 0 {
                        false
                    } else {
                        stealth::wait_for_network_idle(&page, 500, remaining_millis(deadline))
                            .await?
                    };
                    wait_results.insert("network_idle".into(), Value::Bool(ok));
                }
                if args["solve_cloudflare"].as_bool().unwrap_or(false) {
                    let ok = if remaining_millis(deadline) == 0 {
                        false
                    } else {
                        stealth::solve_cloudflare(&page, remaining_millis(deadline)).await?
                    };
                    wait_results.insert("cloudflare_clear".into(), Value::Bool(ok));
                }
                if let Some(delay) = args["delay_ms"].as_u64() {
                    tokio::time::sleep(Duration::from_millis(
                        delay.min(remaining_millis(deadline)),
                    ))
                    .await;
                }

                let empty_page_recovery = recover_empty_page(&page).await;
                if empty_page_recovery["reloaded"].as_bool() == Some(true) {
                    self.snapshot_store
                        .clear(&Self::store_key(&session, &profile, target_id))
                        .await;
                    navigation_guard =
                        policy::enforce_page(&page, &self.manager.config().ssrf_policy).await;
                    if navigation_guard["status"] == "blocked" {
                        return Ok(json!({
                            "status": "blocked",
                            "outcome": "not_applied",
                            "navigation_outcome": navigation.outcome,
                            "readiness": navigation.readiness,
                            "elapsed_ms": navigation.elapsed_ms,
                            "browser_degraded": false,
                            "retry_safe": false,
                            "empty_page_recovery": empty_page_recovery,
                            "navigation_guard": navigation_guard,
                            "profile": profile,
                        }));
                    }
                }

                let title = manager::probe_page_title_once(&page)
                    .await
                    .unwrap_or_default();
                let current_url = manager::probe_page_url_once(&page)
                    .await
                    .unwrap_or_default();

                // Pin the tab we just loaded so the snapshot/content call
                // that follows reads this page and not some other one.
                let landed_on = page.target_id().inner().clone();
                self.manager
                    .set_sticky_target(&session, &profile, &landed_on);

                Ok(json!({
                    "title": title,
                    "url": current_url,
                    "status": navigation.status,
                    "outcome": navigation.outcome,
                    "readiness": navigation.readiness,
                    "reason": navigation.reason,
                    "elapsed_ms": navigation.elapsed_ms,
                    "browser_degraded": false,
                    "targetId": landed_on,
                    "waits": Value::Object(wait_results),
                    "waits_preceded_reload": empty_page_recovery["reloaded"].as_bool() == Some(true),
                    "empty_page_recovery": empty_page_recovery,
                    "navigation_guard": navigation_guard,
                    "profile": profile
                }))
            }

            "back" | "forward" => {
                let _ = self.manager.get_browser(&profile).await?;
                let page = self.page_for(action, &profile, &session, target_id).await?;
                let result = navigate_history_entry(
                    &page,
                    if action == "back" { -1 } else { 1 },
                    &self.manager.config().ssrf_policy,
                )
                .await?;
                self.snapshot_store
                    .clear(&Self::store_key(&session, &profile, target_id))
                    .await;
                Ok(result)
            }

            "refresh" => {
                let _ = self.manager.get_browser(&profile).await?;
                let page = self.page_for(action, &profile, &session, target_id).await?;
                tokio::time::timeout(Duration::from_secs(10), page.reload())
                    .await
                    .map_err(|_| Error::ToolExecution("page refresh timed out".into()))?
                    .map_err(|error| {
                        Error::ToolExecution(format!("page refresh failed: {error}").into())
                    })?;
                let guard = policy::enforce_page(&page, &self.manager.config().ssrf_policy).await;
                self.snapshot_store
                    .clear(&Self::store_key(&session, &profile, target_id))
                    .await;
                Ok(json!({
                    "status": if guard["status"] == "blocked" { "blocked" } else { "refreshed" },
                    "outcome": if guard["status"] == "blocked" { "not_applied" } else { "applied" },
                    "url": manager::probe_page_url_once(&page).await.unwrap_or_default(),
                    "navigation_guard": guard,
                    "retry_safe": false,
                    "profile": profile,
                }))
            }

            // ── Snapshot ───────────────────────────────────────────
            "snapshot" => {
                let _ = self.manager.get_browser(&profile).await?;
                let page = self.page_for(action, &profile, &session, target_id).await?;

                let mode = match args["format"].as_str() {
                    Some("aria") => SnapshotMode::Aria,
                    _ => SnapshotMode::Ai,
                };

                let options = SnapshotOptions {
                    mode,
                    interactive_only: args["interactive"].as_bool().unwrap_or(false),
                    compact: args["compact"].as_bool().unwrap_or(false),
                    max_depth: args["depth"].as_u64().unwrap_or(50) as usize,
                    selector: args["selector"].as_str().map(|s| s.to_string()),
                    highlight: args["highlight"].as_bool().unwrap_or(false),
                };

                let key = Self::store_key(&session, &profile, target_id);
                let mut snap =
                    snapshot::take_snapshot(&page, &options, &self.snapshot_store, &key).await?;
                let guard = self
                    .guard_page_output(action, &session, &profile, target_id, &page)
                    .await?;
                if let Value::Object(ref mut object) = snap {
                    object.insert("navigation_guard".into(), guard);
                }

                let captcha_monitor = self
                    .observe_captcha(
                        &key,
                        snap["url"].as_str().unwrap_or_default(),
                        &snap["captcha"],
                    )
                    .await;
                if let Value::Object(ref mut object) = snap {
                    object.insert("captcha_monitor".into(), captcha_monitor);
                }

                // How much of the page the agent actually received.
                //
                // When an agent reports only a page's heading, two very
                // different things could have happened: the snapshot held
                // one node because the document was still arriving, or it
                // held the whole page and the model quoted the first line.
                // Those need opposite fixes and the reply cannot tell them
                // apart. Sizes settle it; the page's text is deliberately
                // not logged, because a snapshot can contain anything the
                // page contains.
                let url = manager::probe_page_url_once(&page)
                    .await
                    .unwrap_or_default();
                tracing::debug!(
                    url = %url,
                    chars = snap.to_string().len(),
                    nodes = snap["elements"].as_array().map(|a| a.len()).unwrap_or(0),
                    "took page snapshot"
                );
                Ok(snap)
            }

            // ── Act (ref-based actions) ────────────────────────────
            "act" => {
                let ref_id = args["ref"].as_str().ok_or_else(|| {
                    Error::ToolExecution(
                        "'act' requires 'ref' parameter from a previous snapshot".into(),
                    )
                })?;
                let act_action = args["actAction"]
                    .as_str()
                    .ok_or_else(|| Error::ToolExecution(
                        "'act' requires 'actAction' parameter (click, type, fill, press, hover, select, drag, upload, options, wait, fill_credential)".into(),
                    ))?;

                let _ = self.manager.get_browser(&profile).await?;
                let mut action_args = args.clone();
                if act_action == "upload" {
                    if self.manager.config().is_remote_profile(&profile) {
                        return Err(Error::ToolExecution(ToolError::invalid_input(
                            "file upload to a remote CDP browser requires an artifact-transfer channel, which is not configured",
                        )));
                    }
                    let paths = Self::validated_upload_paths(&args)?;
                    action_args["paths"] = json!(paths);
                }
                let page = self.page_for(action, &profile, &session, target_id).await?;
                let key = Self::store_key(&session, &profile, target_id);
                let captcha_ticket = match self.begin_captcha_attempt(&args, &key, &page).await? {
                    Some(captcha::AttemptStart::Ready(ticket)) => Some(ticket),
                    Some(captcha::AttemptStart::Rejected(metadata)) => {
                        return Ok(Self::captcha_rejection(
                            &format!("act/{act_action}"),
                            metadata,
                        ));
                    }
                    None => None,
                };
                let targets_before =
                    tokio::time::timeout(Duration::from_secs(3), self.manager.target_ids(&profile))
                        .await
                        .ok()
                        .and_then(std::result::Result::ok);
                let download_since = if act_action == "click" {
                    self.manager.download_sequence(&profile).await
                } else {
                    None
                };

                let action_result = actions::execute_act(
                    &page,
                    &self.snapshot_store,
                    &key,
                    act_action,
                    ref_id,
                    &action_args,
                    actions::ActionPolicies {
                        dialog: self.manager.config().dialog_policy,
                        navigation: &self.manager.config().ssrf_policy,
                    },
                )
                .await;
                let mut outcome = match action_result {
                    Ok(outcome) => outcome,
                    Err(error) if captcha_ticket.is_some() => {
                        Self::captcha_action_error(&format!("act/{act_action}"), error)
                    }
                    Err(error) => return Err(error),
                };

                if let Some(since) = download_since {
                    let expect_download = args["expect_download"].as_bool().unwrap_or(false);
                    let action_degraded = outcome["browser_degraded"].as_bool().unwrap_or(false);
                    let wait = if expect_download && !action_degraded {
                        Duration::from_millis(
                            args["download_timeout_ms"]
                                .as_u64()
                                .unwrap_or(10_000)
                                .min(30_000),
                        )
                    } else {
                        Duration::ZERO
                    };
                    if let Some(observation) =
                        self.manager.observe_downloads(&profile, since, wait).await
                    {
                        if expect_download || observation["status"] != "not_observed" {
                            if let Value::Object(ref mut object) = outcome {
                                object.insert("download_observation".into(), observation);
                            }
                        }
                    } else if expect_download {
                        if let Value::Object(ref mut object) = outcome {
                            object.insert(
                                "download_observation".into(),
                                json!({
                                    "status": "unavailable",
                                    "downloads": [],
                                    "message": "The click outcome does not prove whether a download completed because browser-level download observation is unavailable."
                                }),
                            );
                        }
                    }
                }

                // A timed-out CDP stage is not repaired by repeating the same
                // action against the same session. Rebuild the driver/session
                // now, clear all refs for this tab, and report the recovery
                // without retrying the potentially-applied side effect.
                let action_degraded = outcome["browser_degraded"].as_bool().unwrap_or(false);
                if !action_degraded {
                    let popup_guard = self
                        .guard_new_tabs(&profile, &session, targets_before)
                        .await;
                    if let Value::Object(ref mut object) = outcome {
                        object.insert("popup_guard".into(), popup_guard);
                    }
                }

                if action_degraded {
                    let recovery_value = self.recover_degraded_profile(&profile).await;
                    if let Value::Object(ref mut object) = outcome {
                        object.insert("recovery".into(), recovery_value);
                    }
                }

                // A click may create a popup/new tab even when the originating
                // page does not navigate. Return the current target set with
                // the action so the agent can focus the new tab instead of
                // clicking the same control again.
                if act_action == "click" {
                    let tabs =
                        tokio::time::timeout(Duration::from_secs(5), self.manager.tabs(&profile))
                            .await
                            .ok()
                            .and_then(std::result::Result::ok)
                            .unwrap_or(Value::Null);
                    if let Value::Object(ref mut object) = outcome {
                        object.insert("tabs".into(), tabs);
                    }
                }
                self.finish_captcha_attempt(
                    captcha_ticket,
                    &format!("act/{act_action}"),
                    &page,
                    &mut outcome,
                )
                .await;
                Ok(outcome)
            }

            "click_coordinates" => {
                let x = args["x"].as_f64().expect("validated x coordinate");
                let y = args["y"].as_f64().expect("validated y coordinate");
                let _ = self.manager.get_browser(&profile).await?;
                let page = self.page_for(action, &profile, &session, target_id).await?;
                let key = Self::store_key(&session, &profile, target_id);
                let (x, y, coordinate_mapping) = self
                    .map_screenshot_coordinates(
                        &key,
                        &page,
                        x,
                        y,
                        args["captchaAttempt"].as_bool() == Some(true),
                    )
                    .await?;
                let captcha_ticket = match self.begin_captcha_attempt(&args, &key, &page).await? {
                    Some(captcha::AttemptStart::Ready(ticket)) => Some(ticket),
                    Some(captcha::AttemptStart::Rejected(metadata)) => {
                        return Ok(Self::captcha_rejection(action, metadata));
                    }
                    None => None,
                };
                let targets_before =
                    tokio::time::timeout(Duration::from_secs(3), self.manager.target_ids(&profile))
                        .await
                        .ok()
                        .and_then(std::result::Result::ok);
                let action_result = actions::execute_coordinate_click(
                    &page,
                    &self.snapshot_store,
                    &key,
                    x,
                    y,
                    self.manager.config().dialog_policy,
                    &self.manager.config().ssrf_policy,
                )
                .await;
                let mut outcome = match action_result {
                    Ok(outcome) => outcome,
                    Err(error) if captcha_ticket.is_some() => {
                        Self::captcha_action_error(action, error)
                    }
                    Err(error) => return Err(error),
                };
                if let Value::Object(object) = &mut outcome {
                    object.insert("coordinate_mapping".into(), coordinate_mapping);
                }
                let action_degraded = outcome["browser_degraded"].as_bool().unwrap_or(false);
                if !action_degraded {
                    let popup_guard = self
                        .guard_new_tabs(&profile, &session, targets_before)
                        .await;
                    if let Value::Object(ref mut object) = outcome {
                        object.insert("popup_guard".into(), popup_guard);
                    }
                } else {
                    let recovery = self.recover_degraded_profile(&profile).await;
                    if let Value::Object(ref mut object) = outcome {
                        object.insert("recovery".into(), recovery);
                    }
                }
                self.finish_captcha_attempt(captcha_ticket, action, &page, &mut outcome)
                    .await;
                Ok(outcome)
            }

            "send_keys" => {
                let keys = args["keys"].as_str().expect("validated keys");
                let _ = self.manager.get_browser(&profile).await?;
                let page = self.page_for(action, &profile, &session, target_id).await?;
                let store_key = Self::store_key(&session, &profile, target_id);
                let captcha_ticket =
                    match self.begin_captcha_attempt(&args, &store_key, &page).await? {
                        Some(captcha::AttemptStart::Ready(ticket)) => Some(ticket),
                        Some(captcha::AttemptStart::Rejected(metadata)) => {
                            return Ok(Self::captcha_rejection(action, metadata));
                        }
                        None => None,
                    };
                let targets_before =
                    tokio::time::timeout(Duration::from_secs(3), self.manager.target_ids(&profile))
                        .await
                        .ok()
                        .and_then(std::result::Result::ok);
                let action_result = actions::execute_send_keys(
                    &page,
                    &self.snapshot_store,
                    &store_key,
                    keys,
                    self.manager.config().dialog_policy,
                    &self.manager.config().ssrf_policy,
                )
                .await;
                let mut outcome = match action_result {
                    Ok(outcome) => outcome,
                    Err(error) if captcha_ticket.is_some() => {
                        Self::captcha_action_error(action, error)
                    }
                    Err(error) => return Err(error),
                };
                let action_degraded = outcome["browser_degraded"].as_bool().unwrap_or(false);
                if !action_degraded {
                    let popup_guard = self
                        .guard_new_tabs(&profile, &session, targets_before)
                        .await;
                    if let Value::Object(ref mut object) = outcome {
                        object.insert("popup_guard".into(), popup_guard);
                    }
                } else {
                    let recovery = self.recover_degraded_profile(&profile).await;
                    if let Value::Object(ref mut object) = outcome {
                        object.insert("recovery".into(), recovery);
                    }
                }
                self.finish_captcha_attempt(captcha_ticket, action, &page, &mut outcome)
                    .await;
                Ok(outcome)
            }
            // Type a stored credential into a field without the value
            // passing through the model.
            //
            // `act` with `fill` would work mechanically, but only by
            // putting the password in `text` — which means in the tool
            // call, in the conversation, and in every transcript. The
            // whole point of asking the user through a secure field is
            // that the value never reaches the context window, and a
            // fill action that undoes that on the way back in would make
            // the rest of this pointless.
            "fill_credential" => {
                let ref_id = args["ref"].as_str().ok_or_else(|| {
                    Error::ToolExecution(
                        "'fill_credential' requires 'ref' from a previous snapshot".into(),
                    )
                })?;
                let field = args["field"].as_str().unwrap_or(crate::PASSWORD);
                let secrets = self.secrets.as_ref().ok_or_else(|| {
                    Error::ToolExecution(
                        "credential fill is unavailable: this browser has no credential store"
                            .into(),
                    )
                })?;

                let _ = self.manager.get_browser(&profile).await?;
                let page = self.page_for(action, &profile, &session, target_id).await?;

                // The page's own URL, so the key matches wherever the
                // agent actually is rather than where it meant to be.
                let url = match args["url"].as_str() {
                    Some(u) => u.to_string(),
                    None => manager::probe_page_url_once(&page)
                        .await
                        .unwrap_or_default(),
                };
                let cred_key = crate::origin_credential_key(&url, field)?;

                let value = secrets.get(&cred_key).await.map_err(|_| {
                    // Names the key so the agent can ask for exactly it.
                    Error::ToolExecution(
                        format!(
                            "no credential stored under '{cred_key}'. Ask the user for it with \
                             credential_request using that exact name, then try again."
                        )
                        .into(),
                    )
                })?;

                // What is about to be typed, without saying it.
                //
                // `status: "filled"` was returned whether or not the right
                // string reached the field, so a wrong fill and a right
                // one were indistinguishable from outside -- the same
                // shape of defect as a read that returns "" and calls it
                // success. Length plus a short digest is enough to tell
                // "the value arrived" from "something else did" when
                // reading a failed run, and neither reveals the secret.
                let digest = {
                    use sha2::{Digest, Sha256};
                    let mut h = Sha256::new();
                    h.update(value.as_bytes());
                    hex::encode(&h.finalize()[..4])
                };
                tracing::debug!(
                    key = %cred_key,
                    field,
                    value_len = value.len(),
                    value_sha256_prefix = %digest,
                    "filling a credential field"
                );

                let store_key = Self::store_key(&session, &profile, target_id);
                let fill_args = json!({ "text": value, "clear": true });
                let fill_outcome = actions::execute_act(
                    &page,
                    &self.snapshot_store,
                    &store_key,
                    "fill",
                    ref_id,
                    &fill_args,
                    actions::ActionPolicies {
                        dialog: self.manager.config().dialog_policy,
                        navigation: &self.manager.config().ssrf_policy,
                    },
                )
                .await?;

                if fill_outcome["outcome"] != "applied" {
                    let recovery = if fill_outcome["browser_degraded"].as_bool().unwrap_or(false) {
                        self.recover_degraded_profile(&profile).await
                    } else {
                        Value::Null
                    };
                    return Ok(json!({
                        "status": fill_outcome["status"],
                        "outcome": fill_outcome["outcome"],
                        "field": field,
                        "credentialKey": cred_key,
                        "ref": ref_id,
                        "reason": fill_outcome["reason"],
                        "stage": fill_outcome["stage"],
                        "retry_safe": fill_outcome["retry_safe"],
                        "browser_degraded": fill_outcome["browser_degraded"],
                        "page_state": fill_outcome["page_state"],
                        "snapshot": fill_outcome["snapshot"],
                        "message": fill_outcome["message"],
                        "recovery": recovery,
                    }));
                }

                // A fresh result rather than the fill's own. Nothing
                // derived from the value travels back to the model — not
                // its length, which for a password is worth guessing with.
                // The length is reported so a caller can tell an empty
                // or placeholder fill from a real one. The value is not.
                Ok(json!({
                    "status": "filled",
                    "outcome": "applied",
                    "field": field,
                    "credentialKey": cred_key,
                    "ref": ref_id,
                    "value_len": value.len(),
                    "page_state": fill_outcome["page_state"],
                }))
            }

            // ── Screenshot ─────────────────────────────────────────
            "screenshot" => {
                let _ = self.manager.get_browser(&profile).await?;
                let page = self.page_for(action, &profile, &session, target_id).await?;
                let full_page = args["full_page"].as_bool().unwrap_or(false);
                let selector = args["selector"].as_str();

                let png_bytes = if let Some(sel) = selector {
                    // Same bound the `act` path got: an element lookup
                    // against a wedged document never returns on its own.
                    let elem = actions::find_element_bounded(&page, sel).await?;
                    elem.screenshot(
                        chromiumoxide::cdp::browser_protocol::page::CaptureScreenshotFormat::Png,
                    )
                    .await
                    .map_err(|e| {
                        Error::ToolExecution(format!("element screenshot failed: {e}").into())
                    })?
                } else {
                    let params = ScreenshotParams::builder().full_page(full_page).build();
                    page.screenshot(params).await.map_err(|e| {
                        Error::ToolExecution(format!("screenshot failed: {e}").into())
                    })?
                };

                let size_bytes = png_bytes.len();
                let dimensions = png_dimensions(&png_bytes);
                let b64 = base64::engine::general_purpose::STANDARD.encode(&png_bytes);
                let navigation_guard = self
                    .guard_page_output(action, &session, &profile, target_id, &page)
                    .await?;
                let page_url = manager::probe_page_url_once(&page)
                    .await
                    .unwrap_or_default();
                let captcha_state = snapshot::detect_captcha(&page).await;
                let key = Self::store_key(&session, &profile, target_id);
                let captcha_monitor = self.observe_captcha(&key, &page_url, &captcha_state).await;
                let viewport = tokio::time::timeout(Duration::from_secs(2), page.layout_metrics())
                    .await
                    .ok()
                    .and_then(std::result::Result::ok)
                    .map(|metrics| {
                        (
                            metrics.css_visual_viewport.client_width,
                            metrics.css_visual_viewport.client_height,
                        )
                    });
                let coordinate_actions_compatible =
                    selector.is_none() && !full_page && dimensions.is_some() && viewport.is_some();
                if coordinate_actions_compatible {
                    let (image_width, image_height) = dimensions.expect("checked above");
                    let (viewport_width, viewport_height) = viewport.expect("checked above");
                    self.screenshot_geometry.lock().await.insert(
                        key,
                        ScreenshotGeometry {
                            image_width: f64::from(image_width),
                            image_height: f64::from(image_height),
                            viewport_width,
                            viewport_height,
                            page_url: page_url.clone(),
                            captured_at: Instant::now(),
                        },
                    );
                } else {
                    self.screenshot_geometry.lock().await.remove(&key);
                }

                let mut output = json!({
                    "screenshot": "attached_to_multimodal_result",
                    "size_bytes": size_bytes,
                    "image_width": dimensions.map(|(width, _)| width),
                    "image_height": dimensions.map(|(_, height)| height),
                    "viewport_width": viewport.map(|(width, _)| width),
                    "viewport_height": viewport.map(|(_, height)| height),
                    "coordinate_space": if selector.is_some() {
                        "element_pixels_not_viewport"
                    } else if full_page {
                        "full_page_pixels_not_current_viewport"
                    } else {
                        "viewport_screenshot_pixels"
                    },
                    "coordinate_actions_compatible": coordinate_actions_compatible,
                    "format": "png",
                    "profile": profile,
                    "navigation_guard": navigation_guard,
                    "captcha": captcha_state,
                    "captcha_monitor": captcha_monitor,
                    "_images": [{
                        "media_type": "image/png",
                        "data": b64.clone(),
                    }],
                });
                if args["includeBase64"].as_bool() == Some(true) {
                    output["screenshot"] = Value::String(b64);
                    output["encoding"] = Value::String("base64".to_string());
                }
                Ok(output)
            }

            // ── Content ────────────────────────────────────────────
            "content" => {
                let _ = self.manager.get_browser(&profile).await?;
                let page = self.page_for(action, &profile, &session, target_id).await?;
                let format = args["format"].as_str().unwrap_or("text");

                let content = match format {
                    "html" => page.content().await.map_err(|e| {
                        Error::ToolExecution(format!("failed to get page HTML: {e}").into())
                    })?,
                    _ => {
                        let result =
                            page.evaluate("document.body.innerText")
                                .await
                                .map_err(|e| {
                                    Error::ToolExecution(
                                        format!("failed to get page text: {e}").into(),
                                    )
                                })?;
                        result.into_value::<String>().unwrap_or_default()
                    }
                };

                let (truncated_content, was_truncated) = truncate_utf8(&content, MAX_CONTENT_BYTES);
                let title = manager::probe_page_title_once(&page)
                    .await
                    .unwrap_or_default();
                let current_url = manager::probe_page_url_once(&page)
                    .await
                    .unwrap_or_default();
                let navigation_guard = self
                    .guard_page_output(action, &session, &profile, target_id, &page)
                    .await?;

                Ok(json!({
                    "content": truncated_content,
                    "url": current_url,
                    "title": title,
                    "format": format,
                    "truncated": was_truncated,
                    "profile": profile,
                    "navigation_guard": navigation_guard,
                }))
            }

            // ── Evaluate ───────────────────────────────────────────
            "evaluate" => {
                if !self.manager.config().evaluate_enabled {
                    return Err(Error::ToolExecution(
                        "JavaScript evaluation is disabled. Set browser.evaluateEnabled=true in config."
                            .into(),
                    ));
                }

                let expression = args["expression"].as_str().ok_or_else(|| {
                    Error::ToolExecution("'evaluate' requires 'expression' parameter".into())
                })?;

                // Limit expression length to prevent abuse
                const MAX_EXPRESSION_LEN: usize = 10_000;
                if expression.len() > MAX_EXPRESSION_LEN {
                    return Err(Error::ToolExecution(
                        format!(
                            "expression too long ({} bytes, max {MAX_EXPRESSION_LEN})",
                            expression.len()
                        )
                        .into(),
                    ));
                }

                // Block access to sensitive browser APIs that could exfiltrate
                // credentials or session data
                let expr_lower = expression.to_lowercase();
                let blocked_patterns = [
                    "document.cookie",
                    "localstorage",
                    "sessionstorage",
                    "indexeddb",
                    "navigator.credentials",
                    "serviceworker",
                    "importscripts",
                ];
                for pattern in &blocked_patterns {
                    if expr_lower.contains(pattern) {
                        return Err(Error::ToolExecution(
                            format!(
                                "access to '{pattern}' is blocked in evaluate for security. \
                                 Use dedicated browser actions instead."
                            )
                            .into(),
                        ));
                    }
                }

                let _ = self.manager.get_browser(&profile).await?;
                let page = self.page_for(action, &profile, &session, target_id).await?;
                let result = page.evaluate(expression).await.map_err(|e| {
                    Error::ToolExecution(format!("JS evaluation failed: {e}").into())
                })?;

                let value: Value = result.into_value().unwrap_or(Value::Null);
                let navigation_guard = self
                    .guard_page_output(action, &session, &profile, target_id, &page)
                    .await?;

                Ok(json!({
                    "result": value,
                    "profile": profile,
                    "navigation_guard": navigation_guard,
                }))
            }

            // ── Scroll ─────────────────────────────────────────────
            "scroll" => {
                let direction = args["direction"].as_str().unwrap_or("down");
                let amount = args["amount"].as_i64().unwrap_or(500);

                let _ = self.manager.get_browser(&profile).await?;
                let page = self.page_for(action, &profile, &session, target_id).await?;

                let js = match direction {
                    "down" => format!("window.scrollBy(0, {amount}); window.scrollY"),
                    "up" => format!("window.scrollBy(0, -{amount}); window.scrollY"),
                    "bottom" => {
                        "window.scrollTo(0, document.body.scrollHeight); window.scrollY".to_string()
                    }
                    "top" => "window.scrollTo(0, 0); window.scrollY".to_string(),
                    _ => {
                        return Err(Error::ToolExecution(
                            format!(
                                "unknown scroll direction: '{direction}'. Use: down, up, bottom, top"
                            )
                            .into(),
                        ));
                    }
                };

                let result = page
                    .evaluate(js)
                    .await
                    .map_err(|e| Error::ToolExecution(format!("scroll failed: {e}").into()))?;

                let scroll_y: f64 = result.into_value().unwrap_or(0.0);
                let navigation_guard = self
                    .guard_page_output(action, &session, &profile, target_id, &page)
                    .await?;

                Ok(json!({
                    "status": "scrolled",
                    "direction": direction,
                    "scroll_y": scroll_y as i64,
                    "profile": profile,
                    "navigation_guard": navigation_guard,
                }))
            }

            "scroll_to_text" => {
                let text = args["text"].as_str().expect("validated scroll text");
                let _ = self.manager.get_browser(&profile).await?;
                let page = self.page_for(action, &profile, &session, target_id).await?;
                let needle = serde_json::to_string(text).map_err(|error| {
                    Error::ToolExecution(format!("invalid scroll text: {error}").into())
                })?;
                let script = format!(
                    r#"(function() {{
                        var needle = String({needle}).toLocaleLowerCase();
                        var roots = [document];
                        var seen = new Set();
                        while (roots.length) {{
                            var root = roots.shift();
                            if (!root || seen.has(root)) continue;
                            seen.add(root);
                            var walker = document.createTreeWalker(root, NodeFilter.SHOW_TEXT);
                            var node;
                            while ((node = walker.nextNode())) {{
                                var value = (node.nodeValue || '').trim();
                                if (value && value.toLocaleLowerCase().includes(needle)) {{
                                    var element = node.parentElement;
                                    if (!element) continue;
                                    element.scrollIntoView({{block:'center', inline:'nearest', behavior:'instant'}});
                                    var rect = element.getBoundingClientRect();
                                    return {{found:true, text:value.substring(0,200), bounds:[rect.x,rect.y,rect.width,rect.height]}};
                                }}
                            }}
                            var elements = root.querySelectorAll ? root.querySelectorAll('*') : [];
                            for (var i = 0; i < elements.length; i++) {{
                                if (elements[i].shadowRoot) roots.push(elements[i].shadowRoot);
                            }}
                        }}
                        return {{found:false}};
                    }})()"#
                );
                let result = tokio::time::timeout(Duration::from_secs(5), page.evaluate(script))
                    .await
                    .map_err(|_| Error::ToolExecution("scroll-to-text timed out".into()))?
                    .map_err(|error| {
                        Error::ToolExecution(format!("scroll-to-text failed: {error}").into())
                    })?
                    .into_value::<Value>()
                    .unwrap_or_else(|_| json!({ "found": false }));
                let navigation_guard = self
                    .guard_page_output(action, &session, &profile, target_id, &page)
                    .await?;
                Ok(json!({
                    "status": if result["found"] == true { "scrolled" } else { "not_found" },
                    "outcome": if result["found"] == true { "applied" } else { "not_applied" },
                    "query": text,
                    "match": result,
                    "retry_safe": result["found"] != true,
                    "profile": profile,
                    "navigation_guard": navigation_guard,
                }))
            }

            // ── Console ────────────────────────────────────────────
            "console" => {
                let _ = self.manager.get_browser(&profile).await?;
                let page = self.page_for(action, &profile, &session, target_id).await?;

                // Retrieve console messages via JS — collects last N entries
                let js = r#"
                    (function() {
                        if (!window.__rustykrab_console) return JSON.stringify([]);
                        return JSON.stringify(window.__rustykrab_console.slice(-50));
                    })()
                "#;

                // First, inject the console interceptor if not already present
                let inject_js = r#"
                    if (!window.__rustykrab_console) {
                        window.__rustykrab_console = [];
                        var origLog = console.log;
                        var origWarn = console.warn;
                        var origError = console.error;
                        console.log = function() {
                            window.__rustykrab_console.push({level: 'log', text: Array.from(arguments).join(' '), ts: Date.now()});
                            origLog.apply(console, arguments);
                        };
                        console.warn = function() {
                            window.__rustykrab_console.push({level: 'warn', text: Array.from(arguments).join(' '), ts: Date.now()});
                            origWarn.apply(console, arguments);
                        };
                        console.error = function() {
                            window.__rustykrab_console.push({level: 'error', text: Array.from(arguments).join(' '), ts: Date.now()});
                            origError.apply(console, arguments);
                        };
                        'installed'
                    } else {
                        'already_installed'
                    }
                "#;

                let _ = page.evaluate(inject_js).await;
                let result = page.evaluate(js).await.map_err(|e| {
                    Error::ToolExecution(format!("failed to get console logs: {e}").into())
                })?;

                let raw: String = result.into_value().unwrap_or_else(|_| "[]".to_string());
                let entries: Value = serde_json::from_str(&raw).unwrap_or(json!([]));
                let navigation_guard = self
                    .guard_page_output(action, &session, &profile, target_id, &page)
                    .await?;

                Ok(json!({
                    "console": entries,
                    "note": "Console interception is installed on first call. Earlier messages are not captured.",
                    "profile": profile,
                    "navigation_guard": navigation_guard,
                }))
            }

            // ── Cookies ────────────────────────────────────────────
            "cookies" => {
                let _ = self.manager.get_browser(&profile).await?;
                let page = self.page_for(action, &profile, &session, target_id).await?;
                let domain_filter = args["domain"].as_str();

                let cookies: Vec<Cookie> = page.get_cookies().await.map_err(|e| {
                    Error::ToolExecution(format!("failed to get cookies: {e}").into())
                })?;

                let filtered: Vec<Value> = cookies
                    .iter()
                    .filter(|c| {
                        if let Some(domain) = domain_filter {
                            c.domain.contains(domain)
                        } else {
                            true
                        }
                    })
                    .map(|c| {
                        json!({
                            "name": c.name,
                            "value": mask_cookie_value(&c.value),
                            "domain": c.domain,
                            "path": c.path,
                        })
                    })
                    .collect();
                let navigation_guard = self
                    .guard_page_output(action, &session, &profile, target_id, &page)
                    .await?;

                Ok(json!({
                    "cookies": filtered,
                    "count": filtered.len(),
                    "profile": profile,
                    "navigation_guard": navigation_guard,
                }))
            }

            // ── PDF ────────────────────────────────────────────────
            "pdf" => {
                let _ = self.manager.get_browser(&profile).await?;
                let page = self.page_for(action, &profile, &session, target_id).await?;

                let pdf_bytes = page.pdf(Default::default()).await.map_err(|e| {
                    Error::ToolExecution(
                        format!("PDF generation failed: {e}. Note: PDF requires headless mode.")
                            .into(),
                    )
                })?;

                let size_bytes = pdf_bytes.len();
                let b64 = base64::engine::general_purpose::STANDARD.encode(&pdf_bytes);

                let url = manager::probe_page_url_once(&page)
                    .await
                    .unwrap_or_default();
                let title = manager::probe_page_title_once(&page)
                    .await
                    .unwrap_or_default();
                let navigation_guard = self
                    .guard_page_output(action, &session, &profile, target_id, &page)
                    .await?;

                Ok(json!({
                    "pdf": b64,
                    "size_bytes": size_bytes,
                    "encoding": "base64",
                    "url": url,
                    "title": title,
                    "profile": profile,
                    "navigation_guard": navigation_guard,
                }))
            }

            // ── Scrapling.Fetcher ──────────────────────────────────
            "fetch" => {
                let params = fetcher::FetchParams::from_args(&args)?;
                fetcher::execute_fetch(params).await
            }

            // ── Scrapling.StealthyFetcher (single-call) ────────────
            "stealth_fetch" => {
                let url = args["url"].as_str().ok_or_else(|| {
                    Error::ToolExecution("'stealth_fetch' requires 'url' parameter".into())
                })?;
                self.validate_requested_url(url).await?;

                let _ = self.manager.get_browser(&profile).await?;
                let page = self.manager.get_page(&profile, target_id).await?;

                let stealth_opts = stealth::StealthOptions::from_args(&args);
                let _ = stealth::apply_network_overrides(&page, &stealth_opts).await;
                let _ = stealth::install_stealth_on_new_document(&page, &stealth_opts).await;

                let timeout_ms = args["timeout_ms"].as_u64().unwrap_or(30_000);
                let deadline =
                    tokio::time::Instant::now() + Duration::from_millis(timeout_ms.max(1));
                let navigation = navigate_with_deadline(&page, url, deadline).await?;

                if navigation.browser_degraded {
                    let recovery = self.recover_degraded_profile(&profile).await;
                    return Ok(json!({
                        "status": navigation.status,
                        "outcome": navigation.outcome,
                        "readiness": navigation.readiness,
                        "reason": navigation.reason,
                        "elapsed_ms": navigation.elapsed_ms,
                        "browser_degraded": true,
                        "retry_safe": false,
                        "recovery": recovery,
                        "profile": profile
                    }));
                }
                if navigation.outcome == "not_applied" {
                    return Ok(json!({
                        "status": navigation.status,
                        "outcome": navigation.outcome,
                        "readiness": navigation.readiness,
                        "reason": navigation.reason,
                        "elapsed_ms": navigation.elapsed_ms,
                        "browser_degraded": false,
                        "retry_safe": true,
                        "profile": profile
                    }));
                }

                let mut navigation_guard =
                    policy::enforce_page(&page, &self.manager.config().ssrf_policy).await;
                if navigation_guard["status"] == "blocked" {
                    self.snapshot_store
                        .clear(&Self::store_key(&session, &profile, target_id))
                        .await;
                    return Ok(json!({
                        "status": "blocked",
                        "outcome": "not_applied",
                        "navigation_outcome": navigation.outcome,
                        "readiness": navigation.readiness,
                        "elapsed_ms": navigation.elapsed_ms,
                        "browser_degraded": false,
                        "retry_safe": false,
                        "navigation_guard": navigation_guard,
                        "profile": profile,
                    }));
                }

                if remaining_millis(deadline) > 0 {
                    let _ = tokio::time::timeout_at(
                        deadline,
                        stealth::apply_stealth(&page, &stealth_opts),
                    )
                    .await;
                }

                let mut wait_results = serde_json::Map::new();
                if let Some(sel) = args["wait_selector"].as_str() {
                    let state = stealth::WaitState::parse(
                        args["wait_selector_state"].as_str().unwrap_or("visible"),
                    );
                    let ok = if remaining_millis(deadline) == 0 {
                        false
                    } else {
                        stealth::wait_for_selector(&page, sel, state, remaining_millis(deadline))
                            .await?
                    };
                    wait_results.insert("wait_selector".into(), Value::Bool(ok));
                }
                if args["network_idle"].as_bool().unwrap_or(true) {
                    let ok = if remaining_millis(deadline) == 0 {
                        false
                    } else {
                        stealth::wait_for_network_idle(&page, 500, remaining_millis(deadline))
                            .await?
                    };
                    wait_results.insert("network_idle".into(), Value::Bool(ok));
                }
                if args["solve_cloudflare"].as_bool().unwrap_or(false) {
                    let ok = if remaining_millis(deadline) == 0 {
                        false
                    } else {
                        stealth::solve_cloudflare(&page, remaining_millis(deadline)).await?
                    };
                    wait_results.insert("cloudflare_clear".into(), Value::Bool(ok));
                }
                if let Some(delay) = args["delay_ms"].as_u64() {
                    tokio::time::sleep(Duration::from_millis(
                        delay.min(remaining_millis(deadline)),
                    ))
                    .await;
                }

                let empty_page_recovery = recover_empty_page(&page).await;
                if empty_page_recovery["reloaded"].as_bool() == Some(true) {
                    self.snapshot_store
                        .clear(&Self::store_key(&session, &profile, target_id))
                        .await;
                    navigation_guard =
                        policy::enforce_page(&page, &self.manager.config().ssrf_policy).await;
                    if navigation_guard["status"] == "blocked" {
                        return Ok(json!({
                            "status": "blocked",
                            "outcome": "not_applied",
                            "navigation_outcome": navigation.outcome,
                            "readiness": navigation.readiness,
                            "elapsed_ms": navigation.elapsed_ms,
                            "browser_degraded": false,
                            "retry_safe": false,
                            "empty_page_recovery": empty_page_recovery,
                            "navigation_guard": navigation_guard,
                            "profile": profile,
                        }));
                    }
                }

                let final_url = manager::probe_page_url_once(&page)
                    .await
                    .unwrap_or_default();
                let title = manager::probe_page_title_once(&page)
                    .await
                    .unwrap_or_default();
                let html = page.content().await.unwrap_or_default();
                let text = page
                    .evaluate("document.body ? document.body.innerText : ''")
                    .await
                    .ok()
                    .and_then(|r| r.into_value::<String>().ok())
                    .unwrap_or_default();

                let cookies: Vec<Cookie> = page.get_cookies().await.unwrap_or_default();
                let cookie_map: std::collections::HashMap<String, String> = cookies
                    .iter()
                    .map(|c| (c.name.clone(), mask_cookie_value(&c.value)))
                    .collect();

                let (truncated_text, text_truncated) = truncate_utf8(&text, MAX_CONTENT_BYTES);
                let (truncated_html, html_truncated) = truncate_utf8(&html, MAX_CONTENT_BYTES * 4);

                Ok(json!({
                    "url": final_url,
                    "title": title,
                    "status": 200,
                    "ok": true,
                    "outcome": navigation.outcome,
                    "navigation_status": navigation.status,
                    "readiness": navigation.readiness,
                    "navigation_reason": navigation.reason,
                    "elapsed_ms": navigation.elapsed_ms,
                    "text": truncated_text,
                    "text_truncated": text_truncated,
                    "body": truncated_html,
                    "body_truncated": html_truncated,
                    "cookies": cookie_map,
                    "waits": Value::Object(wait_results),
                    "waits_preceded_reload": empty_page_recovery["reloaded"].as_bool() == Some(true),
                    "empty_page_recovery": empty_page_recovery,
                    "navigation_guard": navigation_guard,
                    "profile": profile,
                }))
            }

            // ── Scrapling.Selector ─────────────────────────────────
            "select" => {
                let params = selectors::SelectParams::from_args(&args);
                let live_page = if params.html.is_none() {
                    let _ = self.manager.get_browser(&profile).await?;
                    Some(self.page_for(action, &profile, &session, target_id).await?)
                } else {
                    None
                };

                let mut matches = if let Some(html) = &params.html {
                    selectors::select_static(html, &params)?
                } else {
                    let page = live_page.as_ref().expect("live page was resolved");
                    selectors::select_live(page, &params).await?
                };

                let mut adaptive_used = false;
                if matches.is_empty() && params.auto_match {
                    let id = params.auto_save_id.as_deref().unwrap_or_default();
                    if !id.is_empty() {
                        // Pull all elements once to build a candidate pool.
                        let pool_params = selectors::SelectParams {
                            css: Some("*".to_string()),
                            ..Default::default()
                        };
                        let candidates = if let Some(html) = &params.html {
                            selectors::select_static(html, &pool_params).unwrap_or_default()
                        } else {
                            let page = live_page.as_ref().expect("live page was resolved");
                            selectors::select_live(page, &pool_params)
                                .await
                                .unwrap_or_default()
                        };
                        let threshold = args["auto_match_threshold"].as_f64().unwrap_or(0.6);
                        let scored = self
                            .adaptive_store
                            .match_against(id, &candidates, threshold)
                            .await;
                        matches = scored.into_iter().map(|(m, _)| m).collect();
                        adaptive_used = !matches.is_empty();
                    }
                }

                if params.auto_save {
                    if let Some(id) = params.auto_save_id.as_deref() {
                        if !id.is_empty() {
                            self.adaptive_store.save(id, &matches).await;
                        }
                    }
                }

                let mut value = selectors::matches_to_json(&matches);
                if let Value::Object(ref mut o) = value {
                    o.insert("adaptive_used".into(), Value::Bool(adaptive_used));
                    if let Some(page) = &live_page {
                        let guard = self
                            .guard_page_output(action, &session, &profile, target_id, page)
                            .await?;
                        o.insert("navigation_guard".into(), guard);
                    }
                }
                Ok(value)
            }

            // ── Wait helper ────────────────────────────────────────
            "wait_for" => {
                let _ = self.manager.get_browser(&profile).await?;
                let page = self.page_for(action, &profile, &session, target_id).await?;
                let timeout_ms = args["timeout_ms"].as_u64().unwrap_or(10_000);

                let mut results = serde_json::Map::new();
                let mut did_any = false;

                if let Some(sel) = args["wait_selector"].as_str() {
                    let state = stealth::WaitState::parse(
                        args["wait_selector_state"].as_str().unwrap_or("visible"),
                    );
                    let ok = stealth::wait_for_selector(&page, sel, state, timeout_ms).await?;
                    results.insert("wait_selector".into(), Value::Bool(ok));
                    did_any = true;
                }
                if args["network_idle"].as_bool().unwrap_or(false) {
                    let ok = stealth::wait_for_network_idle(&page, 500, timeout_ms).await?;
                    results.insert("network_idle".into(), Value::Bool(ok));
                    did_any = true;
                }
                if args["solve_cloudflare"].as_bool().unwrap_or(false) {
                    let ok = stealth::solve_cloudflare(&page, timeout_ms).await?;
                    results.insert("cloudflare_clear".into(), Value::Bool(ok));
                    did_any = true;
                }
                if let Some(delay) = args["delay_ms"].as_u64() {
                    tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                    results.insert("delay_ms".into(), Value::Number(delay.into()));
                    did_any = true;
                }

                if !did_any {
                    return Err(Error::ToolExecution(
                        "'wait_for' requires at least one of: wait_selector, network_idle, solve_cloudflare, delay_ms"
                            .into(),
                    ));
                }

                let guard = self
                    .guard_page_output(action, &session, &profile, target_id, &page)
                    .await?;
                results.insert("navigation_guard".into(), guard);
                Ok(Value::Object(results))
            }

            _ => Err(Error::ToolExecution(
                format!(
                    "unknown browser action: '{action}'. Available: \
                     status, start, stop, profiles, tabs, open, close, focus, \
                     navigate, back, forward, refresh, snapshot, act, click_coordinates, \
                     send_keys, screenshot, content, evaluate, scroll, scroll_to_text, \
                     console, cookies, pdf, fetch, stealth_fetch, select, wait_for"
                )
                .into(),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn png_dimensions_reads_the_ihdr_without_decoding_the_image() {
        let mut header = b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR".to_vec();
        header.extend_from_slice(&640u32.to_be_bytes());
        header.extend_from_slice(&400u32.to_be_bytes());
        assert_eq!(png_dimensions(&header), Some((640, 400)));
        assert_eq!(png_dimensions(b"not a png"), None);
    }

    #[test]
    fn screenshot_coordinates_scale_back_to_the_css_viewport() {
        let geometry = ScreenshotGeometry {
            image_width: 1280.0,
            image_height: 720.0,
            viewport_width: 640.0,
            viewport_height: 360.0,
            page_url: "https://example.com".into(),
            captured_at: Instant::now(),
        };
        assert_eq!(
            scale_screenshot_point(320.0, 180.0, &geometry).expect("point in bounds"),
            (160.0, 90.0)
        );
        assert!(scale_screenshot_point(1281.0, 1.0, &geometry).is_err());
    }

    #[test]
    fn schema_enforces_action_specific_arguments() {
        let parameters = schema_parameters();

        let act_err = rustykrab_core::validate_tool_args(
            &parameters,
            &json!({ "action": "act", "ref": "e12" }),
        )
        .expect_err("act without actAction must fail schema validation");
        assert_eq!(act_err.kind, rustykrab_core::ToolErrorKind::InvalidInput);
        assert!(act_err.message.contains("'actAction'"), "{act_err}");
        assert!(act_err.message.contains("'click'"), "{act_err}");

        let evaluate_err =
            rustykrab_core::validate_tool_args(&parameters, &json!({ "action": "evaluate" }))
                .expect_err("evaluate without expression must fail schema validation");
        assert!(
            evaluate_err.message.contains("'expression'"),
            "{evaluate_err}"
        );

        let type_err = rustykrab_core::validate_tool_args(
            &parameters,
            &json!({ "action": "act", "ref": "e12", "actAction": "type" }),
        )
        .expect_err("act/type without text must fail schema validation");
        assert!(type_err.message.contains("'text'"), "{type_err}");

        rustykrab_core::validate_tool_args(
            &parameters,
            &json!({ "action": "act", "ref": "e12", "actAction": "fill_credential" }),
        )
        .expect("fill_credential must not require text");
        rustykrab_core::validate_tool_args(&parameters, &json!({ "action": "snapshot" }))
            .expect("actions without conditional arguments must remain valid");
    }

    #[test]
    fn schema_describes_companion_arguments_for_the_model() {
        let parameters = schema_parameters();
        let act_description = parameters["properties"]["actAction"]["description"]
            .as_str()
            .unwrap();
        assert!(act_description.contains("type/fill -> text"));
        assert!(act_description.contains("press -> key"));
        assert!(act_description.contains("drag -> targetRef"));

        let expression_description = parameters["properties"]["expression"]["description"]
            .as_str()
            .unwrap();
        assert!(expression_description.contains("Required when action='evaluate'"));
    }

    #[test]
    fn captcha_attempt_marker_is_limited_to_visible_interactions() {
        for args in [
            json!({"action":"act","actAction":"click","ref":"s1-1","captchaAttempt":true}),
            json!({"action":"click_coordinates","x":10,"y":20,"captchaAttempt":true}),
            json!({"action":"send_keys","keys":"Space","captchaAttempt":true}),
        ] {
            BrowserTool::validate_action_args(args["action"].as_str().unwrap(), &args)
                .expect("visible challenge action must accept the marker");
        }

        for args in [
            json!({"action":"navigate","url":"https://example.com","captchaAttempt":true}),
            json!({"action":"evaluate","expression":"true","captchaAttempt":true}),
            json!({"action":"act","actAction":"upload","ref":"s1-1","path":"x","captchaAttempt":true}),
        ] {
            let error = BrowserTool::validate_action_args(args["action"].as_str().unwrap(), &args)
                .expect_err("non-challenge or privilege-expanding action must reject marker");
            assert_eq!(error.kind(), rustykrab_core::ToolErrorKind::InvalidInput);
        }
    }

    #[tokio::test]
    async fn malformed_action_arguments_are_typed_invalid_input() {
        let tool = BrowserTool::with_config(config::BrowserConfig::default());
        for (args, missing_field) in [
            (json!({ "action": "act", "ref": "e12" }), "actAction"),
            (json!({ "action": "evaluate" }), "expression"),
            (json!({ "action": "navigate" }), "url"),
        ] {
            let err = tool
                .execute(args)
                .await
                .expect_err("malformed action must fail before browser I/O");
            match err {
                Error::ToolExecution(tool_err) => {
                    assert_eq!(tool_err.kind, rustykrab_core::ToolErrorKind::InvalidInput);
                    assert!(
                        tool_err.message.contains(missing_field),
                        "{}",
                        tool_err.message
                    );
                }
                other => panic!("expected ToolExecution, got {other}"),
            }
        }
    }

    #[test]
    fn read_on_a_blank_tab_is_an_error() {
        // This is the regression: a snapshot that resolved to the startup
        // tab used to return `count: 0` as a success, which reads as "the
        // page is empty" rather than "you are on the wrong page".
        let err = BrowserTool::reject_blank_read("snapshot", "about:blank", true)
            .expect_err("blank read must fail");
        let msg = err.to_string();
        assert!(msg.contains("blank tab"), "{msg}");
        assert!(msg.contains("navigate"), "{msg}");
    }

    #[test]
    fn blank_read_message_distinguishes_never_navigated() {
        let never = BrowserTool::reject_blank_read("content", "", false)
            .expect_err("blank read must fail")
            .to_string();
        assert!(never.contains("no page has been navigated"), "{never}");

        let lost = BrowserTool::reject_blank_read("content", "", true)
            .expect_err("blank read must fail")
            .to_string();
        assert!(lost.contains("no longer available"), "{lost}");
    }

    #[test]
    fn reads_on_a_real_page_pass() {
        BrowserTool::reject_blank_read("snapshot", "https://www.instagram.com/cutty13/", true)
            .expect("a real page is readable");
    }

    #[test]
    fn non_read_actions_are_not_guarded() {
        // `navigate` legitimately starts from a blank tab — guarding it
        // would make the browser unusable from a cold start.
        BrowserTool::reject_blank_read("navigate", "about:blank", false)
            .expect("navigate may start blank");
        BrowserTool::reject_blank_read("act", "about:blank", true).expect("act is not a read");
    }

    /// End-to-end check of the whole fix against the page that exposed it:
    /// a real Instagram profile, which serves a login wall to a logged-out
    /// browser. The old resolution answered this with `about:blank` and an
    /// empty element list, which read to the model as "this page has nothing
    /// on it" — so it abandoned the browser and never came back.
    ///
    /// Needs the network and launches a real Chrome; ignored by default:
    ///
    /// ```sh
    /// cargo test -p rustykrab-tools --no-default-features \
    ///   browser::tests::live -- --ignored --nocapture
    /// ```
    #[tokio::test]
    #[ignore = "needs the network and launches a real Chrome"]
    async fn live_instagram_navigate_then_snapshot_sees_the_page() {
        const PROFILE_URL: &str = "https://www.instagram.com/secret_sanfrancisco/";

        let dir = tempfile::tempdir().expect("tempdir");
        let port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").expect("free port");
            l.local_addr().unwrap().port()
        };
        let profile = "instagram-live-test";

        let mut cfg = config::BrowserConfig {
            default_profile: profile.to_string(),
            ..Default::default()
        };
        cfg.profiles.insert(
            profile.to_string(),
            config::BrowserProfile {
                cdp_port: Some(port),
                user_data_dir: Some(dir.path().display().to_string()),
                headless: Some(true),
                ..Default::default()
            },
        );
        let tool = BrowserTool::with_config(cfg);

        let navigated = tool
            .execute(json!({"action": "navigate", "url": PROFILE_URL, "timeout_ms": 30000}))
            .await
            .expect("navigate");
        eprintln!("navigate -> {navigated}");

        let landed = navigated["url"].as_str().unwrap_or_default();
        assert!(
            !manager::is_blank_url(landed),
            "navigate landed on a blank tab: {navigated}"
        );
        let pinned = navigated["targetId"]
            .as_str()
            .expect("navigate must report the tab it used")
            .to_string();

        // The read that follows must observe the page that was navigated —
        // this is the exact pair that broke.
        let snapshot = tool
            .execute(json!({"action": "snapshot"}))
            .await
            .expect("snapshot must not fail on a loaded page");
        eprintln!(
            "snapshot -> count={} url={}",
            snapshot["count"], snapshot["url"]
        );
        for el in snapshot["elements"]
            .as_array()
            .into_iter()
            .flatten()
            .take(12)
        {
            eprintln!("  [{}] {} {}", el["ref"], el["role"], el["name"]);
        }

        assert!(
            !manager::is_blank_url(snapshot["url"].as_str().unwrap_or_default()),
            "snapshot resolved to a blank tab: {snapshot}"
        );
        assert!(
            snapshot["count"].as_u64().unwrap_or(0) > 0,
            "snapshot saw no elements: {snapshot}"
        );

        // And it stays on that page across further reads, without the model
        // having to thread a targetId through.
        let content = tool
            .execute(json!({"action": "content", "format": "text"}))
            .await
            .expect("content");
        assert!(
            !manager::is_blank_url(content["url"].as_str().unwrap_or(landed)),
            "content resolved to a blank tab: {content}"
        );

        let tabs = tool.execute(json!({"action": "tabs"})).await.expect("tabs");
        assert!(
            tabs["tabs"]
                .as_array()
                .expect("tabs array")
                .iter()
                .any(|t| t["targetId"].as_str() == Some(pinned.as_str())),
            "the pinned tab should still be listed: {tabs}"
        );

        let _ = tool.execute(json!({"action": "stop"})).await;
    }

    /// Repeat production BrowserTool navigation/snapshot/action/status calls
    /// against a stable public interactive page. This is intentionally ignored
    /// in the default suite because it requires network and Chrome, but it is a
    /// reproducible protocol soak for release verification.
    #[tokio::test]
    #[ignore = "needs the network and launches a real Chrome"]
    async fn live_public_w3c_accordion_protocol_soak() {
        const URL: &str = "https://www.w3.org/WAI/ARIA/apg/patterns/accordion/examples/accordion/";
        const ROUNDS: usize = 12;

        let profile = "w3c-protocol-soak";
        let (tool, _dir) = isolated_live_tool(profile);
        let navigation = tool
            .execute(json!({
                "action": "navigate",
                "profile": profile,
                "url": URL,
                "timeout_ms": 20_000,
            }))
            .await
            .expect("navigate public W3C fixture");
        assert_eq!(navigation["outcome"], "applied", "{navigation}");

        let target_id = navigation["targetId"]
            .as_str()
            .expect("navigated target id")
            .to_string();
        let mut slowest = Duration::ZERO;
        for round in 0..ROUNDS {
            let started = std::time::Instant::now();
            let snapshot = tool
                .execute(json!({
                    "action": "snapshot",
                    "profile": profile,
                    "targetId": target_id,
                    "interactive": true,
                }))
                .await
                .unwrap_or_else(|error| panic!("round {round} snapshot failed: {error}"));
            let button_ref = snapshot["elements"]
                .as_array()
                .expect("snapshot elements")
                .iter()
                .find(|element| {
                    element["role"] == "button"
                        && [
                            "Personal Information",
                            "Billing Address",
                            "Shipping Address",
                        ]
                        .contains(&element["name"].as_str().unwrap_or_default())
                })
                .and_then(|element| element["ref"].as_str())
                .unwrap_or_else(|| panic!("round {round} accordion button missing: {snapshot}"));
            let outcome = tool
                .execute(json!({
                    "action": "act",
                    "actAction": "click",
                    "ref": button_ref,
                    "profile": profile,
                    "targetId": target_id,
                }))
                .await
                .unwrap_or_else(|error| panic!("round {round} click failed: {error}"));
            assert_eq!(outcome["outcome"], "applied", "round {round}: {outcome}");
            assert_eq!(
                outcome["page_state_status"], "captured",
                "round {round}: post-action observation was not explicit: {outcome}"
            );
            assert!(
                outcome["page_state"]["count"].as_u64().unwrap_or(0) > 0,
                "round {round}: missing post-action state: {outcome}"
            );
            slowest = slowest.max(started.elapsed());
            assert!(
                started.elapsed() < Duration::from_secs(20),
                "round {round} exceeded action bound: {:?}",
                started.elapsed()
            );
        }

        let status = tool
            .execute(json!({"action": "status", "profile": profile}))
            .await
            .expect("post-soak browser status");
        assert_eq!(status["status"], "running", "{status}");
        assert_eq!(status["protocol_handler_running"], true, "{status}");
        assert_eq!(status["protocol_invalid_messages"], 0, "{status}");
        assert!(status["browser_product"].is_string(), "{status}");
        assert!(status["browser_protocol_version"].is_string(), "{status}");
        eprintln!("protocol_soak slowest_round={slowest:?} status={status}");

        let _ = tool.manager.stop(profile).await;
    }

    fn isolated_live_tool(profile: &str) -> (BrowserTool, tempfile::TempDir) {
        isolated_live_tool_with(profile, |_| {})
    }

    fn isolated_live_tool_with(
        profile: &str,
        configure: impl FnOnce(&mut config::BrowserConfig),
    ) -> (BrowserTool, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let port = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("free port");
            listener.local_addr().unwrap().port()
        };
        let mut config = config::BrowserConfig {
            default_profile: profile.to_string(),
            headless: true,
            cdp_request_timeout_ms: 5_000,
            ..Default::default()
        };
        config.ssrf_policy.allow_private_network = true;
        config
            .ssrf_policy
            .hostname_allowlist
            .push("localhost".to_string());
        configure(&mut config);
        config.profiles.insert(
            profile.to_string(),
            config::BrowserProfile {
                cdp_port: Some(port),
                user_data_dir: Some(dir.path().display().to_string()),
                headless: Some(true),
                ..Default::default()
            },
        );
        (BrowserTool::with_config(config), dir)
    }

    /// Exercise the complete monitored CAPTCHA interaction boundary in a
    /// real Chrome process. The fixture intentionally behaves like a visible
    /// two-step challenge without imitating or bypassing a third-party
    /// service: the first trusted click leaves the marker present, the second
    /// removes it. This proves detection, screenshot image transport, attempt
    /// accounting, and double-confirmed clearance independently of a model's
    /// reasoning quality.
    #[tokio::test]
    #[ignore = "launches a real Chrome"]
    async fn live_model_captcha_attempts_are_bounded_observed_and_multimodal() {
        let profile = "captcha-observability-live-test";
        let (tool, _dir) = isolated_live_tool_with(profile, |config| {
            config.model_captcha_solver = true;
            config.captcha_max_attempts = 3;
            config.captcha_timeout_ms = 30_000;
        });
        tool.manager
            .get_browser(profile)
            .await
            .expect("start browser");
        let page = tool
            .manager
            .get_page(profile, None)
            .await
            .expect("blank page");
        let html = r#"<!doctype html><html><body>
            <h1>Local challenge fixture</h1>
            <div class="cf-turnstile">
              <button id="challenge" aria-label="Verify challenge" onclick="
                window.challengeAttempts = (window.challengeAttempts || 0) + 1;
                document.getElementById('attempt-status').textContent =
                  'attempt:' + window.challengeAttempts + ':trusted:' + event.isTrusted;
                if (window.challengeAttempts === 2) this.parentElement.remove();
              ">Verify challenge</button>
            </div>
            <div id="attempt-status" role="status">attempt:0</div>
        </body></html>"#;
        let data_url = format!(
            "data:text/html;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(html)
        );
        page.execute(chromiumoxide::cdp::browser_protocol::page::NavigateParams::new(data_url))
            .await
            .expect("navigate fixture");
        tokio::time::sleep(Duration::from_millis(250)).await;
        let target_id = page.target_id().inner().clone();

        let snapshot = tool
            .execute(json!({
                "action":"snapshot", "profile":profile, "targetId":target_id,
                "interactive":false,
            }))
            .await
            .expect("initial snapshot");
        assert_eq!(snapshot["captcha"]["detected"], true, "{snapshot}");
        assert_eq!(
            snapshot["captcha_monitor"]["model_solver_enabled"], true,
            "{snapshot}"
        );
        assert_eq!(snapshot["captcha_monitor"]["attempts"], 0, "{snapshot}");

        let screenshot = tool
            .execute(json!({
                "action":"screenshot", "profile":profile, "targetId":target_id,
            }))
            .await
            .expect("fixture screenshot");
        let (screenshot_text, images) = rustykrab_core::types::split_tool_result_images(screenshot);
        assert!(screenshot_text.get("_images").is_none());
        assert_eq!(
            screenshot_text["screenshot"],
            "attached_to_multimodal_result"
        );
        assert!(screenshot_text["image_width"].as_u64().unwrap_or(0) > 0);
        assert!(screenshot_text["image_height"].as_u64().unwrap_or(0) > 0);
        assert_eq!(
            images.len(),
            1,
            "one PNG must cross the multimodal boundary"
        );
        assert!(matches!(
            &images[0],
            rustykrab_core::types::ContentBlock::Image { media_type, data }
                if media_type == "image/png" && !data.is_empty()
        ));

        let center: Vec<f64> = page
            .evaluate(
                "(function(){ const r=document.getElementById('challenge').getBoundingClientRect(); return [r.left+r.width/2,r.top+r.height/2]; })()",
            )
            .await
            .expect("read challenge center")
            .into_value()
            .expect("challenge center coordinates");
        let screenshot_x = center[0]
            * screenshot_text["image_width"]
                .as_f64()
                .expect("image width")
            / screenshot_text["viewport_width"]
                .as_f64()
                .expect("viewport width");
        let screenshot_y = center[1]
            * screenshot_text["image_height"]
                .as_f64()
                .expect("image height")
            / screenshot_text["viewport_height"]
                .as_f64()
                .expect("viewport height");

        let first = tool
            .execute(json!({
                "action":"click_coordinates", "x":screenshot_x, "y":screenshot_y,
                "profile":profile, "targetId":target_id,
                "captchaAttempt":true,
            }))
            .await
            .expect("first challenge interaction");
        assert_eq!(first["outcome"], "applied", "{first}");
        assert_eq!(first["captcha_attempt"]["result"], "in_progress", "{first}");
        assert_eq!(first["captcha_attempt"]["attempt"], 1, "{first}");
        assert!(
            (first["coordinate_mapping"]["dispatched_x"]
                .as_f64()
                .expect("dispatched x")
                - center[0])
                .abs()
                < 0.5,
            "{first}"
        );

        let second = tool
            .execute(json!({
                "action":"click_coordinates", "x":screenshot_x, "y":screenshot_y,
                "profile":profile, "targetId":target_id,
                "captchaAttempt":true,
            }))
            .await
            .expect("second challenge interaction");
        assert_eq!(second["outcome"], "applied", "{second}");
        assert_eq!(second["captcha_attempt"]["result"], "cleared", "{second}");
        assert_eq!(second["captcha_attempt"]["attempt"], 2, "{second}");
        assert_eq!(
            second["captcha_attempt"]["clearance_confirmations"], 2,
            "{second}"
        );

        let trusted: String = page
            .evaluate("document.getElementById('attempt-status').textContent")
            .await
            .expect("read fixture result")
            .into_value()
            .expect("fixture result string");
        assert_eq!(trusted, "attempt:2:trusted:true");

        let status = tool
            .execute(json!({
                "action":"status", "profile":profile, "targetId":target_id,
            }))
            .await
            .expect("monitor status");
        assert_eq!(status["captcha_monitor"]["totals"]["attempts"], 2);
        assert_eq!(status["captcha_monitor"]["totals"]["challenges_cleared"], 1);
        assert_eq!(
            status["captcha_monitor"]["recent_attempts"]
                .as_array()
                .expect("recent attempts")
                .len(),
            2
        );

        let _ = tool.manager.stop(profile).await;
    }

    /// Wix-style pages reuse `data-testid="linkElement"` for every link. The
    /// snapshot must retain exact identity and the action must click the chosen
    /// link, not the first match in document order.
    #[tokio::test]
    #[ignore = "launches a real Chrome"]
    async fn live_duplicate_testids_keep_distinct_click_identity() {
        let profile = "duplicate-selector-live-test";
        let (tool, _dir) = isolated_live_tool(profile);
        tool.manager
            .get_browser(profile)
            .await
            .expect("start browser");
        let page = tool
            .manager
            .get_page(profile, None)
            .await
            .expect("blank page");
        tokio::time::timeout(
            Duration::from_secs(10),
            page.set_content(
                r##"<html><body>
                    <a data-testid="linkElement" href="#first" onclick="document.body.dataset.clicked='first'">First choice</a>
                    <a data-testid="linkElement" href="#second" onclick="document.body.dataset.clicked='second'">Second choice</a>
                </body></html>"##,
            ),
        )
        .await
        .expect("set_content deadline")
        .expect("set_content");

        let key = "test:duplicate-selector";
        let snapshot = snapshot::take_snapshot(
            &page,
            &SnapshotOptions {
                interactive_only: true,
                ..Default::default()
            },
            &tool.snapshot_store,
            key,
        )
        .await
        .expect("snapshot");
        let second_ref = snapshot["elements"]
            .as_array()
            .expect("elements")
            .iter()
            .find(|element| element["name"] == "Second choice")
            .and_then(|element| element["ref"].as_str())
            .expect("second link ref")
            .to_string();
        let second = tool
            .snapshot_store
            .get_ref(key, &second_ref)
            .await
            .expect("stored second ref");
        assert!(
            !second.selector.contains("[data-testid=\"linkElement\"]"),
            "a duplicated test id must not be accepted as identity: {}",
            second.selector
        );

        let outcome = actions::execute_act(
            &page,
            &tool.snapshot_store,
            key,
            "click",
            &second_ref,
            &json!({}),
            actions::ActionPolicies {
                dialog: config::DialogPolicy::Auto,
                navigation: &tool.manager.config().ssrf_policy,
            },
        )
        .await
        .expect("click outcome");
        assert_eq!(outcome["outcome"], "applied", "{outcome}");
        let clicked: String = page
            .evaluate("document.body.dataset.clicked || ''")
            .await
            .expect("read click marker")
            .into_value()
            .expect("click marker string");
        assert_eq!(
            clicked, "second",
            "selector={} outcome={outcome}",
            second.selector
        );

        let _ = tool.manager.stop(profile).await;
    }

    /// Exercises the browser-use-compatible native action surface across the
    /// real BrowserTool boundary, including the automatic post-action state
    /// that supplies the next snapshot generation.
    #[tokio::test]
    #[ignore = "launches a real Chrome"]
    async fn live_native_forms_upload_coordinates_and_send_keys() {
        fn element<'a>(state: &'a Value, name: &str) -> &'a Value {
            state["elements"]
                .as_array()
                .expect("snapshot elements")
                .iter()
                .find(|element| element["name"] == name)
                .unwrap_or_else(|| panic!("element '{name}' missing from {state}"))
        }

        let profile = "native-actions-live-test";
        let (tool, _dir) = isolated_live_tool(profile);
        tool.manager
            .get_browser(profile)
            .await
            .expect("start browser");
        let page = tool
            .manager
            .get_page(profile, None)
            .await
            .expect("blank page");
        let html = r#"<!doctype html><html><body>
                <label>Message <input id="message" aria-label="Message"></label>
                <div id="key-status" role="status">No key</div>
                <label>Choice <select id="choice" aria-label="Choice">
                    <option value="a">Alpha</option><option value="b">Beta</option>
                </select></label>
                <input id="upload" type="file" aria-label="Upload fixture" hidden>
                <div id="upload-status" role="status">No upload</div>
                <button id="coordinate" onclick="this.textContent='Coordinate clicked'">Coordinate target</button>
                <div style="height:1600px"></div><h2>Late marker text</h2>
                <script>
                    message.addEventListener('keydown', function(e) {
                        document.getElementById('key-status').textContent = 'trusted:' + e.isTrusted + ':key:' + e.key;
                    });
                    upload.addEventListener('change', function() {
                        document.getElementById('upload-status').textContent = this.files.length ? 'Uploaded ' + this.files[0].name : 'No upload';
                    });
                </script>
            </body></html>"#;
        let data_url = format!(
            "data:text/html;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(html)
        );
        page.execute(chromiumoxide::cdp::browser_protocol::page::NavigateParams::new(data_url))
            .await
            .expect("navigate test page");
        tokio::time::sleep(Duration::from_millis(250)).await;
        let target_id = page.target_id().inner().clone();

        let snapshot = tool
            .execute(json!({
                "action":"snapshot", "profile":profile, "targetId":target_id,
                "interactive":false,
            }))
            .await
            .expect("initial snapshot");
        let message_ref = element(&snapshot, "Message")["ref"]
            .as_str()
            .expect("message ref");
        let focused = tool
            .execute(json!({
                "action":"act", "actAction":"click", "ref":message_ref,
                "profile":profile, "targetId":target_id,
            }))
            .await
            .expect("focus input");
        assert_eq!(focused["outcome"], "applied", "{focused}");

        let typed = tool
            .execute(json!({
                "action":"send_keys", "keys":"Hello", "profile":profile,
                "targetId":target_id,
            }))
            .await
            .expect("send trusted text");
        assert_eq!(typed["outcome"], "applied", "{typed}");
        assert_eq!(
            element(&typed["page_state"], "Message")["value"],
            "Hello",
            "{typed}"
        );
        assert!(
            typed["page_state"]["elements"]
                .as_array()
                .expect("post-key elements")
                .iter()
                .any(|element| element["name"]
                    .as_str()
                    .is_some_and(|name| name.starts_with("trusted:true:key:"))),
            "the DOM did not observe trusted CDP keyboard events: {typed}"
        );

        let choice_ref = element(&typed["page_state"], "Choice")["ref"]
            .as_str()
            .expect("choice ref");
        let options = tool
            .execute(json!({
                "action":"act", "actAction":"options", "ref":choice_ref,
                "profile":profile, "targetId":target_id,
            }))
            .await
            .expect("inspect options");
        assert_eq!(
            options["options"].as_array().map(Vec::len),
            Some(2),
            "{options}"
        );
        let choice_ref = element(&options["page_state"], "Choice")["ref"]
            .as_str()
            .expect("refreshed choice ref");
        let selected = tool
            .execute(json!({
                "action":"act", "actAction":"select", "ref":choice_ref, "value":"b",
                "profile":profile, "targetId":target_id,
            }))
            .await
            .expect("select option");
        assert_eq!(element(&selected["page_state"], "Choice")["value"], "b");

        let upload_ref = element(&selected["page_state"], "Upload fixture")["ref"]
            .as_str()
            .expect("upload ref");
        let fixture = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
        let uploaded = tool
            .execute(json!({
                "action":"act", "actAction":"upload", "ref":upload_ref,
                "path":fixture.display().to_string(), "profile":profile,
                "targetId":target_id,
            }))
            .await
            .expect("upload fixture");
        assert!(
            uploaded["page_state"]["elements"]
                .as_array()
                .expect("post-upload elements")
                .iter()
                .any(|element| element["name"] == "Uploaded Cargo.toml"),
            "upload change was not independently observed: {uploaded}"
        );

        let coordinate = element(&uploaded["page_state"], "Coordinate target");
        let bounds = coordinate["bounds"].as_array().expect("coordinate bounds");
        let x = bounds[0].as_f64().unwrap() + bounds[2].as_f64().unwrap() / 2.0;
        let y = bounds[1].as_f64().unwrap() + bounds[3].as_f64().unwrap() / 2.0;
        let clicked = tool
            .execute(json!({
                "action":"click_coordinates", "x":x, "y":y,
                "profile":profile, "targetId":target_id,
            }))
            .await
            .expect("coordinate click");
        assert_eq!(clicked["outcome"], "applied", "{clicked}");
        assert!(
            clicked["page_state"]["elements"]
                .as_array()
                .expect("post-coordinate elements")
                .iter()
                .any(|element| element["name"] == "Coordinate clicked"),
            "coordinate click was not independently observed: {clicked}"
        );

        let scrolled = tool
            .execute(json!({
                "action":"scroll_to_text", "text":"Late marker text",
                "profile":profile, "targetId":target_id,
            }))
            .await
            .expect("scroll to text");
        assert_eq!(scrolled["outcome"], "applied", "{scrolled}");

        let _ = tool.manager.stop(profile).await;
    }

    /// Proves the full ref path across the browser's same-origin boundary:
    /// discover an element in a child frame, preserve its frame identity, and
    /// apply a real mouse click without the parent document accessing the
    /// frame DOM. Two loopback hostnames make the documents different origins.
    #[tokio::test]
    #[ignore = "launches a real Chrome"]
    async fn live_cross_origin_iframe_snapshot_and_native_actions() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        fn frame_element<'a>(state: &'a Value, name: &str) -> &'a Value {
            state["elements"]
                .as_array()
                .expect("snapshot elements")
                .iter()
                .find(|element| element["name"] == name)
                .unwrap_or_else(|| panic!("frame element '{name}' missing from {state}"))
        }

        async fn serve(
            body: String,
            advertised_host: &str,
        ) -> (String, tokio::task::JoinHandle<()>) {
            let listener = tokio::net::TcpListener::bind("0.0.0.0:0")
                .await
                .expect("bind test server");
            let port = listener.local_addr().expect("server address").port();
            let task = tokio::spawn(async move {
                loop {
                    let Ok((mut socket, _)) = listener.accept().await else {
                        return;
                    };
                    let mut request = [0_u8; 2048];
                    let _ = socket.read(&mut request).await;
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(), body
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                    let _ = socket.shutdown().await;
                }
            });
            (format!("http://{advertised_host}:{port}"), task)
        }

        let child_body = r#"<!doctype html><html><body>
            <button id="inside" onclick="this.textContent='Clicked inside frame'">Click inside frame</button>
            <input id="frame-input" aria-label="Frame input" onkeydown="if(event.key==='Enter'){document.getElementById('key-status').textContent=event.isTrusted?'Trusted frame key':'Synthetic frame key'}">
            <p id="key-status" role="status"></p>
            <select id="frame-choice" aria-label="Frame choice">
                <option value="alpha">Alpha</option><option value="beta">Beta</option>
            </select>
            <input id="frame-upload" type="file" aria-label="Frame upload" hidden onchange="document.getElementById('upload-status').textContent='Uploaded '+this.files[0].name">
            <p id="upload-status" role="status"></p>
        </body></html>"#.to_string();
        let (child_url, child_server) = serve(child_body, "127.0.0.1").await;
        let parent_body = format!(
            r#"<!doctype html><html><body><h1>Parent</h1><iframe src="{child_url}"></iframe></body></html>"#
        );
        let (parent_url, parent_server) = serve(parent_body, "localhost").await;

        let profile = "cross-origin-frame-live-test";
        let (tool, _dir) = isolated_live_tool(profile);
        tool.manager
            .get_browser(profile)
            .await
            .expect("start browser");
        let page = tool
            .manager
            .get_page(profile, None)
            .await
            .expect("blank page");
        tokio::time::timeout(
            Duration::from_secs(10),
            page.execute(
                chromiumoxide::cdp::browser_protocol::page::NavigateParams::new(&parent_url),
            ),
        )
        .await
        .expect("navigation deadline")
        .expect("navigate parent");
        tokio::time::sleep(Duration::from_secs(2)).await;

        let parent_can_read_child: bool = page
            .evaluate(
                "Boolean(document.querySelector('iframe').contentDocument && document.querySelector('iframe').contentDocument.body)",
            )
            .await
            .expect("same-origin probe")
            .into_value()
            .expect("boolean same-origin result");
        assert!(!parent_can_read_child, "fixture must be cross-origin");

        let target_id = page.target_id().inner().clone();
        let snapshot = tool
            .execute(json!({
                "action": "snapshot",
                "profile": profile,
                "targetId": target_id,
                "interactive": true,
            }))
            .await
            .expect("frame-aware snapshot");
        let frame_button = snapshot["elements"]
            .as_array()
            .expect("snapshot elements")
            .iter()
            .find(|element| element["name"] == "Click inside frame")
            .unwrap_or_else(|| panic!("button inside cross-origin frame: {snapshot}"));
        assert!(frame_button["frame_id"].is_string(), "{frame_button}");
        assert!(
            frame_button["target_id"].is_string(),
            "the fixture must be captured through a real OOPIF target: {frame_button}"
        );
        let frame_ref = frame_button["ref"].as_str().expect("frame ref");

        let outcome = tool
            .execute(json!({
                "action": "act",
                "actAction": "click",
                "profile": profile,
                "targetId": target_id,
                "ref": frame_ref,
            }))
            .await
            .expect("cross-origin click outcome");
        assert_eq!(outcome["outcome"], "applied", "{outcome}");
        assert_eq!(outcome["page_state_status"], "captured", "{outcome}");
        let post_elements = outcome["page_state"]["elements"]
            .as_array()
            .expect("post-action frame-aware snapshot");
        assert!(
            post_elements
                .iter()
                .any(|element| element["name"] == "Clicked inside frame"),
            "post-action snapshot did not independently observe the click: {outcome}"
        );

        let input_ref = frame_element(&outcome["page_state"], "Frame input")["ref"]
            .as_str()
            .expect("OOPIF input ref")
            .to_string();
        let typed = tool
            .execute(json!({
                "action":"act", "actAction":"fill", "text":"OOPIF text",
                "profile":profile, "targetId":target_id, "ref":input_ref,
            }))
            .await
            .expect("cross-origin fill outcome");
        assert_eq!(typed["outcome"], "applied", "{typed}");
        assert_eq!(
            frame_element(&typed["page_state"], "Frame input")["value"],
            "OOPIF text",
            "{typed}"
        );

        let input_ref = frame_element(&typed["page_state"], "Frame input")["ref"]
            .as_str()
            .expect("refreshed OOPIF input ref");
        let pressed = tool
            .execute(json!({
                "action":"act", "actAction":"press", "key":"Enter",
                "profile":profile, "targetId":target_id, "ref":input_ref,
            }))
            .await
            .expect("cross-origin key outcome");
        assert_eq!(pressed["outcome"], "applied", "{pressed}");
        assert!(
            pressed["page_state"]["elements"]
                .as_array()
                .expect("post-key snapshot")
                .iter()
                .any(|element| element["name"] == "Trusted frame key"),
            "OOPIF key event was not independently observed as trusted: {pressed}"
        );

        let choice_ref = frame_element(&pressed["page_state"], "Frame choice")["ref"]
            .as_str()
            .expect("OOPIF choice ref");
        let options = tool
            .execute(json!({
                "action":"act", "actAction":"options",
                "profile":profile, "targetId":target_id, "ref":choice_ref,
            }))
            .await
            .expect("cross-origin options outcome");
        assert_eq!(
            options["options"].as_array().map(Vec::len),
            Some(2),
            "{options}"
        );
        let choice_ref = frame_element(&options["page_state"], "Frame choice")["ref"]
            .as_str()
            .expect("refreshed OOPIF choice ref");
        let selected = tool
            .execute(json!({
                "action":"act", "actAction":"select", "value":"beta",
                "profile":profile, "targetId":target_id, "ref":choice_ref,
            }))
            .await
            .expect("cross-origin select outcome");
        assert_eq!(selected["outcome"], "applied", "{selected}");
        assert_eq!(
            frame_element(&selected["page_state"], "Frame choice")["value"],
            "beta",
            "{selected}"
        );

        let upload_ref = frame_element(&selected["page_state"], "Frame upload")["ref"]
            .as_str()
            .expect("OOPIF upload ref");
        let fixture = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
        let uploaded = tool
            .execute(json!({
                "action":"act", "actAction":"upload",
                "path":fixture.display().to_string(),
                "profile":profile, "targetId":target_id, "ref":upload_ref,
            }))
            .await
            .expect("cross-origin upload outcome");
        assert_eq!(uploaded["outcome"], "applied", "{uploaded}");
        assert!(
            uploaded["page_state"]["elements"]
                .as_array()
                .expect("post-upload snapshot")
                .iter()
                .any(|element| element["name"] == "Uploaded Cargo.toml"),
            "OOPIF upload was not independently observed: {uploaded}"
        );

        let _ = tool.manager.stop(profile).await;
        parent_server.abort();
        child_server.abort();
    }

    /// Verifies the full production tool boundary for downloads: a click is
    /// correlated with browser-level lifecycle events, completion has an
    /// independently observed file, and the reported canonical path remains
    /// inside the profile's download directory.
    #[tokio::test]
    #[ignore = "launches a real Chrome"]
    async fn live_download_click_reports_validated_completion() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind download fixture");
        let port = listener.local_addr().expect("fixture address").port();
        let server = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let mut request = [0_u8; 4096];
                let read = socket.read(&mut request).await.unwrap_or(0);
                let request = String::from_utf8_lossy(&request[..read]);
                let response = if request.starts_with("GET /file ") {
                    let payload = b"rustykrab download verification\n";
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Disposition: attachment; filename=\"../../verified.txt\"\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        payload.len(),
                        String::from_utf8_lossy(payload)
                    )
                } else {
                    let body = r#"<!doctype html><html><body><a id="download" href="/file">Download fixture</a></body></html>"#;
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(), body
                    )
                };
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.shutdown().await;
            }
        });

        let profile = "download-live-test";
        let (tool, _dir) = isolated_live_tool(profile);
        tool.manager
            .get_browser(profile)
            .await
            .expect("start browser with download observer");
        let status = tool.manager.status(profile).await;
        assert_eq!(status["downloads_status"], "ready", "{status}");
        let page = tool
            .manager
            .get_page(profile, None)
            .await
            .expect("blank page");
        let url = format!("http://127.0.0.1:{port}/");
        tokio::time::timeout(Duration::from_secs(10), page.goto(&url))
            .await
            .expect("fixture navigation deadline")
            .expect("navigate fixture");

        let target_id = page.target_id().inner().clone();
        let key = BrowserTool::store_key("global", profile, Some(&target_id));
        let snapshot = snapshot::take_snapshot(
            &page,
            &SnapshotOptions {
                interactive_only: true,
                ..Default::default()
            },
            &tool.snapshot_store,
            &key,
        )
        .await
        .expect("download fixture snapshot");
        let download_ref = snapshot["elements"]
            .as_array()
            .expect("snapshot elements")
            .iter()
            .find(|element| element["name"] == "Download fixture")
            .and_then(|element| element["ref"].as_str())
            .expect("download link ref");

        let outcome = tool
            .execute(json!({
                "action": "act",
                "actAction": "click",
                "ref": download_ref,
                "profile": profile,
                "targetId": target_id,
                "expect_download": true,
                "download_timeout_ms": 10_000,
            }))
            .await
            .expect("download click outcome");
        assert_eq!(outcome["outcome"], "applied", "{outcome}");
        assert_eq!(
            outcome["download_observation"]["status"], "terminal",
            "{outcome}"
        );
        assert_eq!(outcome["download_observation"]["completed"], 1, "{outcome}");
        let download = &outcome["download_observation"]["downloads"][0];
        assert_eq!(download["status"], "completed", "{outcome}");
        assert_eq!(download["path_status"], "validated", "{outcome}");
        let reported_path =
            std::path::PathBuf::from(download["path"].as_str().expect("validated download path"));
        let configured_root = std::fs::canonicalize(status["download_dir"].as_str().unwrap())
            .expect("canonical download root");
        assert!(reported_path.starts_with(&configured_root), "{outcome}");
        assert_eq!(
            std::fs::read(&reported_path).expect("downloaded file"),
            b"rustykrab download verification\n"
        );

        let _ = tool.manager.stop(profile).await;
        server.abort();
    }

    /// Native dialogs freeze the renderer. The browser-use pattern is to
    /// handle them concurrently with the action and report what was closed.
    #[tokio::test]
    #[ignore = "launches a real Chrome"]
    async fn live_click_accepts_and_reports_javascript_dialog() {
        let profile = "dialog-click-live-test";
        let (tool, _dir) = isolated_live_tool(profile);
        tool.manager
            .get_browser(profile)
            .await
            .expect("start browser");
        let page = tool
            .manager
            .get_page(profile, None)
            .await
            .expect("blank page");
        tokio::time::timeout(
            Duration::from_secs(10),
            page.set_content(
                r##"<html><body><button id="dialog" onclick="document.body.dataset.clicked='before'; alert('dialog observed'); document.body.dataset.clicked='after'">Open dialog</button></body></html>"##,
            ),
        )
        .await
        .expect("set_content deadline")
        .expect("set_content");

        let key = "test:dialog-click";
        let snapshot = snapshot::take_snapshot(
            &page,
            &SnapshotOptions {
                interactive_only: true,
                ..Default::default()
            },
            &tool.snapshot_store,
            key,
        )
        .await
        .expect("snapshot");
        let ref_id = snapshot["elements"][0]["ref"].as_str().expect("button ref");

        let outcome = actions::execute_act(
            &page,
            &tool.snapshot_store,
            key,
            "click",
            ref_id,
            &json!({}),
            actions::ActionPolicies {
                dialog: config::DialogPolicy::Auto,
                navigation: &config::SsrfPolicy::default(),
            },
        )
        .await
        .expect("dialog click outcome");
        assert_eq!(outcome["outcome"], "applied", "{outcome}");
        assert_eq!(outcome["dialogs"][0]["type"], "alert", "{outcome}");
        assert_eq!(outcome["dialogs"][0]["accepted"], true, "{outcome}");
        let clicked: String = page
            .evaluate("document.body.dataset.clicked || ''")
            .await
            .expect("read post-dialog marker")
            .into_value()
            .expect("post-dialog marker string");
        assert_eq!(clicked, "after");

        let _ = tool.manager.stop(profile).await;
    }

    /// A page that blocks its renderer from a click handler used to consume
    /// chromiumoxide's 30-second request timeout repeatedly and then the
    /// runner's 60-second tool timeout. The complete action must now return an
    /// explicit unknown outcome within its own deadline.
    #[tokio::test]
    #[ignore = "launches a real Chrome"]
    async fn live_hanging_click_is_bounded_and_ambiguous() {
        let profile = "hanging-click-live-test";
        let (tool, _dir) = isolated_live_tool(profile);
        tool.manager
            .get_browser(profile)
            .await
            .expect("start browser");
        let page = tool
            .manager
            .get_page(profile, None)
            .await
            .expect("blank page");
        tokio::time::timeout(
            Duration::from_secs(10),
            page.set_content(
                r##"<html><body><button id="hang" onclick="while(true){}">Hang renderer</button></body></html>"##,
            ),
        )
        .await
        .expect("set_content deadline")
        .expect("set_content");

        let target_id = page.target_id().inner().clone();
        let key = BrowserTool::store_key("global", profile, Some(&target_id));
        let snapshot = snapshot::take_snapshot(
            &page,
            &SnapshotOptions {
                interactive_only: true,
                ..Default::default()
            },
            &tool.snapshot_store,
            &key,
        )
        .await
        .expect("snapshot");
        let ref_id = snapshot["elements"][0]["ref"]
            .as_str()
            .expect("button ref")
            .to_string();

        let started = std::time::Instant::now();
        let outcome = tool
            .execute(json!({
                "action": "act",
                "actAction": "click",
                "ref": ref_id,
                "profile": profile,
                "targetId": target_id,
            }))
            .await
            .expect("bounded click outcome");
        assert!(
            started.elapsed() < Duration::from_secs(35),
            "action plus browser recovery exceeded its deadline: {:?}",
            started.elapsed()
        );
        assert_eq!(outcome["outcome"], "unknown", "{outcome}");
        assert_eq!(outcome["retry_safe"], false, "{outcome}");
        assert_eq!(outcome["browser_degraded"], true, "{outcome}");
        assert_eq!(outcome["recovery"]["status"], "recovered", "{outcome}");

        // Independent post-recovery probe: the replacement Chrome accepts a
        // new renderer command, rather than merely reporting that relaunch
        // code ran.
        let replacement = tool
            .manager
            .get_page(profile, None)
            .await
            .expect("replacement page");
        tokio::time::timeout(
            Duration::from_secs(10),
            replacement.set_content("<p id='healthy'>recovered</p>"),
        )
        .await
        .expect("replacement renderer deadline")
        .expect("replacement renderer command");
        let healthy: String = replacement
            .evaluate("document.querySelector('#healthy').textContent")
            .await
            .expect("replacement renderer response")
            .into_value()
            .expect("replacement renderer text");
        assert_eq!(healthy, "recovered");

        let _ = tool.manager.stop(profile).await;
    }

    #[test]
    fn store_key_separates_tabs() {
        assert_eq!(
            BrowserTool::store_key("conv-a", "default", Some("TARGET-1")),
            "conv-a:default:TARGET-1"
        );
        assert_ne!(
            BrowserTool::store_key("conv-a", "default", Some("TARGET-1")),
            BrowserTool::store_key("conv-a", "default", Some("TARGET-2"))
        );
        assert_ne!(
            BrowserTool::store_key("conv-a", "default", Some("TARGET-1")),
            BrowserTool::store_key("conv-b", "default", Some("TARGET-1"))
        );
        assert_eq!(
            BrowserTool::store_key("conv-a", "default", None),
            "conv-a:default:active"
        );
    }
}

#[cfg(test)]
mod act_action_tests {
    use super::effective_action;
    use serde_json::json;

    #[test]
    fn fill_credential_as_a_sub_action_is_accepted() {
        // The spelling a model reaches for when it holds a snapshot ref.
        let args = json!({"action": "act", "ref": "e12", "actAction": "fill_credential",
                          "field": "password"});
        assert_eq!(effective_action("act", &args), "fill_credential");
    }

    #[test]
    fn top_level_spelling_still_works() {
        let args = json!({"action": "fill_credential", "ref": "e12", "field": "password"});
        assert_eq!(
            effective_action("fill_credential", &args),
            "fill_credential"
        );
    }

    #[test]
    fn other_sub_actions_are_untouched() {
        for sub in [
            "click", "type", "fill", "press", "hover", "select", "drag", "wait",
        ] {
            let args = json!({"action": "act", "ref": "e1", "actAction": sub});
            assert_eq!(
                effective_action("act", &args),
                "act",
                "{sub} should stay an act"
            );
        }
    }

    #[test]
    fn act_without_a_sub_action_is_untouched() {
        let args = json!({"action": "act", "ref": "e1"});
        assert_eq!(effective_action("act", &args), "act");
    }

    #[test]
    fn the_schema_advertises_it_so_the_model_need_not_guess() {
        // The fallback that made this matter was the model typing the
        // credential's key name into the form; it should not have to
        // infer the spelling from a rejection.
        let params = super::schema_parameters();
        let sub = &params["properties"]["actAction"];
        let variants: Vec<&str> = sub["enum"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert!(
            variants.contains(&"fill_credential"),
            "actAction enum: {variants:?}"
        );
    }
}
