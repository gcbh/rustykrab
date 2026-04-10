//! Browser automation tool modeled after OpenClaw's browser management.
//!
//! Provides a comprehensive browser control surface with:
//! - Multi-profile browser management (isolated Chrome instances)
//! - Browser lifecycle (status/start/stop)
//! - Tab management (tabs/open/close/focus) with targetId addressing
//! - Accessibility-tree snapshots with element refs
//! - Ref-based actions (click/type/press/hover/select/drag via snapshot refs)
//! - Screenshot, navigate, evaluate, console, PDF, scroll
//! - SSRF protection and cookie security

pub mod actions;
pub mod config;
pub mod manager;
pub mod snapshot;

use async_trait::async_trait;
use base64::Engine;
use chromiumoxide::cdp::browser_protocol::network::Cookie;
use chromiumoxide::page::ScreenshotParams;
use rustykrab_core::types::ToolSchema;
use rustykrab_core::{Error, Result, Tool};
use serde_json::{json, Value};

use crate::security;
use manager::BrowserManager;
use snapshot::{SnapshotMode, SnapshotOptions, SnapshotStore};

const MAX_CONTENT_BYTES: usize = 50 * 1024; // 50KB cap for page content

/// Browser automation tool using Chrome DevTools Protocol.
///
/// Modeled after OpenClaw's browser management architecture:
/// - Multiple named browser profiles, each an isolated Chrome instance
/// - Browser lifecycle management (status/start/stop)
/// - Deterministic tab control (tabs/open/close/focus by targetId)
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
}

