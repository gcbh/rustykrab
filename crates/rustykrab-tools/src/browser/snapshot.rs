//! Accessibility-tree snapshot system modeled after OpenClaw's snapshot/ref pattern.
//!
//! Takes a snapshot of the page's accessibility tree, assigns stable numeric
//! refs to interactive elements, and returns a structured representation that
//! the agent can use for targeted actions (click ref 12, type ref 23 "hello").
//!
//! Two snapshot modes:
//! - **ai**: Compact text summary with numeric refs (default)
//! - **aria**: Full accessibility tree with `e`-prefixed refs (e.g., e12)

use chromiumoxide::Page;
use rustykrab_core::{Error, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

/// Maximum depth for accessibility tree traversal.
const DEFAULT_MAX_DEPTH: usize = 10;

/// A single element ref from a snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ElementRef {
    /// The ref identifier (numeric for ai mode, e-prefixed for aria mode).
    pub ref_id: String,
    /// CSS selector that can locate this element.
    pub selector: String,
    /// Element role (button, link, textbox, etc.).
    pub role: String,
    /// Human-readable name/label.
    pub name: String,
    /// Current value (for inputs).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    /// Whether the element is interactive.
    pub interactive: bool,
    /// Bounding box (x, y, width, height) if available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bounds: Option<[f64; 4]>,
}

/// Stores ref mappings from the most recent snapshot for each profile+tab.
pub struct SnapshotStore {
    /// Maps (profile, tab_key) -> { ref_id -> ElementRef }
    refs: Arc<Mutex<HashMap<String, HashMap<String, ElementRef>>>>,
}

