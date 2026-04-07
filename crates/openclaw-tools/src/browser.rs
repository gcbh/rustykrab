use async_trait::async_trait;
use base64::Engine;
use chromiumoxide::cdp::browser_protocol::network::Cookie;
use chromiumoxide::page::ScreenshotParams;
use chromiumoxide::Browser;
use openclaw_core::types::ToolSchema;
use openclaw_core::{Error, Result, Tool};
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio_stream::StreamExt;

use crate::security;

const DEFAULT_CDP_URL: &str = "ws://127.0.0.1:9222";
const MAX_CONTENT_BYTES: usize = 50 * 1024; // 50KB cap for page content

/// Browser automation tool using Chrome DevTools Protocol.
///
/// Connects to a running Chrome instance via CDP for full browser automation
/// including navigation, form filling, screenshots, and cookie persistence.
/// The user's existing Chrome profile is used, so all logged-in sessions are available.
///
/// Launch Chrome with remote debugging:
/// ```sh
/// open -a 'Google Chrome' --args --remote-debugging-port=9222
/// ```
///
/// Configure the CDP URL via `CHROME_CDP_URL` env var (default: ws://127.0.0.1:9222).
pub struct BrowserTool {
    cdp_url: String,
    /// Lazy connection: created on first use and held for the tool's lifetime.
    /// Stores (Browser, JoinHandle) — the JoinHandle keeps the CDP handler task alive.
    state: Arc<Mutex<Option<BrowserState>>>,
}

struct BrowserState {
    browser: Browser,
    _handler_task: tokio::task::JoinHandle<()>,
}

impl BrowserTool {
    pub fn new() -> Self {
        let cdp_url = std::env::var("CHROME_CDP_URL")
            .unwrap_or_else(|_| DEFAULT_CDP_URL.to_string());
        Self {
            cdp_url,
            state: Arc::new(Mutex::new(None)),
        }
    }

    /// Get or create the browser connection.
    async fn get_browser(&self) -> Result<Arc<Mutex<Option<BrowserState>>>> {
        let mut guard = self.state.lock().await;
        if guard.is_none() {
            let (browser, mut handler) =
                Browser::connect(&self.cdp_url).await.map_err(|e| {
                    Error::ToolExecution(format!(
                        "Chrome not reachable at {}: {}. \
                         Launch Chrome with: open -a 'Google Chrome' --args --remote-debugging-port=9222",
                        self.cdp_url, e
                    ).into())
                })?;

            // Spawn the CDP handler so messages are processed in the background.
            let handler_task = tokio::spawn(async move {
                while let Some(_event) = handler.next().await {}
            });

            *guard = Some(BrowserState {
                browser,
                _handler_task: handler_task,
            });
        }
        drop(guard);
        Ok(Arc::clone(&self.state))
    }
}

impl Default for BrowserTool {
    fn default() -> Self {
        Self::new()
    }
}

