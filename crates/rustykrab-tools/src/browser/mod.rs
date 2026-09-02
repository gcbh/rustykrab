//! Browser automation tool modeled after OpenClaw's browser management.
//!
//! Provides a comprehensive browser control surface with:
//! - Multi-profile browser management (isolated Chrome instances)
//! - Browser lifecycle (status/start/stop)
//! - Tab management (tabs/open/close/focus) addressed by Chrome target ID
//! - Accessibility-tree snapshots with element refs
//! - Ref-based actions (click/type/press/hover/select/drag via snapshot refs)
//! - Screenshot, navigate, evaluate, console, PDF, scroll
//! - SSRF protection and cookie security

pub mod actions;
pub mod adaptive;
pub mod config;
pub mod fetcher;
pub mod manager;
pub mod selectors;
pub mod snapshot;
pub mod stealth;

use async_trait::async_trait;
use base64::Engine;
use chromiumoxide::cdp::browser_protocol::network::Cookie;
use chromiumoxide::page::ScreenshotParams;
use rustykrab_core::types::ToolSchema;
use rustykrab_core::{Error, Result, SandboxRequirements, Tool, ToolError};
use serde_json::{json, Value};

use crate::security;
use adaptive::AdaptiveStore;
use manager::BrowserManager;
use snapshot::{SnapshotMode, SnapshotOptions, SnapshotStore};

const MAX_CONTENT_BYTES: usize = 50 * 1024; // 50KB cap for page content

