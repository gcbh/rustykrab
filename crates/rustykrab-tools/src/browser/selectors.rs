//! Scrapling-style selector engine.
//!
//! Mirrors the surface of Scrapling's `Selector`/`Adaptor`:
//! - `.css(selector)` with Scrapling pseudo-selectors `::text` and
//!   `::attr(name)`.
//! - `.xpath(query)` — via the live page's `document.evaluate` when a
//!   `Page` is available, or skipped for static HTML.
//! - `.find_by_text(needle, regex=False)` for text-based filtering.
//!
//! Two operating modes:
//! - **Static**: parse a provided HTML body with the `scraper` crate.
//! - **Live**: query the active tab's DOM via JavaScript.
//!
//! The result shape is consistent across modes: a list of match objects
//! with `text`, `html`, `attributes`, and optional extracted `value`
//! when a pseudo-selector was used.

use chromiumoxide::Page;
use rustykrab_core::{Error, Result};
use scraper::{ElementRef, Html, Selector};
use serde_json::{json, Map, Value};

/// Maximum HTML to parse to avoid pathological inputs.
const MAX_STATIC_HTML: usize = 4 * 1024 * 1024;

/// Maximum results returned per call.
const MAX_RESULTS: usize = 500;

/// Parsed CSS selector with optional Scrapling pseudo-suffix.
struct ParsedCss {
    /// The CSS selector minus the trailing pseudo, if any.
    base: String,
    /// `Text` extracts text nodes; `Attr(name)` extracts an attribute.
    pseudo: Option<Pseudo>,
}

enum Pseudo {
    Text,
    Attr(String),
}

fn parse_css(s: &str) -> ParsedCss {
    let trimmed = s.trim();
    if let Some(rest) = trimmed.strip_suffix("::text") {
        return ParsedCss {
            base: rest.trim_end().to_string(),
            pseudo: Some(Pseudo::Text),
        };
    }
    // ::attr(name)
    if let Some(idx) = trimmed.rfind("::attr(") {
        let after = &trimmed[idx + "::attr(".len()..];
        if let Some(end) = after.find(')') {
            let name = after[..end]
                .trim_matches('"')
                .trim_matches('\'')
                .to_string();
            return ParsedCss {
                base: trimmed[..idx].trim_end().to_string(),
                pseudo: Some(Pseudo::Attr(name)),
            };
        }
    }
    ParsedCss {
        base: trimmed.to_string(),
        pseudo: None,
    }
}

/// Parameters for a `select` call.
#[derive(Debug, Default)]
pub struct SelectParams {
    pub html: Option<String>,
    pub css: Option<String>,
    pub xpath: Option<String>,
    pub find_by_text: Option<String>,
    pub regex: bool,
    pub limit: Option<usize>,
    pub auto_save: bool,
    pub auto_match: bool,
    pub auto_save_id: Option<String>,
    pub include_html: bool,
}

impl SelectParams {
    pub fn from_args(args: &Value) -> Self {
        Self {
            html: args["html"].as_str().map(str::to_string),
            css: args["css"].as_str().map(str::to_string),
            xpath: args["xpath"].as_str().map(str::to_string),
            find_by_text: args["find_by_text"].as_str().map(str::to_string),
            regex: args["regex"].as_bool().unwrap_or(false),
            limit: args["limit"].as_u64().map(|v| v as usize),
            auto_save: args["auto_save"].as_bool().unwrap_or(false),
            auto_match: args["auto_match"].as_bool().unwrap_or(false),
            auto_save_id: args["auto_save_id"].as_str().map(str::to_string),
            include_html: args["include_html"].as_bool().unwrap_or(false),
        }
    }
}

/// A unified match object (independent of static vs live mode).
#[derive(Debug, Clone)]
pub struct Match {
    pub tag: String,
    pub text: String,
    pub html: Option<String>,
    pub attributes: Map<String, Value>,
    /// Extracted value when `::text` or `::attr(name)` was used.
    pub value: Option<String>,
    /// Stable-ish path of the element within its document.
    pub path: String,
}

