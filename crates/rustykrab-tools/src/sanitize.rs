//! Sanitization utilities for untrusted external content.
//!
//! These functions strip HTML tags, decode entities, and normalize whitespace
//! to produce clean plaintext from web pages, emails, and other external sources.
//! They are intentionally kept free of any interpretation logic — the goal is to
//! reduce raw HTML to readable text without executing scripts or preserving
//! potentially adversarial markup.

/// Convert HTML to readable plain text.
pub fn html_to_text(html: &str, include_links: bool) -> String {
    let mut result = String::with_capacity(html.len() / 2);
    let mut in_tag = false;
    let mut tag_name = String::new();
    let mut in_script_or_style = false;
    let mut skip_depth: u32 = 0;
    let mut collecting_tag = false;
    let mut current_href = String::new();
    let mut in_anchor = false;
    let mut anchor_text = String::new();

    // Iterate directly over the str using byte offsets instead of collecting
    // all chars into a Vec (which would use 4 bytes per char — ~4x memory).
    let len = html.len();
    let mut i = 0;

    while i < len {
        let ch = html[i..].chars().next().unwrap();
        let ch_len = ch.len_utf8();

        if ch == '<' {
            in_tag = true;
            collecting_tag = true;
            tag_name.clear();
            i += ch_len;
            continue;
        }

        if in_tag {
            if ch == '>' {
                in_tag = false;
                collecting_tag = false;

                let tag_lower = tag_name.to_lowercase();
                let is_closing = tag_lower.starts_with('/');
                let clean_tag = tag_lower
                    .trim_start_matches('/')
                    .split_whitespace()
                    .next()
                    .unwrap_or("");

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
                    "br" | "hr" if !in_script_or_style => {
                        if in_anchor {
                            anchor_text.push('\n');
                        } else {
                            result.push('\n');
                        }
                    }
                    "p" | "div" | "section" | "article" | "main" | "header" | "footer" | "nav"
                    | "aside" | "blockquote" | "tr" | "table"
                        if !in_script_or_style =>
                    {
                        if is_closing {
                            if in_anchor {
                                anchor_text.push_str("\n\n");
                            } else {
                                result.push_str("\n\n");
                            }
                        } else if in_anchor {
                            anchor_text.push('\n');
                        } else {
                            result.push('\n');
                        }
                    }
                    "h1" | "h2" | "h3" | "h4" | "h5" | "h6" if !in_script_or_style => {
                        result.push_str("\n\n");
                    }
                    "li" if !in_script_or_style && !is_closing => {
                        result.push_str("\n- ");
                    }
                    "a" if !in_script_or_style && include_links => {
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
                    "td" | "th" if !in_script_or_style && is_closing => {
                        result.push('\t');
                    }
                    _ => {}
                }

                i += ch_len;
                continue;
            }

            if collecting_tag {
                tag_name.push(ch);
            }
            i += ch_len;
            continue;
        }

        // Not in a tag — this is text content
        if in_script_or_style {
            i += ch_len;
            continue;
        }

        // Handle HTML entities
        if ch == '&' {
            if let Some(end) = html[i..].find(';') {
                let entity = &html[i..i + end + 1];
                let decoded = decode_entity(entity);
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
        i += ch_len;
    }

    // Collapse excessive whitespace but preserve paragraph breaks
    collapse_whitespace(&result)
}

/// Extract href attribute value from a tag string like `a href="..."`
fn extract_href(tag_content: &str) -> String {
    // Use to_ascii_lowercase() to preserve byte positions — to_lowercase()
    // can shift byte offsets when multi-byte chars change length.
    let lower = tag_content.to_ascii_lowercase();
    if let Some(pos) = lower.find("href=") {
        let after_href = &tag_content[pos + 5..];
        let trimmed = after_href.trim_start();
        if let Some(rest) = trimmed.strip_prefix('"') {
            rest.split('"').next().unwrap_or("").to_string()
        } else if let Some(rest) = trimmed.strip_prefix('\'') {
            rest.split('\'').next().unwrap_or("").to_string()
        } else {
            trimmed.split_whitespace().next().unwrap_or("").to_string()
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
