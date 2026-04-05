use async_trait::async_trait;
use openclaw_core::types::ToolSchema;
use openclaw_core::{Error, Result, Tool};
use serde_json::{json, Value};

/// Maximum characters to return from a fetched page to avoid context explosion.
const MAX_CONTENT_LENGTH: usize = 50_000;

/// A tool that fetches a web page and returns cleaned, readable text.
///
/// Unlike the raw `http_request` tool, this strips HTML tags, scripts,
/// and styles to produce content the model can actually reason about.
pub struct WebFetchTool {
    client: reqwest::Client,
}

impl WebFetchTool {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::builder()
                .user_agent("OpenClaw/0.1 (AI Agent)")
                .timeout(std::time::Duration::from_secs(30))
                .redirect(reqwest::redirect::Policy::limited(10))
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
        }
    }
}

impl Default for WebFetchTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for WebFetchTool {
    fn name(&self) -> &str {
        "web_fetch"
    }

    fn description(&self) -> &str {
        "Fetch a web page and return its content as clean, readable text with HTML stripped. \
         Use this to read articles, documentation, or any web page."
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: self.name().to_string(),
            description: self.description().to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "url": {
                        "type": "string",
                        "description": "The URL to fetch"
                    },
                    "include_links": {
                        "type": "boolean",
                        "description": "Whether to include [link text](url) in the output (default: false)"
                    }
                },
                "required": ["url"]
            }),
        }
    }

    async fn execute(&self, args: Value) -> Result<Value> {
        let url = args["url"]
            .as_str()
            .ok_or_else(|| Error::ToolExecution("missing url".into()))?;
        let include_links = args["include_links"].as_bool().unwrap_or(false);

        let resp = self
            .client
            .get(url)
            .header("Accept", "text/html,application/xhtml+xml,text/plain")
            .send()
            .await
            .map_err(|e| Error::ToolExecution(format!("fetch failed: {e}")))?;

        let status = resp.status().as_u16();
        let content_type = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();

        let body = resp
            .text()
            .await
            .map_err(|e| Error::ToolExecution(format!("failed to read body: {e}")))?;

        // If it's not HTML, return raw text (could be JSON, plain text, etc.)
        let text = if content_type.contains("text/html") || content_type.contains("xhtml") {
            html_to_text(&body, include_links)
        } else {
            body
        };

        // Truncate to avoid context explosion.
        let truncated = text.len() > MAX_CONTENT_LENGTH;
        let content = if truncated {
            let end = text
                .char_indices()
                .take_while(|(i, _)| *i < MAX_CONTENT_LENGTH)
                .last()
                .map(|(i, c)| i + c.len_utf8())
                .unwrap_or(MAX_CONTENT_LENGTH);
            format!("{}...\n\n[Content truncated at {} characters]", &text[..end], MAX_CONTENT_LENGTH)
        } else {
            text
        };

        Ok(json!({
            "status": status,
            "content": content,
            "truncated": truncated,
        }))
    }
}