/// Helper to get or create a page. Reuses the first existing page if available,
/// otherwise creates a new one.
async fn get_active_page(browser: &Browser) -> Result<chromiumoxide::Page> {
    // Try to get existing pages first
    let pages = browser.pages().await.map_err(|e| {
        Error::ToolExecution(format!("failed to list browser pages: {e}").into())
    })?;
    if let Some(page) = pages.into_iter().next() {
        return Ok(page);
    }
    // No pages — create one
    browser.new_page("about:blank").await.map_err(|e| {
        Error::ToolExecution(format!("failed to create new page: {e}").into())
    })
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
        "Control a Chrome browser via DevTools Protocol. Can navigate pages, \
         fill forms, click elements, take screenshots, read content, and execute JavaScript. \
         Connects to Chrome on localhost:9222. Supports login flows \u{2014} cookies persist \
         across calls."
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
                        "enum": ["navigate", "click", "type", "screenshot", "content", "evaluate", "wait", "select", "cookies", "scroll"],
                        "description": "Action to perform"
                    },
                    "url": { "type": "string", "description": "URL to navigate to (navigate action)" },
                    "selector": { "type": "string", "description": "CSS selector for element interaction" },
                    "text": { "type": "string", "description": "Text to type (type action)" },
                    "clear": { "type": "boolean", "description": "Clear field before typing (default false)" },
                    "expression": { "type": "string", "description": "JavaScript to evaluate" },
                    "format": { "type": "string", "enum": ["text", "html"], "description": "Content format (default text)" },
                    "full_page": { "type": "boolean", "description": "Full page screenshot (default false)" },
                    "direction": { "type": "string", "enum": ["down", "up", "bottom", "top"], "description": "Scroll direction" },
                    "amount": { "type": "integer", "description": "Scroll amount in pixels (default 500)" },
                    "value": { "type": "string", "description": "Value to select (select action)" },
                    "domain": { "type": "string", "description": "Cookie domain filter (cookies action)" },
                    "timeout_ms": { "type": "integer", "description": "Timeout in milliseconds (wait action, default 10000)" }
                },
                "required": ["action"]
            }),
        }
    }

    async fn execute(&self, args: Value) -> Result<Value> {
        let action = args["action"]
            .as_str()
            .ok_or_else(|| Error::ToolExecution("missing 'action' parameter".into()))?;

        // Connect to Chrome (lazy — first call creates the connection)
        let state_arc = self.get_browser().await?;
        let state_guard = state_arc.lock().await;
        let browser_state = state_guard.as_ref().ok_or_else(|| {
            Error::ToolExecution("browser connection not initialized".into())
        })?;
        let browser = &browser_state.browser;

        match action {
            "navigate" => {
                let url = args["url"]
                    .as_str()
                    .ok_or_else(|| Error::ToolExecution("navigate action requires 'url' parameter".into()))?;

                // SSRF protection: validate URL before navigating
                security::validate_url(url)
                    .map_err(|e| Error::ToolExecution(e.into()))?;

                let page = get_active_page(browser).await?;
                page.goto(url).await.map_err(|e| {
                    Error::ToolExecution(format!("navigation failed: {e}").into())
                })?;

                // Wait briefly for the page to settle
                let _ = page.wait_for_navigation().await;

                let title = page
                    .get_title()
                    .await
                    .map_err(|e| Error::ToolExecution(format!("failed to get title: {e}").into()))?
                    .unwrap_or_default();

                let current_url = page
                    .url()
                    .await
                    .map_err(|e| Error::ToolExecution(format!("failed to get URL: {e}").into()))?
                    .unwrap_or_default();

                Ok(json!({
                    "title": title,
                    "url": current_url,
                    "status": "loaded"
                }))
            }

            "click" => {
                let selector = args["selector"]
                    .as_str()
                    .ok_or_else(|| Error::ToolExecution("click action requires 'selector' parameter".into()))?;

                let page = get_active_page(browser).await?;
                let elem = page.find_element(selector).await.map_err(|e| {
                    Error::ToolExecution(format!("element not found for selector '{selector}': {e}").into())
                })?;
                elem.click().await.map_err(|e| {
                    Error::ToolExecution(format!("click failed on '{selector}': {e}").into())
                })?;

                Ok(json!({
                    "status": "clicked",
                    "selector": selector
                }))
            }

            "type" => {
                let selector = args["selector"]
                    .as_str()
                    .ok_or_else(|| Error::ToolExecution("type action requires 'selector' parameter".into()))?;
                let text = args["text"]
                    .as_str()
                    .ok_or_else(|| Error::ToolExecution("type action requires 'text' parameter".into()))?;
                let clear = args["clear"].as_bool().unwrap_or(false);

                let page = get_active_page(browser).await?;
                let elem = page.find_element(selector).await.map_err(|e| {
                    Error::ToolExecution(format!("element not found for selector '{selector}': {e}").into())
                })?;

                // Click to focus the field
                elem.click().await.map_err(|e| {
                    Error::ToolExecution(format!("failed to focus '{selector}': {e}").into())
                })?;

                if clear {
                    // Select all existing content then replace with new text
                    page.evaluate(format!(
                        "document.querySelector('{}').select && document.querySelector('{}').select()",
                        selector.replace('\'', "\\'"),
                        selector.replace('\'', "\\'"),
                    ))
                    .await
                    .ok();
                    // Also try Ctrl+A via JS for input fields
                    page.evaluate(format!(
                        "var el = document.querySelector('{}'); if(el) {{ el.value = ''; }}",
                        selector.replace('\'', "\\'"),
                    ))
                    .await
                    .ok();
                }

                elem.type_str(text).await.map_err(|e| {
                    Error::ToolExecution(format!("typing failed on '{selector}': {e}").into())
                })?;

                Ok(json!({
                    "status": "typed",
                    "selector": selector,
                    "length": text.len()
                }))
            }

            "screenshot" => {
                let full_page = args["full_page"].as_bool().unwrap_or(false);
                let selector = args["selector"].as_str();

                let page = get_active_page(browser).await?;

                let png_bytes = if let Some(sel) = selector {
                    // Element screenshot
                    let elem = page.find_element(sel).await.map_err(|e| {
                        Error::ToolExecution(format!(
                            "element not found for selector '{sel}': {e}"
                        ).into())
                    })?;
                    elem.screenshot(
                        chromiumoxide::cdp::browser_protocol::page::CaptureScreenshotFormat::Png,
                    )
                    .await
                    .map_err(|e| {
                        Error::ToolExecution(format!("element screenshot failed: {e}").into())
                    })?
                } else {
                    // Page screenshot
                    let params = ScreenshotParams::builder()
                        .full_page(full_page)
                        .build();
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
                    "encoding": "base64"
                }))
            }

            "content" => {
                let format = args["format"].as_str().unwrap_or("text");
                let page = get_active_page(browser).await?;

                let content = match format {
                    "html" => {
                        page.content().await.map_err(|e| {
                            Error::ToolExecution(format!("failed to get page HTML: {e}").into())
                        })?
                    }
                    _ => {
                        // Extract text content via JS for context-window efficiency
                        let result = page
                            .evaluate("document.body.innerText")
                            .await
                            .map_err(|e| {
                                Error::ToolExecution(format!("failed to get page text: {e}").into())
                            })?;
                        result.into_value::<String>().unwrap_or_default()
                    }
                };

                let (truncated_content, was_truncated) =
                    truncate_utf8(&content, MAX_CONTENT_BYTES);

                let title = page
                    .get_title()
                    .await
                    .map_err(|e| Error::ToolExecution(format!("failed to get title: {e}").into()))?
                    .unwrap_or_default();

                let current_url = page
                    .url()
                    .await
                    .map_err(|e| Error::ToolExecution(format!("failed to get URL: {e}").into()))?
                    .unwrap_or_default();

                Ok(json!({
                    "content": truncated_content,
                    "url": current_url,
                    "title": title,
                    "format": format,
                    "truncated": was_truncated
                }))
            }

            "evaluate" => {
                let expression = args["expression"]
                    .as_str()
                    .ok_or_else(|| Error::ToolExecution("evaluate action requires 'expression' parameter".into()))?;

                let page = get_active_page(browser).await?;
                let result = page.evaluate(expression).await.map_err(|e| {
                    Error::ToolExecution(format!("JS evaluation failed: {e}").into())
                })?;

                // Try to deserialize as a JSON value; fall back to string representation
                let value: Value = result
                    .into_value()
                    .unwrap_or(Value::Null);

                Ok(json!({
                    "result": value
                }))
            }

            "wait" => {
                let selector = args["selector"].as_str();
                let timeout_ms = args["timeout_ms"].as_u64().unwrap_or(10_000);

                let page = get_active_page(browser).await?;

                if let Some(sel) = selector {
                    // Poll for element with timeout
                    let deadline =
                        tokio::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
                    loop {
                        match page.find_element(sel).await {
                            Ok(_) => {
                                return Ok(json!({
                                    "status": "found",
                                    "selector": sel
                                }));
                            }
                            Err(_) => {
                                if tokio::time::Instant::now() >= deadline {
                                    return Ok(json!({
                                        "status": "timeout",
                                        "selector": sel,
                                        "timeout_ms": timeout_ms
                                    }));
                                }
                                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                            }
                        }
                    }
                } else {
                    // Wait for navigation to complete
                    match tokio::time::timeout(
                        std::time::Duration::from_millis(timeout_ms),
                        page.wait_for_navigation(),
                    )
                    .await
                    {
                        Ok(Ok(_)) => Ok(json!({
                            "status": "navigation_complete"
                        })),
                        Ok(Err(e)) => Ok(json!({
                            "status": "navigation_error",
                            "error": format!("{e}")
                        })),
                        Err(_) => Ok(json!({
                            "status": "timeout",
                            "timeout_ms": timeout_ms
                        })),
                    }
                }
            }

            "select" => {
                let selector = args["selector"]
                    .as_str()
                    .ok_or_else(|| Error::ToolExecution("select action requires 'selector' parameter".into()))?;
                let value = args["value"]
                    .as_str()
                    .ok_or_else(|| Error::ToolExecution("select action requires 'value' parameter".into()))?;

                let page = get_active_page(browser).await?;

                // Use JS to set the select value and fire change event
                let js = format!(
                    r#"(function() {{
                        var el = document.querySelector('{}');
                        if (!el) return 'element_not_found';
                        el.value = '{}';
                        el.dispatchEvent(new Event('change', {{ bubbles: true }}));
                        return 'selected';
                    }})()"#,
                    selector.replace('\'', "\\'").replace('"', "\\\""),
                    value.replace('\'', "\\'").replace('"', "\\\""),
                );

                let result = page.evaluate(js).await.map_err(|e| {
                    Error::ToolExecution(format!("select failed: {e}").into())
                })?;

                let status: String = result.into_value().unwrap_or_else(|_| "unknown".into());

                if status == "element_not_found" {
                    return Err(Error::ToolExecution(format!(
                        "element not found for selector '{selector}'"
                    ).into()));
                }

                Ok(json!({
                    "status": "selected",
                    "selector": selector,
                    "value": value
                }))
            }

            "cookies" => {
                let domain_filter = args["domain"].as_str();
                let page = get_active_page(browser).await?;

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
                    "count": filtered.len()
                }))
            }

            "scroll" => {
                let direction = args["direction"].as_str().unwrap_or("down");
                let amount = args["amount"].as_i64().unwrap_or(500);

                let page = get_active_page(browser).await?;

                let js = match direction {
                    "down" => format!("window.scrollBy(0, {amount}); window.scrollY"),
                    "up" => format!("window.scrollBy(0, -{amount}); window.scrollY"),
                    "bottom" => "window.scrollTo(0, document.body.scrollHeight); window.scrollY".to_string(),
                    "top" => "window.scrollTo(0, 0); window.scrollY".to_string(),
                    _ => {
                        return Err(Error::ToolExecution(format!(
                            "unknown scroll direction: '{direction}'. Use: down, up, bottom, top"
                        ).into()));
                    }
                };

                let result = page.evaluate(js).await.map_err(|e| {
                    Error::ToolExecution(format!("scroll failed: {e}").into())
                })?;

                let scroll_y: f64 = result.into_value().unwrap_or(0.0);

                Ok(json!({
                    "status": "scrolled",
                    "direction": direction,
                    "scroll_y": scroll_y as i64
                }))
            }

            _ => Err(Error::ToolExecution(format!(
                "unknown browser action: '{action}'. Available actions: \
                 navigate, click, type, screenshot, content, evaluate, wait, select, cookies, scroll"
            ).into())),
        }
    }
}