impl Match {
    pub fn to_json(&self) -> Value {
        let mut o = Map::new();
        o.insert("tag".into(), Value::String(self.tag.clone()));
        o.insert("text".into(), Value::String(self.text.clone()));
        o.insert("path".into(), Value::String(self.path.clone()));
        if let Some(ref h) = self.html {
            o.insert("html".into(), Value::String(h.clone()));
        }
        o.insert("attributes".into(), Value::Object(self.attributes.clone()));
        if let Some(ref v) = self.value {
            o.insert("value".into(), Value::String(v.clone()));
        }
        Value::Object(o)
    }
}

/// Perform a static select over `html`. `Page` is unused.
pub fn select_static(html: &str, params: &SelectParams) -> Result<Vec<Match>> {
    if html.len() > MAX_STATIC_HTML {
        return Err(Error::ToolExecution(
            format!("html exceeds {MAX_STATIC_HTML} byte limit").into(),
        ));
    }
    let doc = Html::parse_document(html);

    let mut elements: Vec<ElementRef> = Vec::new();
    let mut pseudo_value: Option<Pseudo> = None;

    if let Some(css) = &params.css {
        let parsed = parse_css(css);
        let sel = Selector::parse(&parsed.base).map_err(|e| {
            Error::ToolExecution(format!("invalid CSS selector '{}': {e:?}", parsed.base).into())
        })?;
        for elem in doc.select(&sel) {
            elements.push(elem);
            if elements.len() >= MAX_RESULTS {
                break;
            }
        }
        pseudo_value = parsed.pseudo;
    } else if params.xpath.is_some() {
        return Err(Error::ToolExecution(
            "xpath is only supported in live mode (no 'html' arg). \
             Use css or omit 'html' to run against the active tab."
                .into(),
        ));
    } else {
        // No selector: scan the body for `find_by_text`.
        let body_sel = Selector::parse("body").unwrap();
        for body in doc.select(&body_sel) {
            elements.push(body);
        }
    }

    // Optional text filter.
    if let Some(needle) = &params.find_by_text {
        elements = filter_by_text(elements, needle, params.regex)?;
    }

    let limit = params.limit.unwrap_or(MAX_RESULTS).min(MAX_RESULTS);
    let mut results = Vec::with_capacity(limit.min(elements.len()));
    for (i, elem) in elements.into_iter().enumerate() {
        if i >= limit {
            break;
        }
        results.push(element_to_match(elem, &pseudo_value, params.include_html));
    }
    Ok(results)
}