/// Convert HTML to readable plain text.
fn html_to_text(html: &str, include_links: bool) -> String {
    let mut result = String::with_capacity(html.len() / 2);
    let mut in_tag = false;
    let mut tag_name = String::new();
    let mut in_script_or_style = false;
    let mut skip_depth: u32 = 0;
    let mut collecting_tag = false;
    let mut current_href = String::new();
    let mut in_anchor = false;
    let mut anchor_text = String::new();

    let chars: Vec<char> = html.chars().collect();
    let len = chars.len();
    let mut i = 0;

    while i < len {
        let ch = chars[i];

        if ch == '<' {
            in_tag = true;
            collecting_tag = true;
            tag_name.clear();
            i += 1;
            continue;
        }

        if in_tag {
            if ch == '>' {
                in_tag = false;
                collecting_tag = false;

                let tag_lower = tag_name.to_lowercase();
                let is_closing = tag_lower.starts_with('/');
                let clean_tag = tag_lower.trim_start_matches('/').split_whitespace().next().unwrap_or("");

                match clean_tag {
                    "script" | "style" | "noscript" | "svg" | "head" => {
                        if is_closing {
                            skip_depth = skip_depth.saturating_sub(1);
                            if skip_depth == 0 {
                                in_script_or_style = false;
                            }
                        } else if !tag_lower.ends_with('/') {
                            // Not self-closing
                            skip_depth += 1;
                            in_script_or_style = true;
                        }
                    }
                    "br" | "hr" => {
                        if !in_script_or_style {
                            if in_anchor {
                                anchor_text.push('\n');
                            } else {
                                result.push('\n');
                            }
                        }
                    }
                    "p" | "div" | "section" | "article" | "main" | "header" | "footer"
                    | "nav" | "aside" | "blockquote" | "tr" | "table" => {
                        if !in_script_or_style {
                            if is_closing {
                                if in_anchor {
                                    anchor_text.push_str("\n\n");
                                } else {
                                    result.push_str("\n\n");
                                }
                            } else {
                                if in_anchor {
                                    anchor_text.push('\n');
                                } else {
                                    result.push('\n');
                                }
                            }
                        }
                    }
                    "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                        if !in_script_or_style {
                            if is_closing {
                                result.push_str("\n\n");
                            } else {
                                result.push_str("\n\n");
                            }
                        }
                    }
                    "li" => {
                        if !in_script_or_style && !is_closing {
                            result.push_str("\n- ");
                        }
                    }
                    "a" => {
                        if !in_script_or_style && include_links {
                            if is_closing {
                                // End of anchor — emit markdown link
                                if !current_href.is_empty() && !anchor_text.trim().is_empty() {
                                    result.push('[');
                                    result.push_str(anchor_text.trim());
                                    result.push_str("](");
                                    result.push_str(&current_href);
                                    result.push(')');
                                } else {
                                    result.push_str(anchor_text.trim());
                                }
                                in_anchor = false;
                                anchor_text.clear();
                                current_href.clear();
                            } else {
                                // Extract href from tag attributes
                                current_href = extract_href(&tag_name);
                                in_anchor = true;
                                anchor_text.clear();
                            }
                        }
                    }
                    "td" | "th" => {
                        if !in_script_or_style && is_closing {
                            result.push('\t');
                        }
                    }
                    _ => {}
                }

                i += 1;
                continue;
            }

            if collecting_tag {
                tag_name.push(ch);
            }
            i += 1;
            continue;
        }

        // Not in a tag — this is text content
        if in_script_or_style {
            i += 1;
            continue;
        }

        // Handle HTML entities
        if ch == '&' {
            let entity_end = chars[i..].iter().position(|&c| c == ';');
            if let Some(end) = entity_end {
                let entity: String = chars[i..i + end + 1].iter().collect();
                let decoded = decode_entity(&entity);
                if in_anchor {
                    anchor_text.push_str(&decoded);
                } else {
                    result.push_str(&decoded);
                }
                i += end + 1;
                continue;
            }
        }

        if in_anchor {
            anchor_text.push(ch);
        } else {
            result.push(ch);
        }
        i += 1;
    }

    // Collapse excessive whitespace but preserve paragraph breaks
    collapse_whitespace(&result)
}

/// Extract href attribute value from a tag string like `a href="..."`
fn extract_href(tag_content: &str) -> String {
    let lower = tag_content.to_lowercase();
    if let Some(pos) = lower.find("href=") {
        let after_href = &tag_content[pos + 5..];
        let trimmed = after_href.trim_start();
        if trimmed.starts_with('"') {
            trimmed[1..]
                .split('"')
                .next()
                .unwrap_or("")
                .to_string()
        } else if trimmed.starts_with('\'') {
            trimmed[1..]
                .split('\'')
                .next()
                .unwrap_or("")
                .to_string()
        } else {
            trimmed
                .split_whitespace()
                .next()
                .unwrap_or("")
                .to_string()
        }
    } else {
        String::new()
    }
}

/// Decode common HTML entities.
fn decode_entity(entity: &str) -> String {
    match entity {
        "&amp;" => "&".to_string(),
        "&lt;" => "<".to_string(),
        "&gt;" => ">".to_string(),
        "&quot;" => "\"".to_string(),
        "&apos;" | "&#39;" => "'".to_string(),
        "&nbsp;" | "&#160;" => " ".to_string(),
        "&mdash;" | "&#8212;" => "\u{2014}".to_string(),
        "&ndash;" | "&#8211;" => "\u{2013}".to_string(),
        "&hellip;" | "&#8230;" => "\u{2026}".to_string(),
        "&copy;" => "\u{00A9}".to_string(),
        "&reg;" => "\u{00AE}".to_string(),
        "&trade;" => "\u{2122}".to_string(),
        other => {
            // Try numeric entities: &#123; or &#x1F4A9;
            if other.starts_with("&#x") || other.starts_with("&#X") {
                let hex = &other[3..other.len() - 1];
                u32::from_str_radix(hex, 16)
                    .ok()
                    .and_then(char::from_u32)
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| other.to_string())
            } else if other.starts_with("&#") {
                let num = &other[2..other.len() - 1];
                num.parse::<u32>()
                    .ok()
                    .and_then(char::from_u32)
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| other.to_string())
            } else {
                other.to_string()
            }
        }
    }
}

/// Collapse runs of whitespace: keep double newlines (paragraph breaks),
/// collapse everything else to single spaces.
fn collapse_whitespace(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut prev_newline_count = 0;
    let mut prev_was_space = false;

    for ch in text.chars() {
        if ch == '\n' {
            prev_newline_count += 1;
            prev_was_space = false;
            if prev_newline_count <= 2 {
                result.push('\n');
            }
        } else if ch.is_whitespace() {
            prev_newline_count = 0;
            if !prev_was_space {
                result.push(' ');
                prev_was_space = true;
            }
        } else {
            prev_newline_count = 0;
            prev_was_space = false;
            result.push(ch);
        }
    }

    result.trim().to_string()
}
