use async_trait::async_trait;
use rustykrab_core::types::ToolSchema;
use rustykrab_core::{Error, Result, Tool};
use serde_json::{json, Value};

/// A tool that searches the web using DuckDuckGo and returns results.
///
/// Uses DuckDuckGo's HTML lite interface so no API key is required.
pub struct WebSearchTool {
    client: reqwest::Client,
}

impl WebSearchTool {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::builder()
                .user_agent("RustyKrab/0.1 (AI Agent)")
                .timeout(std::time::Duration::from_secs(15))
                .redirect(reqwest::redirect::Policy::limited(5))
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
        }
    }
}

impl Default for WebSearchTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for WebSearchTool {
    fn name(&self) -> &str {
        "web_search"
    }

    fn description(&self) -> &str {
        "Search the web using DuckDuckGo and return a list of results with titles, \
         URLs, and snippets. Use this to find information, discover URLs, or research topics."
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: self.name().to_string(),
            description: self.description().to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "The search query"
                    },
                    "max_results": {
                        "type": "integer",
                        "description": "Maximum number of results to return (default: 10, max: 25)",
                        "default": 10
                    }
                },
                "required": ["query"]
            }),
        }
    }

    async fn execute(&self, args: Value) -> Result<Value> {
        let query = args["query"]
            .as_str()
            .ok_or_else(|| Error::ToolExecution("missing query".into()))?;
        let max_results = args["max_results"].as_u64().unwrap_or(10).min(25) as usize;

        let results = search_duckduckgo(&self.client, query, max_results).await?;

        Ok(json!({
            "query": query,
            "results": results,
            "count": results.len(),
        }))
    }
}

/// Search DuckDuckGo using the HTML lite endpoint.
async fn search_duckduckgo(
    client: &reqwest::Client,
    query: &str,
    max_results: usize,
) -> Result<Vec<Value>> {
    let resp = client
        .get("https://html.duckduckgo.com/html/")
        .query(&[("q", query)])
        .header("Accept", "text/html")
        .send()
        .await
        .map_err(|e| Error::ToolExecution(format!("search request failed: {e}").into()))?;

    if !resp.status().is_success() {
        return Err(Error::ToolExecution(
            format!("search returned status {}", resp.status()).into(),
        ));
    }

    let body = resp
        .text()
        .await
        .map_err(|e| Error::ToolExecution(format!("failed to read search response: {e}").into()))?;

    let results = parse_duckduckgo_results(&body, max_results);
    Ok(results)
}

/// Parse DuckDuckGo HTML lite results page.
///
/// The lite page has a predictable structure:
/// - Result links are in `<a class="result-link" href="...">title</a>`
/// - Snippets are in `<td class="result-snippet">...</td>`
/// - Or result blocks with `<a class="result__a" href="...">` and
///   `<a class="result__snippet">...</a>`
fn parse_duckduckgo_results(html: &str, max_results: usize) -> Vec<Value> {
    let mut results = Vec::new();

    // DuckDuckGo lite uses table-based layout. Each result has:
    // 1. A link row with the title/URL
    // 2. A snippet row
    // We'll parse by finding result links and their associated snippets.

    // Strategy: find all <a> tags with class containing "result" and href,
    // then find the next snippet text.
    let mut pos = 0;
    let html_lower = html.to_lowercase();

    while results.len() < max_results {
        // Find next result link — DuckDuckGo lite uses class="result-link"
        // or sometimes class="result__a"
        let link_pos = find_result_link(&html_lower, pos);
        if link_pos.is_none() {
            break;
        }
        let link_start = link_pos.unwrap();

        // Extract href
        let href = extract_attribute(&html[link_start..], "href");
        if href.is_empty() {
            pos = link_start + 1;
            continue;
        }

        // Resolve DuckDuckGo redirect URLs
        let url = resolve_ddg_url(&href);

        // Extract title (text between > and </a>)
        let title = extract_tag_text(&html[link_start..]);

        // Find snippet — look for "result-snippet" or "result__snippet" after this link
        let snippet = extract_snippet(&html[link_start..], &html_lower[link_start..]);

        if !url.is_empty() && !title.is_empty() {
            results.push(json!({
                "title": title.trim(),
                "url": url,
                "snippet": snippet.trim(),
            }));
        }

        pos = link_start + 1;
    }

    results
}

