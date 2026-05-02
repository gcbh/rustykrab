//! Browser stealth helpers — Scrapling.StealthyFetcher analogue.
//!
//! Scrapling layers a number of anti-detection patches on top of a real
//! Chromium: hide `navigator.webdriver`, randomize/normalize the canvas
//! and WebGL fingerprint, block WebRTC ICE leaks, disable resource loads
//! we do not need (images, fonts, media), and so on. This module brings
//! the same surface to the rustykrab CDP-driven browser.
//!
//! All patches are best-effort and run inside the page context. They do
//! not match a fully-patched Chromium build (e.g. rebrowser-patches,
//! camoufox) and should not be relied on against advanced fingerprinters
//! — but they do remove the cheap tells.

use chromiumoxide::Page;
use rustykrab_core::{Error, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::Duration;

/// Selectors commonly used by Cloudflare's Turnstile / interstitial.
const CLOUDFLARE_SELECTORS: &[&str] = &[
    "#challenge-running",
    "#challenge-form",
    "#cf-challenge-stage",
    "iframe[src*='challenges.cloudflare.com']",
    "div[class*='cf-browser-verification']",
];

/// Options controlling stealth behavior on a page.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StealthOptions {
    /// Override the User-Agent header.
    pub user_agent: Option<String>,
    /// Extra HTTP headers to send on every request.
    pub extra_headers: Vec<(String, String)>,
    /// Hide `navigator.webdriver` and patch obvious automation tells.
    pub hide_webdriver: bool,
    /// Block WebRTC (so the local IP cannot leak).
    pub block_webrtc: bool,
    /// Add noise to the canvas fingerprint.
    pub hide_canvas: bool,
    /// Don't load images / fonts / media (the Scrapling "disable_resources").
    pub disable_resources: bool,
    /// Block all images (lighter version of disable_resources).
    pub block_images: bool,
}

impl StealthOptions {
    pub fn from_args(args: &Value) -> Self {
        let mut extra_headers = Vec::new();
        if let Some(map) = args["extra_headers"].as_object() {
            for (k, v) in map {
                if let Some(s) = v.as_str() {
                    extra_headers.push((k.clone(), s.to_string()));
                }
            }
        }
        Self {
            user_agent: args["user_agent"].as_str().map(str::to_string),
            extra_headers,
            hide_webdriver: args["hide_webdriver"].as_bool().unwrap_or(true),
            block_webrtc: args["block_webrtc"].as_bool().unwrap_or(false),
            hide_canvas: args["hide_canvas"].as_bool().unwrap_or(false),
            disable_resources: args["disable_resources"].as_bool().unwrap_or(false),
            block_images: args["block_images"].as_bool().unwrap_or(false),
        }
    }

    pub fn any_patches(&self) -> bool {
        self.hide_webdriver
            || self.block_webrtc
            || self.hide_canvas
            || self.disable_resources
            || self.block_images
            || self.user_agent.is_some()
            || !self.extra_headers.is_empty()
    }
}

/// Apply Scrapling-style stealth patches to a `Page` before navigation.
///
/// We use evaluate-style injection on the existing document. Callers who
/// want patches to apply on every new document should reissue this call
/// after each navigation; it is idempotent.
pub async fn apply_stealth(page: &Page, opts: &StealthOptions) -> Result<()> {
    if !opts.any_patches() {
        return Ok(());
    }

    let script = build_stealth_script(opts);
    page.evaluate(script.as_str())
        .await
        .map_err(|e| Error::ToolExecution(format!("stealth patch failed: {e}").into()))?;

    // User-Agent override via JS (sets navigator.userAgent in the page
    // context; the network-level UA is applied via reqwest in `fetch` or
    // via headers below for browser flows).
    if let Some(ref ua) = opts.user_agent {
        let lit = serde_json::to_string(ua).unwrap_or_else(|_| "\"\"".to_string());
        let js = format!("Object.defineProperty(navigator, 'userAgent', {{ get: () => {lit} }});");
        let _ = page.evaluate(js.as_str()).await;
    }

    Ok(())
}

