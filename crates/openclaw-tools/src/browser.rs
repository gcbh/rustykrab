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
///
/// If Chrome is not already running with remote debugging, the tool will
/// auto-launch it. Modern Chrome requires a non-default `--user-data-dir`
/// for remote debugging, so the tool creates a wrapper directory at
/// `~/.openclaw/chrome-profile` that symlinks back to the user's real
/// Chrome profile — all existing sessions, cookies, and logins are
/// available without re-authenticating. The user's active profile is
/// detected from Chrome's `Local State` file.
///
/// Configure the CDP URL via `CHROME_CDP_URL` env var (default: ws://127.0.0.1:9222).
pub struct BrowserTool {
    cdp_url: String,
    user_data_dir: std::path::PathBuf,
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
        let home = std::env::var("HOME")
            .or_else(|_| std::env::var("USERPROFILE"))
            .unwrap_or_else(|_| ".".to_string());
        let user_data_dir = std::path::PathBuf::from(home)
            .join(".openclaw")
            .join("chrome-profile");
        Self {
            cdp_url,
            user_data_dir,
            state: Arc::new(Mutex::new(None)),
        }
    }

    /// Get or create the browser connection.
    ///
    /// Tries to connect to an existing Chrome instance. If that fails,
    /// attempts to auto-launch Chrome with remote debugging enabled,
    /// then retries the connection.
    async fn get_browser(&self) -> Result<Arc<Mutex<Option<BrowserState>>>> {
        let mut guard = self.state.lock().await;
        if guard.is_none() {
            let connect_result = Browser::connect(&self.cdp_url).await;

            let (browser, handler) = match connect_result {
                Ok(pair) => pair,
                Err(_) => {
                    // Chrome isn't running with debugging — try to launch it.
                    tracing::info!("Chrome not reachable, attempting auto-launch with remote debugging");
                    self.launch_chrome()?;

                    // Give Chrome time to start and open the debugging port.
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;

                    Browser::connect(&self.cdp_url).await.map_err(|e| {
                        Error::ToolExecution(format!(
                            "Chrome not reachable at {} after auto-launch attempt: {}. \
                             If Chrome is already running without remote debugging, \
                             quit it first so a new instance can start with the debugging port.",
                            self.cdp_url, e
                        ).into())
                    })?
                }
            };

            // Spawn the CDP handler so messages are processed in the background.
            let mut handler = handler;
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

    /// Detect the platform-specific Chrome data directory.
    fn chrome_data_dir() -> Option<std::path::PathBuf> {
        let home = std::env::var("HOME").ok()?;
        #[cfg(target_os = "macos")]
        {
            Some(std::path::PathBuf::from(home).join("Library/Application Support/Google/Chrome"))
        }
        #[cfg(target_os = "linux")]
        {
            Some(std::path::PathBuf::from(home).join(".config/google-chrome"))
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        {
            None
        }
    }

    /// Read Chrome's `Local State` to find the last-used profile directory
    /// name (e.g. "Default", "Profile 4"). Falls back to "Default".
    fn detect_profile_name(chrome_dir: &std::path::Path) -> String {
        let local_state_path = chrome_dir.join("Local State");
        if let Ok(data) = std::fs::read_to_string(&local_state_path) {
            if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&data) {
                if let Some(name) = parsed["profile"]["last_used"].as_str() {
                    if chrome_dir.join(name).exists() {
                        return name.to_string();
                    }
                }
            }
        }
        "Default".to_string()
    }

    /// Set up the wrapper data directory so Chrome uses the user's real
    /// profile without showing a profile picker.
    ///
    /// 1. Reads `Local State` from the real Chrome dir to find the active
    ///    profile name (e.g. "Profile 4").
    /// 2. Symlinks that profile folder into our wrapper directory.
    /// 3. Writes a minimal `Local State` with `picker_shown: false`.
    ///
    /// Returns the profile directory name to pass as `--profile-directory`.
    /// Best-effort: if anything fails, returns "Default" and Chrome starts
    /// with a fresh profile.
    fn setup_profile_link(&self) -> String {
        let Some(chrome_dir) = Self::chrome_data_dir() else {
            return "Default".to_string();
        };

        let profile_name = Self::detect_profile_name(&chrome_dir);
        let real_profile = chrome_dir.join(&profile_name);
        let link_path = self.user_data_dir.join(&profile_name);

        // Symlink the profile directory (only if it doesn't already exist).
        if real_profile.exists() && !link_path.exists() {
            #[cfg(unix)]
            {
                if let Err(e) = std::os::unix::fs::symlink(&real_profile, &link_path) {
                    tracing::warn!("could not symlink Chrome profile: {e}");
                }
            }
        }

        // Write a minimal Local State that points to our profile and
        // disables the profile picker.
        let local_state_dest = self.user_data_dir.join("Local State");
        let local_state = serde_json::json!({
            "profile": {
                "last_used": &profile_name,
                "last_active_profiles": [&profile_name],
                "picker_shown": false
            }
        });
        if let Err(e) = std::fs::write(&local_state_dest, local_state.to_string()) {
            tracing::warn!("could not write Chrome Local State: {e}");
        }

        profile_name
    }

    /// Launch Chrome with remote debugging and the user's real profile.
    ///
    /// Modern Chrome requires a non-default `--user-data-dir` for remote
    /// debugging. We create a wrapper dir, symlink the user's actual
    /// profile into it, write a `Local State` that disables the profile
    /// picker, and pass `about:blank` so Chrome opens directly.
    fn launch_chrome(&self) -> Result<()> {
        let port = self.cdp_url
            .rsplit(':')
            .next()
            .and_then(|p| p.trim_end_matches('/').parse::<u16>().ok())
            .unwrap_or(9222);

        std::fs::create_dir_all(&self.user_data_dir).map_err(|e| {
            Error::ToolExecution(format!("failed to create Chrome profile dir: {e}").into())
        })?;

        let profile_name = self.setup_profile_link();

        let args = [
            format!("--remote-debugging-port={port}"),
            format!("--user-data-dir={}", self.user_data_dir.display()),
            format!("--profile-directory={profile_name}"),
            "--no-first-run".to_string(),
            "about:blank".to_string(),
        ];

        #[cfg(target_os = "macos")]
        {
            let chrome_path = "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome";
            std::process::Command::new(chrome_path)
                .args(&args)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .map_err(|e| {
                    Error::ToolExecution(format!("failed to launch Chrome: {e}").into())
                })?;
        }

        #[cfg(target_os = "linux")]
        {
            let browsers = ["google-chrome", "google-chrome-stable", "chromium-browser", "chromium"];
            let launched = browsers.iter().any(|name| {
                std::process::Command::new(name)
                    .args(&args)
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn()
                    .is_ok()
            });
            if !launched {
                return Err(Error::ToolExecution(
                    "could not find Chrome or Chromium. Install Google Chrome or set CHROME_CDP_URL.".into(),
                ));
            }
        }

        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        {
            return Err(Error::ToolExecution(format!(
                "auto-launch not supported on this platform. \
                 Launch Chrome manually with: --remote-debugging-port={port}"
            ).into()));
        }

        tracing::info!(port, %profile_name, "launched Chrome with remote debugging");
        Ok(())
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