impl BrowserTool {
    pub fn new() -> Self {
        Self {
            manager: BrowserManager::from_config(),
            snapshot_store: SnapshotStore::new(),
        }
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

/// Mask a cookie value for security: show first 8 chars + "..."
fn mask_cookie_value(value: &str) -> String {
    if value.len() <= 8 {
        value.to_string()
    } else {
        format!("{}...", &value[..8])
    }
}

#[async_trait]
impl Tool for BrowserTool {
    fn name(&self) -> &str {
        "browser"
    }

    fn description(&self) -> &str {
        "Control Chrome browsers via DevTools Protocol. Supports multiple isolated browser \
         profiles, each with its own CDP port and user-data directory. Actions: \
         status — check browser state; \
         start — launch a browser profile; \
         stop — terminate a browser profile; \
         profiles — list all configured profiles; \
         tabs — list open tabs (with targetId for addressing); \
         open — open a URL in a new tab; \
         close — close a tab by targetId; \
         focus — bring a tab to front by targetId; \
         navigate — go to URL in active tab; \
         snapshot — take accessibility-tree snapshot with element refs; \
         act — interact with elements by ref from snapshot (click/type/press/hover/select/drag); \
         screenshot — capture page or element as PNG; \
         content — extract page text or HTML; \
         evaluate — run JavaScript; \
         scroll — scroll the page; \
         console — get browser console logs; \
         cookies — list cookies; \
         pdf — export page as PDF. \
         Cookies persist across calls. Use snapshot + act for reliable element interaction."
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: self.name().to_string(),
            description: self.description().to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": [
                            "status", "start", "stop", "profiles",
                            "tabs", "open", "close", "focus",
                            "navigate", "snapshot", "act", "screenshot",
                            "content", "evaluate", "scroll",
                            "console", "cookies", "pdf"
                        ],
                        "description": "Action to perform"
                    },
                    "profile": {
                        "type": "string",
                        "description": "Browser profile name (default: configured default profile)"
                    },
                    "url": {
                        "type": "string",
                        "description": "URL to navigate to or open (navigate/open actions)"
                    },
                    "targetId": {
                        "type": "string",
                        "description": "Tab identifier from 'tabs' action (e.g., 'tab_0'). Used by close/focus/navigate/snapshot/act/screenshot/content/evaluate"
                    },
                    "ref": {
                        "type": "string",
                        "description": "Element ref from a snapshot (e.g., '12' or 'e12'). Used by 'act' action"
                    },
                    "actAction": {
                        "type": "string",
                        "enum": ["click", "type", "fill", "press", "hover", "select", "drag", "wait"],
                        "description": "Sub-action for 'act' (e.g., click, type, press). Requires 'ref' from snapshot"
                    },
                    "text": {
                        "type": "string",
                        "description": "Text to type (act type/fill action)"
                    },
                    "key": {
                        "type": "string",
                        "description": "Key to press (act press action, e.g., 'Enter', 'Tab', 'Escape')"
                    },
                    "value": {
                        "type": "string",
                        "description": "Value to select (act select action)"
                    },
                    "targetRef": {
                        "type": "string",
                        "description": "Target element ref for drag action"
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
                        "description": "JavaScript to evaluate (evaluate action)"
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
                        "description": "Snapshot: max tree depth (default: 10)"
                    }
                },
                "required": ["action"]
            }),
        }
    }

    async fn execute(&self, args: Value) -> Result<Value> {
        let action = args["action"]
            .as_str()
            .ok_or_else(|| Error::ToolExecution("missing 'action' parameter".into()))?;

        if !self.manager.config().enabled {
            return Err(Error::ToolExecution(
                "browser subsystem is disabled. Set browser.enabled=true in config.".into(),
            ));
        }

        let profile = self.resolve_profile(&args).to_string();
        let target_id = args["targetId"].as_str();

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
                let url = args["url"]
                    .as_str()
                    .ok_or_else(|| Error::ToolExecution("'open' requires 'url' parameter".into()))?;
                security::validate_url(url).await.map_err(|e| Error::ToolExecution(e.into()))?;
                let _ = self.manager.get_browser(&profile).await?;
                self.manager.open_tab(&profile, url).await
            }

            "close" => {
                let tid = target_id.ok_or_else(|| {
                    Error::ToolExecution("'close' requires 'targetId' parameter".into())
                })?;
                self.manager.close_tab(&profile, tid).await
            }

            "focus" => {
                let tid = target_id.ok_or_else(|| {
                    Error::ToolExecution("'focus' requires 'targetId' parameter".into())
                })?;
                self.manager.focus_tab(&profile, tid).await
            }

            // ── Navigation ─────────────────────────────────────────
            "navigate" => {
                let url = args["url"]
                    .as_str()
                    .ok_or_else(|| Error::ToolExecution("'navigate' requires 'url' parameter".into()))?;
                security::validate_url(url).await.map_err(|e| Error::ToolExecution(e.into()))?;

                let _ = self.manager.get_browser(&profile).await?;
                let page = self.manager.get_page(&profile, target_id).await?;
                page.goto(url).await.map_err(|e| {
                    Error::ToolExecution(format!("navigation failed: {e}").into())
                })?;

                let timeout_ms = args["timeout_ms"].as_u64().unwrap_or(10_000);
                let _ = tokio::time::timeout(
                    std::time::Duration::from_millis(timeout_ms),
                    page.wait_for_navigation(),
                )
                .await;

                let title = page.get_title().await.ok().flatten().unwrap_or_default();
                let current_url = page.url().await.ok().flatten().unwrap_or_default();

                Ok(json!({
                    "title": title,
                    "url": current_url,
                    "status": "loaded",
                    "profile": profile
                }))
            }

            // ── Snapshot ───────────────────────────────────────────
            "snapshot" => {
                let _ = self.manager.get_browser(&profile).await?;
                let page = self.manager.get_page(&profile, target_id).await?;

                let mode = match args["format"].as_str() {
                    Some("aria") => SnapshotMode::Aria,
                    _ => SnapshotMode::Ai,
                };

                let options = SnapshotOptions {
                    mode,
                    interactive_only: args["interactive"].as_bool().unwrap_or(false),
                    compact: args["compact"].as_bool().unwrap_or(false),
                    max_depth: args["depth"].as_u64().unwrap_or(10) as usize,
                    selector: args["selector"].as_str().map(|s| s.to_string()),
                };

                let key = Self::store_key(&profile, target_id);
                snapshot::take_snapshot(&page, &options, &self.snapshot_store, &key).await
            }

            // ── Act (ref-based actions) ────────────────────────────
            "act" => {
                let ref_id = args["ref"]
                    .as_str()
                    .ok_or_else(|| Error::ToolExecution(
                        "'act' requires 'ref' parameter from a previous snapshot".into(),
                    ))?;
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

            // ── Screenshot ─────────────────────────────────────────
            "screenshot" => {
                let _ = self.manager.get_browser(&profile).await?;
                let page = self.manager.get_page(&profile, target_id).await?;
                let full_page = args["full_page"].as_bool().unwrap_or(false);
                let selector = args["selector"].as_str();

                let png_bytes = if let Some(sel) = selector {
                    let elem = page.find_element(sel).await.map_err(|e| {
                        Error::ToolExecution(
                            format!("element not found for selector '{sel}': {e}").into(),
                        )
                    })?;
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
                let page = self.manager.get_page(&profile, target_id).await?;
                let format = args["format"].as_str().unwrap_or("text");

                let content = match format {
                    "html" => page.content().await.map_err(|e| {
                        Error::ToolExecution(format!("failed to get page HTML: {e}").into())
                    })?,
                    _ => {
                        let result = page
                            .evaluate("document.body.innerText")
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
                let title = page.get_title().await.ok().flatten().unwrap_or_default();
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

                let expression = args["expression"]
                    .as_str()
                    .ok_or_else(|| Error::ToolExecution(
                        "'evaluate' requires 'expression' parameter".into(),
                    ))?;

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
                        "window.scrollTo(0, document.body.scrollHeight); window.scrollY"
                            .to_string()
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

                let result = page.evaluate(js).await.map_err(|e| {
                    Error::ToolExecution(format!("scroll failed: {e}").into())
                })?;

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
                let page = self.manager.get_page(&profile, target_id).await?;

                let pdf_bytes = page.pdf(Default::default()).await.map_err(|e| {
                    Error::ToolExecution(
                        format!("PDF generation failed: {e}. Note: PDF requires headless mode.")
                            .into(),
                    )
                })?;

                let size_bytes = pdf_bytes.len();
                let b64 = base64::engine::general_purpose::STANDARD.encode(&pdf_bytes);

                let url = page.url().await.ok().flatten().unwrap_or_default();
                let title = page.get_title().await.ok().flatten().unwrap_or_default();

                Ok(json!({
                    "pdf": b64,
                    "size_bytes": size_bytes,
                    "encoding": "base64",
                    "url": url,
                    "title": title,
                    "profile": profile
                }))
            }

            _ => Err(Error::ToolExecution(
                format!(
                    "unknown browser action: '{action}'. Available: \
                     status, start, stop, profiles, tabs, open, close, focus, \
                     navigate, snapshot, act, screenshot, content, evaluate, \
                     scroll, console, cookies, pdf"
                )
                .into(),
            )),
        }
    }
}