/// Perform a live select against the active tab.
pub async fn select_live(page: &Page, params: &SelectParams) -> Result<Vec<Match>> {
    let limit = params.limit.unwrap_or(MAX_RESULTS).min(MAX_RESULTS);

    // Build a JS routine that returns a JSON-serializable list of matches,
    // mirroring the Match shape produced by static parsing.
    let css_arg = match &params.css {
        Some(s) => serde_json::to_string(s).unwrap_or_else(|_| "null".into()),
        None => "null".into(),
    };
    let xpath_arg = match &params.xpath {
        Some(s) => serde_json::to_string(s).unwrap_or_else(|_| "null".into()),
        None => "null".into(),
    };
    let text_arg = match &params.find_by_text {
        Some(s) => serde_json::to_string(s).unwrap_or_else(|_| "null".into()),
        None => "null".into(),
    };
    let include_html = if params.include_html { "true" } else { "false" };
    let regex = if params.regex { "true" } else { "false" };

    let js = format!(
        r#"
(function(){{
    var CSS_RAW = {css_arg};
    var XPATH = {xpath_arg};
    var TEXT_NEEDLE = {text_arg};
    var REGEX = {regex};
    var INCLUDE_HTML = {include_html};
    var LIMIT = {limit};
    var MAX_TEXT = 4000;

    function pseudoOf(s) {{
        if (!s) return [s, null];
        var t = s.trim();
        if (t.endsWith('::text')) return [t.slice(0, -('::text').length).trim(), {{ kind:'text' }}];
        var i = t.lastIndexOf('::attr(');
        if (i >= 0) {{
            var rest = t.slice(i + ('::attr(').length);
            var j = rest.indexOf(')');
            if (j >= 0) {{
                var name = rest.slice(0, j).replace(/^["']|["']$/g, '');
                return [t.slice(0, i).trim(), {{ kind:'attr', name: name }}];
            }}
        }}
        return [t, null];
    }}

    function structuralPath(el) {{
        var parts = [];
        var node = el;
        while (node && node.nodeType === 1 && node.parentElement) {{
            var parent = node.parentElement;
            var tag = node.tagName.toLowerCase();
            if (node.id && !/^[0-9]/.test(node.id)) {{ parts.unshift('#' + node.id); break; }}
            var sibs = Array.from(parent.children).filter(function(c){{ return c.tagName === node.tagName; }});
            if (sibs.length === 1) parts.unshift(tag);
            else parts.unshift(tag + ':nth-of-type(' + (sibs.indexOf(node)+1) + ')');
            node = parent;
        }}
        return parts.join(' > ') || (el.tagName ? el.tagName.toLowerCase() : '');
    }}

    function attrsOf(el) {{
        var o = {{}};
        if (!el.attributes) return o;
        for (var i = 0; i < el.attributes.length; i++) {{
            var a = el.attributes[i];
            o[a.name] = a.value;
        }}
        return o;
    }}

    function buildMatch(el, pseudo) {{
        var text = (el.textContent || '').trim();
        if (text.length > MAX_TEXT) text = text.slice(0, MAX_TEXT);
        var m = {{
            tag: el.tagName ? el.tagName.toLowerCase() : '',
            text: text,
            path: structuralPath(el),
            attributes: attrsOf(el),
            value: null,
        }};
        if (INCLUDE_HTML) m.html = el.outerHTML || '';
        if (pseudo) {{
            if (pseudo.kind === 'text') m.value = text;
            else if (pseudo.kind === 'attr') m.value = el.getAttribute(pseudo.name) || '';
        }}
        return m;
    }}

    var results = [];
    var pseudo = null;

    if (CSS_RAW) {{
        var pp = pseudoOf(CSS_RAW);
        var base = pp[0]; pseudo = pp[1];
        try {{
            var nodes = document.querySelectorAll(base);
            for (var i = 0; i < nodes.length && results.length < LIMIT; i++) {{
                results.push(buildMatch(nodes[i], pseudo));
            }}
        }} catch(e) {{
            return JSON.stringify({{ error: 'invalid CSS selector: ' + e.message }});
        }}
    }} else if (XPATH) {{
        try {{
            var iter = document.evaluate(XPATH, document, null,
                XPathResult.ORDERED_NODE_SNAPSHOT_TYPE, null);
            for (var k = 0; k < iter.snapshotLength && results.length < LIMIT; k++) {{
                var n = iter.snapshotItem(k);
                if (n && n.nodeType === 1) {{
                    results.push(buildMatch(n, null));
                }} else if (n && n.nodeType === 2) {{
                    // attribute node — surface as a value
                    results.push({{
                        tag: '@' + n.name,
                        text: n.value || '',
                        path: '',
                        attributes: {{}},
                        value: n.value || '',
                    }});
                }} else if (n && n.nodeType === 3) {{
                    // text node
                    results.push({{
                        tag: '#text',
                        text: (n.textContent || '').slice(0, MAX_TEXT),
                        path: '',
                        attributes: {{}},
                        value: (n.textContent || ''),
                    }});
                }}
            }}
        }} catch(e) {{
            return JSON.stringify({{ error: 'xpath failed: ' + e.message }});
        }}
    }} else {{
        // Default to body so find_by_text has something to filter.
        if (document.body) results.push(buildMatch(document.body, null));
    }}

    if (TEXT_NEEDLE) {{
        var rx = null;
        try {{ if (REGEX) rx = new RegExp(TEXT_NEEDLE); }} catch(e) {{}}
        results = results.filter(function(m) {{
            return rx ? rx.test(m.text) : (m.text || '').indexOf(TEXT_NEEDLE) >= 0;
        }});
    }}

    return JSON.stringify({{ matches: results }});
}})()
"#
    );

    let result = page
        .evaluate(js.as_str())
        .await
        .map_err(|e| Error::ToolExecution(format!("select_live failed: {e}").into()))?;

    let raw: String = result.into_value().unwrap_or_else(|_| "{}".to_string());
    let parsed: Value = serde_json::from_str(&raw).unwrap_or(Value::Null);

    if let Some(err) = parsed.get("error").and_then(|v| v.as_str()) {
        return Err(Error::ToolExecution(err.to_string().into()));
    }

    let arr = parsed
        .get("matches")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    let mut out = Vec::with_capacity(arr.len());
    for v in arr {
        let attrs_map = v
            .get("attributes")
            .and_then(|a| a.as_object())
            .cloned()
            .unwrap_or_default();
        out.push(Match {
            tag: v.get("tag").and_then(|s| s.as_str()).unwrap_or("").into(),
            text: v.get("text").and_then(|s| s.as_str()).unwrap_or("").into(),
            html: v.get("html").and_then(|s| s.as_str()).map(str::to_string),
            attributes: attrs_map,
            value: v.get("value").and_then(|s| s.as_str()).map(str::to_string),
            path: v.get("path").and_then(|s| s.as_str()).unwrap_or("").into(),
        });
    }
    Ok(out)
}

fn filter_by_text<'a>(
    elements: Vec<ElementRef<'a>>,
    needle: &str,
    regex: bool,
) -> Result<Vec<ElementRef<'a>>> {
    if regex {
        let rx = regex::Regex::new(needle)
            .map_err(|e| Error::ToolExecution(format!("invalid regex '{needle}': {e}").into()))?;
        Ok(elements
            .into_iter()
            .filter(|el| rx.is_match(&element_text(*el)))
            .collect())
    } else {
        Ok(elements
            .into_iter()
            .filter(|el| element_text(*el).contains(needle))
            .collect())
    }
}

