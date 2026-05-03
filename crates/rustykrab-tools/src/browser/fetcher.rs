//! HTTP fetcher modeled after Scrapling's `Fetcher` class.
//!
//! Performs plain HTTP requests with browser-like header sets, optional
//! "stealthy" header packs, custom user-agents, proxy support, redirect
//! following, and per-call retries. Returns a Scrapling-style response
//! object: `status`, `body`, `text`, `headers`, `cookies`,
//! `request_headers`, `history`, `url`, `encoding`, `reason`.
//!
//! This complements the CDP-driven actions in this module: use `fetch`
//! when a real browser isn't required (faster, lower overhead, no JS
//! execution); use `stealth_fetch` when JS rendering or anti-bot
//! evasion is needed.
//!
//! Scope: this is a Rust analogue of Scrapling.Fetcher's surface, not a
//! reimplementation of `curl-cffi`'s TLS-fingerprint impersonation.
//! `impersonate` here selects a coordinated header pack — the underlying
//! TLS handshake is whatever `reqwest` + `rustls` produce.

use rustykrab_core::{Error, Result};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::time::Duration;

use crate::security;

/// Maximum response body bytes to keep in memory.
const MAX_BODY_BYTES: usize = 5 * 1024 * 1024;

/// Maximum text body returned to the caller.
const MAX_TEXT_LEN: usize = 200_000;

/// Browser pack selection for `impersonate`.
#[derive(Debug, Clone, Copy)]
enum BrowserPack {
    Chrome,
    Firefox,
    Safari,
    Edge,
}

impl BrowserPack {
    fn parse(s: &str) -> Option<Self> {
        // Accept Scrapling-style ids: "chrome", "chrome131", "firefox135",
        // "safari17_2", "edge", etc. We map families, ignoring versions.
        let s = s.to_ascii_lowercase();
        if s.starts_with("chrome") {
            Some(Self::Chrome)
        } else if s.starts_with("firefox") {
            Some(Self::Firefox)
        } else if s.starts_with("safari") {
            Some(Self::Safari)
        } else if s.starts_with("edge") {
            Some(Self::Edge)
        } else {
            None
        }
    }
}

/// Modern user-agent strings, kept up to date enough to avoid the most
/// obvious "Scrapy/0.x" giveaway. These are also what `stealthy_headers`
/// pairs with.
fn default_user_agent(pack: BrowserPack) -> &'static str {
    match pack {
        BrowserPack::Chrome => {
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
             (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36"
        }
        BrowserPack::Firefox => {
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:133.0) \
             Gecko/20100101 Firefox/133.0"
        }
        BrowserPack::Safari => {
            "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) \
             AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.4 Safari/605.1.15"
        }
        BrowserPack::Edge => {
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
             (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36 Edg/131.0.0.0"
        }
    }
}