/// Browser automation tool using Chrome DevTools Protocol.
///
/// Modeled after OpenClaw's browser management architecture:
/// - Multiple named browser profiles, each an isolated Chrome instance
/// - Browser lifecycle management (status/start/stop)
/// - Tab control (tabs/open/close/focus) by stable Chrome target ID
/// - Accessibility-tree snapshots with element refs for actions
/// - Ref-based interactions (click ref 12, type ref 5 "hello")
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
                    "tabs", "open", "close", "focus",
                    "navigate", "snapshot", "act", "screenshot",
                    "content", "evaluate", "scroll",
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
                "description": "Tab identifier from 'tabs' action (e.g., 'tab_0'). Used by close/focus/navigate/snapshot/act/screenshot/content/evaluate"
            },
            "ref": {
                "type": "string",
                "description": "Element ref from a snapshot (e.g., '12' or 'e12'). Required for act and fill_credential"
            },
            "actAction": {
                "type": "string",
                "enum": ["click", "type", "fill", "press", "hover", "select", "drag", "wait", "fill_credential"],
                "description": "Required when action='act'; every act also requires ref. Companion fields: type/fill -> text; press -> key; select -> value; drag -> targetRef. fill_credential uses field='username' or 'password' and never text, so the stored secret does not pass through you."
            },
            "text": {
                "type": "string",
                "description": "Required for actAction='type' or 'fill'"
            },
            "key": {
                "type": "string",
                "description": "Required for actAction='press' (e.g., 'Enter', 'Tab', 'Escape')"
            },
            "value": {
                "type": "string",
                "description": "Required for actAction='select'"
            },
            "targetRef": {
                "type": "string",
                "description": "Required target element ref for actAction='drag'"
            },
            "clear": {
                "type": "boolean",
                "description": "Clear field before typing (default: true for fill, false for type)"
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
                    "click" | "hover" | "wait" | "fill_credential" => {}
                    other => {
                        return Err(Error::ToolExecution(ToolError::invalid_input(format!(
                            "unknown act action '{other}'. Available: click, type, fill, press, hover, select, drag, wait, fill_credential"
                        ))));
                    }
                }
            }
            "fill_credential" => require_non_empty(args, "ref", action)?,
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
            "status" | "start" | "stop" | "profiles" | "tabs" | "snapshot" | "screenshot"
            | "content" | "scroll" | "console" | "cookies" | "pdf" | "select" => {}
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

    /// Build a snapshot store key from profile + target.
    fn store_key(profile: &str, target_id: Option<&str>) -> String {
        match target_id {
            Some(tid) => format!("{profile}:{tid}"),
            None => format!("{profile}:active"),
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
        "snapshot",
        "act",
        "fill_credential",
        "screenshot",
        "content",
        "evaluate",
        "scroll",
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
        Ok(page)
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
         Chrome via DevTools Protocol. \
         Fetch modes: \
         fetch — pure HTTP request with browser-like header packs (impersonate=chrome|firefox|safari|edge), stealthy_headers, custom user-agent, proxy, retries, redirects; \
         stealth_fetch — full browser navigation with anti-bot patches (block_webrtc, hide_canvas, disable_resources), wait_selector, network_idle, solve_cloudflare, and returns rendered body; \
         select — CSS or XPath selector engine over either provided html or the active tab DOM, with Scrapling pseudo-selectors ::text and ::attr(name), find_by_text (regex or substring), and adaptive auto_save/auto_match across DOM changes. \
         Browser control: \
         status/start/stop — lifecycle; \
         profiles — list profiles; \
         tabs/open/close/focus — tab management; \
         navigate — go to URL (supports wait_selector, wait_selector_state, network_idle, solve_cloudflare); \
         wait_for — wait for selector / network idle / fixed delay; \
         snapshot — accessibility-tree with element refs; \
         act — interact by ref (click/type/press/hover/select/drag); \
         fill_credential — type a STORED credential into a field by ref, without \
         ever seeing its value: pass 'ref' and 'field' ('username' or 'password'). \
         Always sign in with this. Never use act/type for a password: you do not \
         have the value, and typing a placeholder or the credential's own name just \
         fails the login. If nothing is stored yet the error names the exact key — \
         ask for it with credential_request under that name, then retry; \
         screenshot/content/evaluate/scroll/console/cookies/pdf. \
         Cookies persist across calls. Use snapshot + act for reliable element interaction. \
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

        // Decide which page this call addresses once, here: an explicit
        // `targetId` wins, otherwise the tab this session last navigated,
        // provided it is still open. Every page action below reads
        // `target_id`, so this is the single place "which page?" is answered
        // — previously each one re-resolved it and could land on a different
        // tab than the call before.
        let resolved_target = self.resolve_target(action, &profile, &session, &args).await;
        let target_id = resolved_target.as_deref();

        match action {
            // ── Lifecycle ──────────────────────────────────────────
            "status" => Ok(self.manager.status(&profile).await),

            "start" => self.manager.start(&profile).await,

            "stop" => self.manager.stop(&profile).await,

            "profiles" => Ok(self.manager.profiles().await),

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
                security::validate_url(url)
                    .await
                    .map_err(|e| Error::ToolExecution(e.into()))?;
                let _ = self.manager.get_browser(&profile).await?;
                let opened = self.manager.open_tab(&profile, url).await?;
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
                security::validate_url(url)
                    .await
                    .map_err(|e| Error::ToolExecution(e.into()))?;

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

                // `goto` gets the caller's budget too, not just the settle
                // wait below. Unbounded, it falls through to the CDP client's
                // own request timeout, so `timeout_ms` silently did not bound
                // the navigation it names -- a server that accepts the
                // connection and never answers cost 30s per attempt, and the
                // runner then retried it.
                let timeout_ms = args["timeout_ms"].as_u64().unwrap_or(10_000);
                match tokio::time::timeout(
                    std::time::Duration::from_millis(timeout_ms),
                    page.goto(url),
                )
                .await
                {
                    Ok(r) => r.map_err(|e| {
                        Error::ToolExecution(format!("navigation failed: {e}").into())
                    })?,
                    Err(_) => {
                        return Err(Error::ToolExecution(
                            format!(
                                "navigation to '{url}' did not complete within {timeout_ms}ms. \
                                 The browser is alive and accepted the request, so this is the \
                                 page or the server it talks to, not the browser: a server that \
                                 accepts the connection and never responds looks exactly like \
                                 this. Retrying the same URL will usually fail the same way."
                            )
                            .into(),
                        ));
                    }
                };
                let _ = tokio::time::timeout(
                    std::time::Duration::from_millis(timeout_ms),
                    page.wait_for_navigation(),
                )
                .await;

                // Apply DOM-level stealth patches (post-navigation).
                let _ = stealth::apply_stealth(&page, &stealth_opts).await;

                let mut wait_results = serde_json::Map::new();
                if let Some(sel) = args["wait_selector"].as_str() {
                    let state = stealth::WaitState::parse(
                        args["wait_selector_state"].as_str().unwrap_or("visible"),
                    );
                    let ok = stealth::wait_for_selector(&page, sel, state, timeout_ms).await?;
                    wait_results.insert("wait_selector".into(), Value::Bool(ok));
                }
                if args["network_idle"].as_bool().unwrap_or(false) {
                    let ok = stealth::wait_for_network_idle(&page, 500, timeout_ms).await?;
                    wait_results.insert("network_idle".into(), Value::Bool(ok));
                }
                if args["solve_cloudflare"].as_bool().unwrap_or(false) {
                    let ok = stealth::solve_cloudflare(&page, timeout_ms).await?;
                    wait_results.insert("cloudflare_clear".into(), Value::Bool(ok));
                }
                if let Some(delay) = args["delay_ms"].as_u64() {
                    tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                }

                let title = manager::probe_page_title_once(&page)
                    .await
                    .unwrap_or_default();
                let current_url = page.url().await.ok().flatten().unwrap_or_default();

                // Pin the tab we just loaded so the snapshot/content call
                // that follows reads this page and not some other one.
                let landed_on = page.target_id().inner().clone();
                self.manager
                    .set_sticky_target(&session, &profile, &landed_on);

                Ok(json!({
                    "title": title,
                    "url": current_url,
                    "status": "loaded",
                    "targetId": landed_on,
                    "waits": Value::Object(wait_results),
                    "profile": profile
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

                let key = Self::store_key(&profile, target_id);
                let snap =
                    snapshot::take_snapshot(&page, &options, &self.snapshot_store, &key).await;

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
                if let Ok(ref v) = snap {
                    let url = page.url().await.ok().flatten().unwrap_or_default();
                    tracing::debug!(
                        url = %url,
                        chars = v.to_string().len(),
                        nodes = v["elements"].as_array().map(|a| a.len()).unwrap_or(0),
                        "took page snapshot"
                    );
                }
                snap
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
                        "'act' requires 'actAction' parameter (click, type, press, hover, select, drag, wait)".into(),
                    ))?;

                let _ = self.manager.get_browser(&profile).await?;
                let page = self.manager.get_page(&profile, target_id).await?;
                let key = Self::store_key(&profile, target_id);

                actions::execute_act(&page, &self.snapshot_store, &key, act_action, ref_id, &args)
                    .await
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
                let page = self.manager.get_page(&profile, target_id).await?;

                // The page's own URL, so the key matches wherever the
                // agent actually is rather than where it meant to be.
                let url = match args["url"].as_str() {
                    Some(u) => u.to_string(),
                    None => page.url().await.ok().flatten().unwrap_or_default(),
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

                let store_key = Self::store_key(&profile, target_id);
                let fill_args = json!({ "text": value, "clear": true });
                actions::execute_act(
                    &page,
                    &self.snapshot_store,
                    &store_key,
                    "fill",
                    ref_id,
                    &fill_args,
                )
                .await?;

                // A fresh result rather than the fill's own. Nothing
                // derived from the value travels back to the model — not
                // its length, which for a password is worth guessing with.
                // The length is reported so a caller can tell an empty
                // or placeholder fill from a real one. The value is not.
                Ok(json!({
                    "status": "filled",
                    "field": field,
                    "credentialKey": cred_key,
                    "ref": ref_id,
                    "value_len": value.len(),
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
                let b64 = base64::engine::general_purpose::STANDARD.encode(&png_bytes);

                Ok(json!({
                    "screenshot": b64,
                    "size_bytes": size_bytes,
                    "format": "png",
                    "encoding": "base64",
                    "profile": profile
                }))
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
                let current_url = page.url().await.ok().flatten().unwrap_or_default();

                Ok(json!({
                    "content": truncated_content,
                    "url": current_url,
                    "title": title,
                    "format": format,
                    "truncated": was_truncated,
                    "profile": profile
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
                let page = self.manager.get_page(&profile, target_id).await?;
                let result = page.evaluate(expression).await.map_err(|e| {
                    Error::ToolExecution(format!("JS evaluation failed: {e}").into())
                })?;

                let value: Value = result.into_value().unwrap_or(Value::Null);

                Ok(json!({
                    "result": value,
                    "profile": profile
                }))
            }

            // ── Scroll ─────────────────────────────────────────────
            "scroll" => {
                let direction = args["direction"].as_str().unwrap_or("down");
                let amount = args["amount"].as_i64().unwrap_or(500);

                let _ = self.manager.get_browser(&profile).await?;
                let page = self.manager.get_page(&profile, target_id).await?;

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

                Ok(json!({
                    "status": "scrolled",
                    "direction": direction,
                    "scroll_y": scroll_y as i64,
                    "profile": profile
                }))
            }

            // ── Console ────────────────────────────────────────────
            "console" => {
                let _ = self.manager.get_browser(&profile).await?;
                let page = self.manager.get_page(&profile, target_id).await?;

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

                Ok(json!({
                    "console": entries,
                    "note": "Console interception is installed on first call. Earlier messages are not captured.",
                    "profile": profile
                }))
            }

            // ── Cookies ────────────────────────────────────────────
            "cookies" => {
                let _ = self.manager.get_browser(&profile).await?;
                let page = self.manager.get_page(&profile, target_id).await?;
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

                Ok(json!({
                    "cookies": filtered,
                    "count": filtered.len(),
                    "profile": profile
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

                let url = page.url().await.ok().flatten().unwrap_or_default();
                let title = manager::probe_page_title_once(&page)
                    .await
                    .unwrap_or_default();

                Ok(json!({
                    "pdf": b64,
                    "size_bytes": size_bytes,
                    "encoding": "base64",
                    "url": url,
                    "title": title,
                    "profile": profile
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
                security::validate_url(url)
                    .await
                    .map_err(|e| Error::ToolExecution(e.into()))?;

                let _ = self.manager.get_browser(&profile).await?;
                let page = self.manager.get_page(&profile, target_id).await?;

                let stealth_opts = stealth::StealthOptions::from_args(&args);
                let _ = stealth::apply_network_overrides(&page, &stealth_opts).await;
                let _ = stealth::install_stealth_on_new_document(&page, &stealth_opts).await;

                // `goto` gets the caller's budget too, not just the settle
                // wait below. Unbounded, it falls through to the CDP client's
                // own request timeout, so `timeout_ms` silently did not bound
                // the navigation it names -- a server that accepts the
                // connection and never answers cost 30s per attempt, and the
                // runner then retried it.
                let timeout_ms = args["timeout_ms"].as_u64().unwrap_or(30_000);
                match tokio::time::timeout(
                    std::time::Duration::from_millis(timeout_ms),
                    page.goto(url),
                )
                .await
                {
                    Ok(r) => r.map_err(|e| {
                        Error::ToolExecution(format!("navigation failed: {e}").into())
                    })?,
                    Err(_) => {
                        return Err(Error::ToolExecution(
                            format!(
                                "navigation to '{url}' did not complete within {timeout_ms}ms. \
                                 The browser is alive and accepted the request, so this is the \
                                 page or the server it talks to, not the browser: a server that \
                                 accepts the connection and never responds looks exactly like \
                                 this. Retrying the same URL will usually fail the same way."
                            )
                            .into(),
                        ));
                    }
                };
                let _ = tokio::time::timeout(
                    std::time::Duration::from_millis(timeout_ms),
                    page.wait_for_navigation(),
                )
                .await;

                let _ = stealth::apply_stealth(&page, &stealth_opts).await;

                let mut wait_results = serde_json::Map::new();
                if let Some(sel) = args["wait_selector"].as_str() {
                    let state = stealth::WaitState::parse(
                        args["wait_selector_state"].as_str().unwrap_or("visible"),
                    );
                    let ok = stealth::wait_for_selector(&page, sel, state, timeout_ms).await?;
                    wait_results.insert("wait_selector".into(), Value::Bool(ok));
                }
                if args["network_idle"].as_bool().unwrap_or(true) {
                    let ok = stealth::wait_for_network_idle(&page, 500, timeout_ms).await?;
                    wait_results.insert("network_idle".into(), Value::Bool(ok));
                }
                if args["solve_cloudflare"].as_bool().unwrap_or(false) {
                    let ok = stealth::solve_cloudflare(&page, timeout_ms).await?;
                    wait_results.insert("cloudflare_clear".into(), Value::Bool(ok));
                }
                if let Some(delay) = args["delay_ms"].as_u64() {
                    tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                }

                let final_url = page.url().await.ok().flatten().unwrap_or_default();
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
                    "text": truncated_text,
                    "text_truncated": text_truncated,
                    "body": truncated_html,
                    "body_truncated": html_truncated,
                    "cookies": cookie_map,
                    "waits": Value::Object(wait_results),
                    "profile": profile,
                }))
            }

            // ── Scrapling.Selector ─────────────────────────────────
            "select" => {
                let params = selectors::SelectParams::from_args(&args);

                let mut matches = if let Some(html) = &params.html {
                    selectors::select_static(html, &params)?
                } else {
                    let _ = self.manager.get_browser(&profile).await?;
                    let page = self.manager.get_page(&profile, target_id).await?;
                    selectors::select_live(&page, &params).await?
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
                            let page = self.manager.get_page(&profile, target_id).await?;
                            selectors::select_live(&page, &pool_params)
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
                }
                Ok(value)
            }

            // ── Wait helper ────────────────────────────────────────
            "wait_for" => {
                let _ = self.manager.get_browser(&profile).await?;
                let page = self.manager.get_page(&profile, target_id).await?;
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

                Ok(Value::Object(results))
            }

            _ => Err(Error::ToolExecution(
                format!(
                    "unknown browser action: '{action}'. Available: \
                     status, start, stop, profiles, tabs, open, close, focus, \
                     navigate, snapshot, act, screenshot, content, evaluate, \
                     scroll, console, cookies, pdf, fetch, stealth_fetch, select, wait_for"
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

    #[test]
    fn store_key_separates_tabs() {
        assert_eq!(
            BrowserTool::store_key("default", Some("TARGET-1")),
            "default:TARGET-1"
        );
        assert_ne!(
            BrowserTool::store_key("default", Some("TARGET-1")),
            BrowserTool::store_key("default", Some("TARGET-2"))
        );
        assert_eq!(BrowserTool::store_key("default", None), "default:active");
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