fn element_text(el: ElementRef<'_>) -> String {
    el.text().collect::<Vec<_>>().join("").trim().to_string()
}

fn element_to_match(el: ElementRef<'_>, pseudo: &Option<Pseudo>, include_html: bool) -> Match {
    let tag = el.value().name().to_string();
    let text = element_text(el);

    let mut attrs = Map::new();
    for (name, value) in el.value().attrs() {
        attrs.insert(name.to_string(), Value::String(value.to_string()));
    }

    let value = match pseudo {
        Some(Pseudo::Text) => Some(text.clone()),
        Some(Pseudo::Attr(name)) => Some(el.value().attr(name).unwrap_or("").to_string()),
        None => None,
    };

    Match {
        tag,
        text,
        html: if include_html { Some(el.html()) } else { None },
        attributes: attrs,
        value,
        path: element_path(el),
    }
}

/// Build a Scrapling-compatible structural path for an element. Produces
/// strings like `html > body > div:nth-of-type(2) > a`, useful as a stable
/// identifier across runs.
fn element_path(el: ElementRef<'_>) -> String {
    // ego-tree node id is opaque, so we walk parents instead.
    let mut parts: Vec<String> = Vec::new();
    let mut current = Some(el);
    while let Some(node) = current {
        let tag = node.value().name();
        if let Some(id) = node.value().attr("id") {
            if !id.is_empty() && !id.starts_with(|c: char| c.is_ascii_digit()) {
                parts.push(format!("#{id}"));
                break;
            }
        }
        if let Some(parent_node) = node.parent().and_then(ElementRef::wrap) {
            let same_tag: Vec<_> = parent_node
                .children()
                .filter_map(ElementRef::wrap)
                .filter(|c| c.value().name() == tag)
                .collect();
            if same_tag.len() == 1 {
                parts.push(tag.to_string());
            } else {
                let idx = same_tag
                    .iter()
                    .position(|c| c.id() == node.id())
                    .map(|i| i + 1)
                    .unwrap_or(1);
                parts.push(format!("{tag}:nth-of-type({idx})"));
            }
            current = Some(parent_node);
        } else {
            parts.push(tag.to_string());
            break;
        }
    }
    parts.reverse();
    parts.join(" > ")
}