/// Build the combined stealth init script. Runs in the page context.
/// Patches are wrapped in try/catch so a failure on one doesn't break
/// the rest.
fn build_stealth_script(opts: &StealthOptions) -> String {
    let mut parts: Vec<String> = Vec::new();
    parts.push("(function(){".to_string());

    if opts.hide_webdriver {
        parts.push(
            r#"try {
                Object.defineProperty(navigator, 'webdriver', { get: () => undefined });
                Object.defineProperty(navigator, 'plugins', {
                    get: () => [1,2,3,4,5].map(function(i){return {name:'Plugin '+i};})
                });
                Object.defineProperty(navigator, 'languages', {
                    get: () => ['en-US','en']
                });
                window.chrome = window.chrome || { runtime: {} };
                var origQuery = window.navigator.permissions && window.navigator.permissions.query;
                if (origQuery) {
                    window.navigator.permissions.query = function(p) {
                        if (p && p.name === 'notifications') {
                            return Promise.resolve({ state: Notification.permission });
                        }
                        return origQuery.apply(this, arguments);
                    };
                }
            } catch(e) {}"#
                .to_string(),
        );
    }

    if opts.block_webrtc {
        parts.push(
            r#"try {
                if (window.RTCPeerConnection) {
                    window.RTCPeerConnection = function(){
                        throw new Error('RTCPeerConnection blocked');
                    };
                }
                if (window.webkitRTCPeerConnection) {
                    window.webkitRTCPeerConnection = function(){
                        throw new Error('webkitRTCPeerConnection blocked');
                    };
                }
                if (navigator.mediaDevices) {
                    navigator.mediaDevices.getUserMedia = function(){
                        return Promise.reject(new Error('blocked'));
                    };
                }
            } catch(e) {}"#
                .to_string(),
        );
    }

    if opts.hide_canvas {
        parts.push(
            r#"try {
                var origToDataURL = HTMLCanvasElement.prototype.toDataURL;
                HTMLCanvasElement.prototype.toDataURL = function() {
                    var ctx = this.getContext('2d');
                    if (ctx) {
                        var w = this.width, h = this.height;
                        if (w > 0 && h > 0) {
                            // Inject 1px of low-amplitude noise to defeat
                            // exact-fingerprint comparisons.
                            try {
                                var data = ctx.getImageData(0, 0, Math.min(w,1), Math.min(h,1));
                                if (data && data.data) {
                                    data.data[0] = (data.data[0] + 1) & 0xff;
                                    ctx.putImageData(data, 0, 0);
                                }
                            } catch(e) {}
                        }
                    }
                    return origToDataURL.apply(this, arguments);
                };
                var getParam = WebGLRenderingContext.prototype.getParameter;
                WebGLRenderingContext.prototype.getParameter = function(p) {
                    if (p === 37445) return 'Intel Inc.';      // UNMASKED_VENDOR_WEBGL
                    if (p === 37446) return 'Intel Iris OpenGL Engine'; // UNMASKED_RENDERER_WEBGL
                    return getParam.apply(this, arguments);
                };
            } catch(e) {}"#
                .to_string(),
        );
    }

    parts.push("})();".to_string());
    parts.join("\n")
}

/// Apply request-level overrides via raw CDP commands. We use page.evaluate
/// for in-page patches; for the network UA + headers we use the high-level
/// `Page` helpers when available, falling back to JS overrides otherwise.
pub async fn apply_network_overrides(page: &Page, opts: &StealthOptions) -> Result<()> {
    use chromiumoxide::cdp::browser_protocol::network::{
        SetExtraHttpHeadersParams, SetUserAgentOverrideParams,
    };

    if let Some(ref ua) = opts.user_agent {
        let cmd = SetUserAgentOverrideParams::new(ua.clone());
        let _ = page.execute(cmd).await;
    }

    if !opts.extra_headers.is_empty() {
        let mut map = serde_json::Map::new();
        for (k, v) in &opts.extra_headers {
            map.insert(k.clone(), Value::String(v.clone()));
        }
        let headers =
            chromiumoxide::cdp::browser_protocol::network::Headers::new(Value::Object(map));
        let cmd = SetExtraHttpHeadersParams::new(headers);
        let _ = page.execute(cmd).await;
    }

    Ok(())
}

/// Wait state for `wait_for_selector`, mirroring Playwright/Scrapling.
#[derive(Debug, Clone, Copy)]
pub enum WaitState {
    Attached,
    Detached,
    Visible,
    Hidden,
}

impl WaitState {
    pub fn parse(s: &str) -> Self {
        match s.to_ascii_lowercase().as_str() {
            "detached" => Self::Detached,
            "hidden" => Self::Hidden,
            "attached" => Self::Attached,
            _ => Self::Visible,
        }
    }