/// Find the position of the next result link in the HTML.
fn find_result_link(html_lower: &str, start: usize) -> Option<usize> {
    let search_from = &html_lower[start..];

    // Look for result-link class (DuckDuckGo lite format)
    if let Some(p) = search_from.find("class=\"result-link\"") {
        // Find the opening <a that precedes this
        let before = &search_from[..p];
        if let Some(a_pos) = before.rfind("<a ") {
            return Some(start + a_pos);
        }
    }

    // Also try result__a class (alternative DDG format)
    if let Some(p) = search_from.find("class=\"result__a\"") {
        let before = &search_from[..p];
        if let Some(a_pos) = before.rfind("<a ") {
            return Some(start + a_pos);
        }
    }

    // Fallback: look for links in result blocks
    if let Some(p) = search_from.find("class=\"result-link") {
        let before = &search_from[..p];
        if let Some(a_pos) = before.rfind("<a ") {
            return Some(start + a_pos);
        }
    }

    None
}

/// Extract an HTML attribute value from a tag.
fn extract_attribute(html: &str, attr: &str) -> String {
    let lower = html.to_lowercase();
    let pattern = format!("{attr}=\"");
    if let Some(pos) = lower.find(&pattern) {
        let value_start = pos + pattern.len();
        if let Some(end) = html[value_start..].find('"') {
            return html[value_start..value_start + end].to_string();
        }
    }
    // Try single quotes
    let pattern = format!("{attr}='");
    if let Some(pos) = lower.find(&pattern) {
        let value_start = pos + pattern.len();
        if let Some(end) = html[value_start..].find('\'') {
            return html[value_start..value_start + end].to_string();
        }
    }
    String::new()
}

/// Extract text content from between > and </a> of an anchor tag.
fn extract_tag_text(html: &str) -> String {
    if let Some(gt) = html.find('>') {
        let after = &html[gt + 1..];
        if let Some(end) = after.to_lowercase().find("</a>") {
            let raw = &after[..end];
            // Strip any inner HTML tags
            return strip_tags(raw);
        }
    }
    String::new()
}

/// Extract snippet text from result block.
fn extract_snippet(html: &str, html_lower: &str) -> String {
    // Look for result-snippet class
    for class in &["result-snippet", "result__snippet"] {
        let pattern = format!("class=\"{class}\"");
        if let Some(pos) = html_lower.find(&pattern) {
            let from = &html[pos..];
            if let Some(gt) = from.find('>') {
                let after = &from[gt + 1..];
                // Find the closing tag
                if let Some(end) = after.find("</") {
                    return strip_tags(&after[..end]);
                }
            }
        }
    }
    String::new()
}

/// Resolve DuckDuckGo redirect URL to the actual URL.
fn resolve_ddg_url(href: &str) -> String {
    // DDG lite uses direct URLs or //duckduckgo.com/l/?uddg=ENCODED_URL
    if href.contains("uddg=") {
        // Extract the actual URL from the redirect
        if let Some(pos) = href.find("uddg=") {
            let encoded = &href[pos + 5..];
            let encoded = encoded.split('&').next().unwrap_or(encoded);
            return url_decode(encoded);
        }
    }
    // Direct URL
    href.to_string()
}

/// Simple URL decoding for search result URLs.
fn url_decode(input: &str) -> String {
    let mut result = String::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(byte) = u8::from_str_radix(&String::from_utf8_lossy(&bytes[i + 1..i + 3]), 16)
            {
                result.push(byte as char);
                i += 3;
                continue;
            }
        }
        result.push(bytes[i] as char);
        i += 1;
    }

    result
}

/// Strip HTML tags from a string, returning only text content.
fn strip_tags(html: &str) -> String {
    let mut result = String::with_capacity(html.len());
    let mut in_tag = false;

    for ch in html.chars() {
        if ch == '<' {
            in_tag = true;
        } else if ch == '>' {
            in_tag = false;
        } else if !in_tag {
            result.push(ch);
        }
    }

    // Decode basic entities
    result
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ")
}