/// Render a list of matches as a JSON Value the tool can return.
pub fn matches_to_json(matches: &[Match]) -> Value {
    let arr: Vec<Value> = matches.iter().map(Match::to_json).collect();
    json!({
        "count": arr.len(),
        "matches": arr,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
        <html><body>
            <h1 id="title">Hello World</h1>
            <ul class="items">
                <li class="item" data-id="1">Apple</li>
                <li class="item" data-id="2">Banana</li>
                <li class="item special" data-id="3">Cherry</li>
            </ul>
            <a href="https://example.com/a">a</a>
            <a href="https://example.com/b">b</a>
        </body></html>
    "#;

    #[test]
    fn css_basic_select() {
        let params = SelectParams {
            css: Some("li.item".into()),
            ..Default::default()
        };
        let m = select_static(SAMPLE, &params).unwrap();
        assert_eq!(m.len(), 3);
        assert_eq!(m[0].text, "Apple");
        assert_eq!(m[2].tag, "li");
    }

    #[test]
    fn css_text_pseudo_extracts_text_value() {
        let params = SelectParams {
            css: Some("h1#title::text".into()),
            ..Default::default()
        };
        let m = select_static(SAMPLE, &params).unwrap();
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].value.as_deref(), Some("Hello World"));
    }

    #[test]
    fn css_attr_pseudo_extracts_attribute() {
        let params = SelectParams {
            css: Some("a::attr(href)".into()),
            ..Default::default()
        };
        let m = select_static(SAMPLE, &params).unwrap();
        assert_eq!(m.len(), 2);
        assert_eq!(m[0].value.as_deref(), Some("https://example.com/a"));
        assert_eq!(m[1].value.as_deref(), Some("https://example.com/b"));
    }

    #[test]
    fn find_by_text_substring() {
        let params = SelectParams {
            css: Some("li.item".into()),
            find_by_text: Some("anan".into()),
            ..Default::default()
        };
        let m = select_static(SAMPLE, &params).unwrap();
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].text, "Banana");
    }

    #[test]
    fn find_by_text_regex() {
        let params = SelectParams {
            css: Some("li.item".into()),
            find_by_text: Some("^Ch".into()),
            regex: true,
            ..Default::default()
        };
        let m = select_static(SAMPLE, &params).unwrap();
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].text, "Cherry");
    }

    #[test]
    fn invalid_css_selector_errors() {
        let params = SelectParams {
            css: Some(":::invalid".into()),
            ..Default::default()
        };
        let err = select_static(SAMPLE, &params).unwrap_err();
        assert!(err.to_string().contains("invalid CSS selector"));
    }

    #[test]
    fn limit_caps_results() {
        let params = SelectParams {
            css: Some("li.item".into()),
            limit: Some(2),
            ..Default::default()
        };
        let m = select_static(SAMPLE, &params).unwrap();
        assert_eq!(m.len(), 2);
    }

    #[test]
    fn xpath_in_static_mode_returns_clear_error() {
        let params = SelectParams {
            xpath: Some("//li".into()),
            ..Default::default()
        };
        let err = select_static(SAMPLE, &params).unwrap_err();
        assert!(err
            .to_string()
            .contains("xpath is only supported in live mode"));
    }
}