/// Build a Scrapling-style "stealthy" header pack for the given browser
/// family. Mirrors the Sec-Ch-Ua / Sec-Fetch-* hints a real browser sends.
fn stealthy_headers(pack: BrowserPack) -> Vec<(&'static str, String)> {
    let common = vec![
        (
            "Accept",
            "text/html,application/xhtml+xml,application/xml;q=0.9,\
             image/avif,image/webp,image/apng,*/*;q=0.8"
                .to_string(),
        ),
        ("Accept-Language", "en-US,en;q=0.9".to_string()),
        ("Accept-Encoding", "gzip, deflate, br".to_string()),
        ("Upgrade-Insecure-Requests", "1".to_string()),
        ("Sec-Fetch-Site", "none".to_string()),
        ("Sec-Fetch-Mode", "navigate".to_string()),
        ("Sec-Fetch-User", "?1".to_string()),
        ("Sec-Fetch-Dest", "document".to_string()),
        ("Cache-Control", "max-age=0".to_string()),
    ];
    let mut h = common;
    match pack {
        BrowserPack::Chrome | BrowserPack::Edge => {
            let brand = if matches!(pack, BrowserPack::Edge) {
                r#""Microsoft Edge";v="131", "Chromium";v="131", "Not_A Brand";v="24""#
            } else {
                r#""Google Chrome";v="131", "Chromium";v="131", "Not_A Brand";v="24""#
            };
            h.extend([
                ("Sec-Ch-Ua", brand.to_string()),
                ("Sec-Ch-Ua-Mobile", "?0".to_string()),
                ("Sec-Ch-Ua-Platform", r#""Windows""#.to_string()),
            ]);
        }
        BrowserPack::Firefox => {
            // Firefox does not send Sec-Ch-Ua hints.
            h.push(("DNT", "1".to_string()));
        }
        BrowserPack::Safari => {
            // Safari does not send Sec-Ch-Ua hints either.
        }
    }
    h
}

/// Parameters for a single `fetch` call.
#[derive(Debug, Default)]
pub struct FetchParams {
    pub method: String,
    pub url: String,
    pub body: Option<String>,
    pub json_body: Option<Value>,
    pub form: Option<HashMap<String, String>>,
    pub headers: HashMap<String, String>,
    pub cookies: HashMap<String, String>,
    pub user_agent: Option<String>,
    pub impersonate: Option<String>,
    pub stealthy_headers: bool,
    pub follow_redirects: bool,
    pub max_redirects: u32,
    pub timeout_ms: u64,
    pub retries: u32,
    pub proxy: Option<String>,
    pub verify_tls: bool,
}

impl FetchParams {
    /// Parse from the tool's JSON args.
    pub fn from_args(args: &Value) -> Result<Self> {
        let url = args["url"]
            .as_str()
            .ok_or_else(|| Error::ToolExecution("'fetch' requires 'url' parameter".into()))?
            .to_string();
        let method = args["method"]
            .as_str()
            .unwrap_or("GET")
            .to_ascii_uppercase();

        let mut headers = HashMap::new();
        if let Some(map) = args["extra_headers"].as_object() {
            for (k, v) in map {
                if let Some(s) = v.as_str() {
                    headers.insert(k.clone(), s.to_string());
                }
            }
        }

        let mut cookies = HashMap::new();
        if let Some(map) = args["cookies"].as_object() {
            for (k, v) in map {
                if let Some(s) = v.as_str() {
                    cookies.insert(k.clone(), s.to_string());
                }
            }
        }

        let mut form = None;
        if let Some(map) = args["form"].as_object() {
            let mut m = HashMap::new();
            for (k, v) in map {
                if let Some(s) = v.as_str() {
                    m.insert(k.clone(), s.to_string());
                }
            }
            form = Some(m);
        }

        Ok(Self {
            method,
            url,
            body: args["body"].as_str().map(str::to_string),
            json_body: args.get("json").cloned(),
            form,
            headers,
            cookies,
            user_agent: args["user_agent"].as_str().map(str::to_string),
            impersonate: args["impersonate"].as_str().map(str::to_string),
            stealthy_headers: args["stealthy_headers"].as_bool().unwrap_or(false),
            follow_redirects: args["follow_redirects"].as_bool().unwrap_or(true),
            max_redirects: args["max_redirects"].as_u64().unwrap_or(10) as u32,
            timeout_ms: args["timeout_ms"].as_u64().unwrap_or(30_000),
            retries: args["retries"].as_u64().unwrap_or(0) as u32,
            proxy: args["proxy"].as_str().map(str::to_string),
            verify_tls: args["verify_tls"].as_bool().unwrap_or(true),
        })
    }
}

/// Execute a Scrapling-style HTTP fetch. Returns a JSON object shaped like
/// Scrapling's response (status, body, text, headers, cookies, etc.).
pub async fn execute_fetch(params: FetchParams) -> Result<Value> {
    security::validate_url(&params.url)
        .await
        .map_err(|e| Error::ToolExecution(e.into()))?;

    let pack = params
        .impersonate
        .as_deref()
        .and_then(BrowserPack::parse)
        .unwrap_or(BrowserPack::Chrome);

    // Build the reqwest client. We rebuild per-call because options like
    // proxy, redirect policy, and TLS verification are call-scoped.
    let mut builder = reqwest::Client::builder()
        .timeout(Duration::from_millis(params.timeout_ms))
        .danger_accept_invalid_certs(!params.verify_tls);

    builder = if params.follow_redirects {
        builder.redirect(reqwest::redirect::Policy::limited(
            params.max_redirects as usize,
        ))
    } else {
        builder.redirect(reqwest::redirect::Policy::none())
    };

    let ua = params
        .user_agent
        .clone()
        .unwrap_or_else(|| default_user_agent(pack).to_string());
    builder = builder.user_agent(ua.clone());

    if let Some(proxy_url) = &params.proxy {
        let p = reqwest::Proxy::all(proxy_url).map_err(|e| {
            Error::ToolExecution(format!("invalid proxy '{proxy_url}': {e}").into())
        })?;
        builder = builder.proxy(p);
    }

    let client = builder
        .build()
        .map_err(|e| Error::ToolExecution(format!("failed to build HTTP client: {e}").into()))?;

    let mut last_err: Option<String> = None;
    let total_attempts = params.retries.saturating_add(1);

    for attempt in 0..total_attempts {
        match send_once(&client, &params, pack, &ua).await {
            Ok(resp) => return Ok(resp),
            Err(e) => {
                last_err = Some(e.to_string());
                if attempt + 1 < total_attempts {
                    let backoff_ms = 250u64 * (1u64 << attempt.min(5));
                    tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                }
            }
        }
    }

    Err(Error::ToolExecution(
        format!(
            "fetch failed after {total_attempts} attempt(s): {}",
            last_err.unwrap_or_else(|| "unknown error".into())
        )
        .into(),
    ))
}

async fn send_once(
    client: &reqwest::Client,
    params: &FetchParams,
    pack: BrowserPack,
    ua: &str,
) -> Result<Value> {
    let mut req = match params.method.as_str() {
        "GET" => client.get(&params.url),
        "POST" => client.post(&params.url),
        "PUT" => client.put(&params.url),
        "DELETE" => client.delete(&params.url),
        "PATCH" => client.patch(&params.url),
        "HEAD" => client.head(&params.url),
        m => {
            return Err(Error::ToolExecution(
                format!("unsupported HTTP method: '{m}'").into(),
            ));
        }
    };

    // Header order: stealthy pack first, then user overrides win.
    if params.stealthy_headers {
        for (k, v) in stealthy_headers(pack) {
            req = req.header(k, v);
        }
    }
    for (k, v) in &params.headers {
        req = req.header(k, v);
    }

    // Cookies as a single Cookie header (we don't keep a jar across calls;
    // for that, use stealth_fetch + browser cookies).
    if !params.cookies.is_empty() {
        let cookie_header = params
            .cookies
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("; ");
        req = req.header("Cookie", cookie_header);
    }

    // Body: prefer json > form > raw body.
    if let Some(j) = &params.json_body {
        req = req.json(j);
    } else if let Some(f) = &params.form {
        // The reqwest workspace build disables default features, so we
        // hand-encode the form body rather than relying on `.form()`.
        let encoded = encode_form(f);
        req = req
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(encoded);
    } else if let Some(b) = &params.body {
        req = req.body(b.clone());
    }

    // Snapshot request headers before sending (best-effort: reqwest does
    // not expose finalized headers ahead of send, so we record the merged
    // user view).
    let mut request_headers: HashMap<String, String> = HashMap::new();
    request_headers.insert("User-Agent".into(), ua.to_string());
    if params.stealthy_headers {
        for (k, v) in stealthy_headers(pack) {
            request_headers.insert(k.to_string(), v);
        }
    }
    for (k, v) in &params.headers {
        request_headers.insert(k.clone(), v.clone());
    }

    let resp = req
        .send()
        .await
        .map_err(|e| Error::ToolExecution(format!("HTTP send failed: {e}").into()))?;

    let final_url = resp.url().to_string();
    let status = resp.status().as_u16();
    let reason = resp.status().canonical_reason().unwrap_or("").to_string();

    let mut headers_map: HashMap<String, String> = HashMap::new();
    for (k, v) in resp.headers().iter() {
        if let Ok(s) = v.to_str() {
            headers_map.insert(k.as_str().to_string(), s.to_string());
        }
    }

    // Extract Set-Cookie cookies as { name: value }.
    let mut cookie_map: HashMap<String, String> = HashMap::new();
    for v in resp.headers().get_all("set-cookie").iter() {
        if let Ok(s) = v.to_str() {
            if let Some((name, rest)) = s.split_once('=') {
                let value = rest.split(';').next().unwrap_or("");
                cookie_map.insert(name.trim().to_string(), value.trim().to_string());
            }
        }
    }

    let content_type = headers_map.get("content-type").cloned().unwrap_or_default();

    // Cap body length to avoid OOM on huge responses.
    if let Some(len) = resp.content_length() {
        if len as usize > MAX_BODY_BYTES {
            return Err(Error::ToolExecution(
                format!(
                    "response Content-Length ({len} bytes) exceeds {MAX_BODY_BYTES} byte limit"
                )
                .into(),
            ));
        }
    }

    let bytes = resp
        .bytes()
        .await
        .map_err(|e| Error::ToolExecution(format!("failed reading body: {e}").into()))?;
    if bytes.len() > MAX_BODY_BYTES {
        return Err(Error::ToolExecution(
            format!("response body exceeds {MAX_BODY_BYTES} byte limit").into(),
        ));
    }

    let encoding = guess_encoding(&content_type);
    let text = String::from_utf8_lossy(&bytes).to_string();
    let truncated_text = truncate_for_text(&text);

    Ok(json!({
        "url": final_url,
        "status": status,
        "reason": reason,
        "ok": (200..400).contains(&status),
        "encoding": encoding,
        "content_type": content_type,
        "body_bytes": bytes.len(),
        "text": truncated_text.0,
        "text_truncated": truncated_text.1,
        "headers": headers_map,
        "cookies": cookie_map,
        "request_headers": request_headers,
        // We do not currently expose the redirect chain individually; the
        // final `url` reflects any follow-through. `history` is reserved
        // for future expansion when reqwest exposes it.
        "history": [],
        "impersonate": format!("{pack:?}").to_lowercase(),
    }))
}

/// Percent-encode a single form value (RFC 3986 unreserved + space-as-plus).
fn percent_encode(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for b in input.as_bytes() {
        match *b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char);
            }
            b' ' => out.push('+'),
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

fn encode_form(map: &HashMap<String, String>) -> String {
    let mut parts = Vec::with_capacity(map.len());
    for (k, v) in map {
        parts.push(format!("{}={}", percent_encode(k), percent_encode(v)));
    }
    parts.join("&")
}

fn guess_encoding(content_type: &str) -> String {
    // Find charset=...
    for part in content_type.split(';').map(str::trim) {
        if let Some(rest) = part.strip_prefix("charset=") {
            return rest.trim_matches('"').to_string();
        }
    }
    "utf-8".to_string()
}

fn truncate_for_text(s: &str) -> (String, bool) {
    if s.len() <= MAX_TEXT_LEN {
        return (s.to_string(), false);
    }
    let mut end = MAX_TEXT_LEN;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    (s[..end].to_string(), true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn impersonate_parses_versioned_ids() {
        assert!(matches!(
            BrowserPack::parse("chrome131"),
            Some(BrowserPack::Chrome)
        ));
        assert!(matches!(
            BrowserPack::parse("firefox135"),
            Some(BrowserPack::Firefox)
        ));
        assert!(matches!(
            BrowserPack::parse("safari17_2"),
            Some(BrowserPack::Safari)
        ));
        assert!(matches!(
            BrowserPack::parse("edge"),
            Some(BrowserPack::Edge)
        ));
        assert!(BrowserPack::parse("opera").is_none());
    }

    #[test]
    fn stealthy_headers_chrome_includes_sec_ch_ua() {
        let pack = BrowserPack::Chrome;
        let headers = stealthy_headers(pack);
        let mut keys: Vec<&str> = headers.iter().map(|(k, _)| *k).collect();
        keys.sort();
        assert!(keys.contains(&"Sec-Ch-Ua"));
        assert!(keys.contains(&"Sec-Fetch-Mode"));
        assert!(keys.contains(&"Accept-Language"));
    }

    #[test]
    fn stealthy_headers_firefox_omits_sec_ch_ua() {
        let pack = BrowserPack::Firefox;
        let headers = stealthy_headers(pack);
        let keys: Vec<&str> = headers.iter().map(|(k, _)| *k).collect();
        assert!(!keys.contains(&"Sec-Ch-Ua"));
        assert!(keys.contains(&"DNT"));
    }

    #[test]
    fn percent_encode_handles_special_chars() {
        assert_eq!(percent_encode("hello world"), "hello+world");
        assert_eq!(percent_encode("a&b=c"), "a%26b%3Dc");
        assert_eq!(percent_encode("café"), "caf%C3%A9");
        assert_eq!(percent_encode("a-b_c.d~e"), "a-b_c.d~e");
    }

    #[test]
    fn encode_form_produces_valid_pairs() {
        let mut m = HashMap::new();
        m.insert("name".to_string(), "Bob & Jane".to_string());
        let encoded = encode_form(&m);
        assert_eq!(encoded, "name=Bob+%26+Jane");
    }

    #[test]
    fn guess_encoding_parses_charset() {
        assert_eq!(
            guess_encoding("text/html; charset=ISO-8859-1"),
            "ISO-8859-1"
        );
        assert_eq!(guess_encoding("text/html"), "utf-8");
        assert_eq!(
            guess_encoding("application/json; charset=\"utf-16\""),
            "utf-16"
        );
    }

    #[test]
    fn fetch_params_from_args_defaults_method_to_get() {
        let args = serde_json::json!({ "url": "https://example.com" });
        let p = FetchParams::from_args(&args).unwrap();
        assert_eq!(p.method, "GET");
        assert!(p.follow_redirects);
        assert_eq!(p.timeout_ms, 30_000);
    }
}