impl SnapshotStore {
    pub fn new() -> Self {
        Self {
            refs: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Store refs from a snapshot.
    pub async fn store(&self, key: &str, refs: HashMap<String, ElementRef>) {
        let mut store = self.refs.lock().await;
        store.insert(key.to_string(), refs);
    }

    /// Look up an element ref.
    pub async fn get_ref(&self, key: &str, ref_id: &str) -> Option<ElementRef> {
        let store = self.refs.lock().await;
        store.get(key).and_then(|m| m.get(ref_id)).cloned()
    }

    /// Clear refs for a key.
    #[allow(dead_code)]
    pub async fn clear(&self, key: &str) {
        let mut store = self.refs.lock().await;
        store.remove(key);
    }
}

/// Snapshot mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapshotMode {
    /// Compact AI-friendly format with numeric refs.
    Ai,
    /// Full accessibility tree with e-prefixed refs.
    Aria,
}

/// Options for taking a snapshot.
#[derive(Debug, Clone)]
pub struct SnapshotOptions {
    pub mode: SnapshotMode,
    /// Only include interactive elements (buttons, links, inputs, etc.).
    pub interactive_only: bool,
    /// Compact output (fewer details).
    pub compact: bool,
    /// Maximum tree depth.
    pub max_depth: usize,
    /// CSS selector to scope the snapshot to a subtree.
    pub selector: Option<String>,
}

impl Default for SnapshotOptions {
    fn default() -> Self {
        Self {
            mode: SnapshotMode::Ai,
            interactive_only: false,
            compact: false,
            max_depth: DEFAULT_MAX_DEPTH,
            selector: None,
        }
    }
}

/// JavaScript that extracts the accessibility tree from a page.
///
/// Returns an array of objects with: tag, role, name, value, selector, interactive,
/// bounds (x, y, w, h), depth, children count.
const SNAPSHOT_JS: &str = r#"
(function() {
    const INTERACTIVE_ROLES = new Set([
        'button', 'link', 'textbox', 'checkbox', 'radio', 'combobox',
        'listbox', 'menuitem', 'menuitemcheckbox', 'menuitemradio',
        'option', 'searchbox', 'slider', 'spinbutton', 'switch',
        'tab', 'treeitem'
    ]);
    const INTERACTIVE_TAGS = new Set([
        'A', 'BUTTON', 'INPUT', 'SELECT', 'TEXTAREA', 'DETAILS', 'SUMMARY'
    ]);

    function getSelector(el) {
        if (el.id) return '#' + CSS.escape(el.id);
        if (el === document.body) return 'body';
        const tag = el.tagName.toLowerCase();
        const parent = el.parentElement;
        if (!parent) return tag;
        const siblings = Array.from(parent.children).filter(c => c.tagName === el.tagName);
        if (siblings.length === 1) return getSelector(parent) + ' > ' + tag;
        const idx = siblings.indexOf(el) + 1;
        return getSelector(parent) + ' > ' + tag + ':nth-of-type(' + idx + ')';
    }

    function isInteractive(el) {
        const role = (el.getAttribute('role') || '').toLowerCase();
        if (INTERACTIVE_ROLES.has(role)) return true;
        if (INTERACTIVE_TAGS.has(el.tagName)) return true;
        if (el.hasAttribute('onclick') || el.hasAttribute('tabindex')) return true;
        if (el.tagName === 'DIV' || el.tagName === 'SPAN') {
            const style = window.getComputedStyle(el);
            if (style.cursor === 'pointer') return true;
        }
        return false;
    }

    function getRole(el) {
        const explicit = el.getAttribute('role');
        if (explicit) return explicit.toLowerCase();
        const tag = el.tagName;
        if (tag === 'A') return 'link';
        if (tag === 'BUTTON') return 'button';
        if (tag === 'INPUT') {
            const type = (el.type || 'text').toLowerCase();
            if (type === 'checkbox') return 'checkbox';
            if (type === 'radio') return 'radio';
            if (type === 'submit' || type === 'button') return 'button';
            return 'textbox';
        }
        if (tag === 'SELECT') return 'combobox';
        if (tag === 'TEXTAREA') return 'textbox';
        if (tag === 'IMG') return 'img';
        if (tag === 'H1' || tag === 'H2' || tag === 'H3' || tag === 'H4' || tag === 'H5' || tag === 'H6') return 'heading';
        if (tag === 'NAV') return 'navigation';
        if (tag === 'MAIN') return 'main';
        if (tag === 'FORM') return 'form';
        if (tag === 'TABLE') return 'table';
        if (tag === 'UL' || tag === 'OL') return 'list';
        if (tag === 'LI') return 'listitem';
        return 'generic';
    }

    function getName(el) {
        const ariaLabel = el.getAttribute('aria-label');
        if (ariaLabel) return ariaLabel;
        const labelledBy = el.getAttribute('aria-labelledby');
        if (labelledBy) {
            const label = document.getElementById(labelledBy);
            if (label) return label.textContent.trim().substring(0, 100);
        }
        if (el.tagName === 'INPUT' || el.tagName === 'TEXTAREA' || el.tagName === 'SELECT') {
            const id = el.id;
            if (id) {
                const label = document.querySelector('label[for="' + CSS.escape(id) + '"]');
                if (label) return label.textContent.trim().substring(0, 100);
            }
            const placeholder = el.getAttribute('placeholder');
            if (placeholder) return placeholder;
            const title = el.getAttribute('title');
            if (title) return title;
        }
        if (el.tagName === 'IMG') return el.alt || '';
        if (el.tagName === 'A' || el.tagName === 'BUTTON') {
            return el.textContent.trim().substring(0, 100);
        }
        return el.textContent.trim().substring(0, 80);
    }

    const results = [];
    const MAX_DEPTH = arguments[0] || 10;
    const INTERACTIVE_ONLY = arguments[1] || false;
    const SCOPE_SELECTOR = arguments[2] || null;

    const root = SCOPE_SELECTOR ? document.querySelector(SCOPE_SELECTOR) : document.body;
    if (!root) return JSON.stringify([]);

    function walk(el, depth) {
        if (depth > MAX_DEPTH) return;
        if (el.nodeType !== 1) return;
        const style = window.getComputedStyle(el);
        if (style.display === 'none' || style.visibility === 'hidden') return;

        const interactive = isInteractive(el);
        if (!INTERACTIVE_ONLY || interactive) {
            const rect = el.getBoundingClientRect();
            const role = getRole(el);
            if (role !== 'generic' || interactive) {
                results.push({
                    tag: el.tagName.toLowerCase(),
                    role: role,
                    name: getName(el),
                    value: (el.tagName === 'INPUT' || el.tagName === 'TEXTAREA' || el.tagName === 'SELECT') ? (el.value || '') : null,
                    selector: getSelector(el),
                    interactive: interactive,
                    bounds: [Math.round(rect.x), Math.round(rect.y), Math.round(rect.width), Math.round(rect.height)],
                    depth: depth
                });
            }
        }

        for (const child of el.children) {
            walk(child, depth + 1);
        }
    }

    walk(root, 0);
    return JSON.stringify(results);
})()
"#;

/// Raw element data from the JS snapshot.
#[derive(Debug, Deserialize)]
struct RawElement {
    #[allow(dead_code)]
    tag: String,
    role: String,
    name: String,
    value: Option<String>,
    selector: String,
    interactive: bool,
    bounds: Option<[f64; 4]>,
    #[allow(dead_code)]
    depth: usize,
}

/// Take a snapshot of the page's accessibility tree.
pub async fn take_snapshot(
    page: &Page,
    options: &SnapshotOptions,
    store: &SnapshotStore,
    store_key: &str,
) -> Result<serde_json::Value> {
    let selector_arg = options
        .selector
        .as_deref()
        .map(|s| serde_json::to_string(s).unwrap_or_else(|_| "null".to_string()))
        .unwrap_or_else(|| "null".to_string());

    // Execute the snapshot JS with arguments
    let eval_js = format!(
        "({SNAPSHOT_JS}).apply(null, [{}, {}, {}])",
        options.max_depth,
        if options.interactive_only {
            "true"
        } else {
            "false"
        },
        selector_arg,
    );

    let result = page
        .evaluate(eval_js)
        .await
        .map_err(|e| Error::ToolExecution(format!("snapshot failed: {e}").into()))?;

    let raw_json: String = result.into_value().unwrap_or_else(|_| "[]".to_string());
    let elements: Vec<RawElement> = serde_json::from_str(&raw_json).unwrap_or_default();

    // Assign refs and build the output
    let mut ref_map = HashMap::new();
    let mut output_elements = Vec::new();
    let mut ref_counter = 0usize;

    for elem in &elements {
        ref_counter += 1;
        let ref_id = match options.mode {
            SnapshotMode::Ai => format!("{ref_counter}"),
            SnapshotMode::Aria => format!("e{ref_counter}"),
        };

        let element_ref = ElementRef {
            ref_id: ref_id.clone(),
            selector: elem.selector.clone(),
            role: elem.role.clone(),
            name: elem.name.clone(),
            value: elem.value.clone(),
            interactive: elem.interactive,
            bounds: elem.bounds,
        };

        ref_map.insert(ref_id.clone(), element_ref.clone());

        if options.compact {
            // Compact: just ref, role, and name
            if elem.interactive || !options.interactive_only {
                let mut line = format!("[{ref_id}] {}", elem.role);
                if !elem.name.is_empty() {
                    line.push_str(&format!(": \"{}\"", truncate(&elem.name, 60)));
                }
                if let Some(ref val) = elem.value {
                    if !val.is_empty() {
                        line.push_str(&format!(" = \"{}\"", truncate(val, 40)));
                    }
                }
                output_elements.push(serde_json::Value::String(line));
            }
        } else {
            output_elements.push(serde_json::json!({
                "ref": ref_id,
                "role": elem.role,
                "name": elem.name,
                "value": elem.value,
                "interactive": elem.interactive,
                "bounds": elem.bounds,
            }));
        }
    }

    // Store refs for later use by act actions
    store.store(store_key, ref_map).await;

    let url = page.url().await.ok().flatten().unwrap_or_default();
    let title = page.get_title().await.ok().flatten().unwrap_or_default();

    Ok(serde_json::json!({
        "url": url,
        "title": title,
        "mode": match options.mode { SnapshotMode::Ai => "ai", SnapshotMode::Aria => "aria" },
        "elements": output_elements,
        "count": ref_counter,
        "interactive_count": elements.iter().filter(|e| e.interactive).count(),
        "note": format!(
            "Use ref numbers with 'act' action to interact with elements (e.g., act click {}1)",
            if options.mode == SnapshotMode::Aria { "e" } else { "" }
        )
    }))
}

/// Truncate a string to a max length, appending "..." if truncated.
fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let mut end = max.saturating_sub(3);
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}...", &s[..end])
    }
}