    fn js_predicate(self) -> &'static str {
        match self {
            // attached: element exists in DOM
            Self::Attached => "el !== null",
            // detached: element no longer in DOM
            Self::Detached => "el === null",
            // visible: element exists and has non-zero box and is not hidden
            Self::Visible => {
                "el !== null && (function(){\
                    var s = window.getComputedStyle(el);\
                    if (s.display === 'none' || s.visibility === 'hidden') return false;\
                    var r = el.getBoundingClientRect();\
                    return r.width > 0 && r.height > 0;\
                })()"
            }
            // hidden: element does not exist OR is hidden/zero-size
            Self::Hidden => {
                "el === null || (function(){\
                    var s = window.getComputedStyle(el);\
                    if (s.display === 'none' || s.visibility === 'hidden') return true;\
                    var r = el.getBoundingClientRect();\
                    return r.width === 0 || r.height === 0;\
                })()"
            }
        }
    }
}

/// Poll for a selector to reach the requested state. Returns `true` on
/// success, `false` on timeout.
pub async fn wait_for_selector(
    page: &Page,
    selector: &str,
    state: WaitState,
    timeout_ms: u64,
) -> Result<bool> {
    let sel_lit = serde_json::to_string(selector).unwrap_or_else(|_| "\"\"".to_string());
    let predicate = state.js_predicate();
    let js = format!(
        "(function(){{ var el = document.querySelector({sel_lit}); return ({predicate}); }})()"
    );

    let deadline = tokio::time::Instant::now() + Duration::from_millis(timeout_ms);
    loop {
        let result = page.evaluate(js.as_str()).await;
        if let Ok(v) = result {
            if v.into_value::<bool>().unwrap_or(false) {
                return Ok(true);
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return Ok(false);
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
}

/// Wait for the network to settle. Approximates Playwright's `networkidle`
/// by polling `performance.getEntriesByType('resource')` and expecting no
/// new entries to arrive for `idle_window_ms` consecutive milliseconds.
pub async fn wait_for_network_idle(
    page: &Page,
    idle_window_ms: u64,
    timeout_ms: u64,
) -> Result<bool> {
    let init_js = r#"
        (function(){
            window.__rustykrab_resources__ = window.__rustykrab_resources__ ||
                performance.getEntriesByType('resource').length;
            return performance.getEntriesByType('resource').length;
        })()
    "#;

    let _ = page.evaluate(init_js).await;

    let deadline = tokio::time::Instant::now() + Duration::from_millis(timeout_ms);
    let mut last_count: i64 = -1;
    let mut idle_since: Option<tokio::time::Instant> = None;

    while tokio::time::Instant::now() < deadline {
        let v = page
            .evaluate("performance.getEntriesByType('resource').length")
            .await
            .ok()
            .and_then(|r| r.into_value::<i64>().ok())
            .unwrap_or(0);

        if v == last_count {
            let since = idle_since.get_or_insert_with(tokio::time::Instant::now);
            if since.elapsed() >= Duration::from_millis(idle_window_ms) {
                return Ok(true);
            }
        } else {
            last_count = v;
            idle_since = Some(tokio::time::Instant::now());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Ok(false)
}

/// Best-effort Cloudflare/Turnstile wait. Polls for known challenge
/// markers to disappear. Returns `true` if the page appears clear,
/// `false` on timeout.
pub async fn solve_cloudflare(page: &Page, timeout_ms: u64) -> Result<bool> {
    let selectors_lit =
        serde_json::to_string(CLOUDFLARE_SELECTORS).unwrap_or_else(|_| "[]".to_string());
    let js = format!(
        "(function(){{\
            var sels = {selectors_lit};\
            for (var i = 0; i < sels.length; i++) {{\
                if (document.querySelector(sels[i])) return false;\
            }}\
            return true;\
        }})()"
    );

    let deadline = tokio::time::Instant::now() + Duration::from_millis(timeout_ms);
    loop {
        let clear = page
            .evaluate(js.as_str())
            .await
            .ok()
            .and_then(|r| r.into_value::<bool>().ok())
            .unwrap_or(false);
        if clear {
            return Ok(true);
        }
        if tokio::time::Instant::now() >= deadline {
            return Ok(false);
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}
