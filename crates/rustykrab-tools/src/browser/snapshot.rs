//! Accessibility-tree snapshot system modeled after OpenClaw's snapshot/ref pattern,
//! with browser-use-inspired enhancements.
//!
//! Takes a snapshot of the page's accessibility tree, assigns stable numeric
//! refs to interactive elements, and returns a structured representation that
//! the agent can use for targeted actions (click ref 12, type ref 23 "hello").
//!
//! Two snapshot modes:
//! - **ai**: Compact text summary with numeric refs (default)
//! - **aria**: Full accessibility tree with `e`-prefixed refs (e.g., e12)
//!
//! Enhancements over the baseline AX-tree extractor:
//! - Pierces open shadow roots (Web Components, Angular Material, etc.).
//! - Pierces same-origin iframes.
//! - Filters out occluded / zero-size / fully transparent elements.
//! - Prefers stable selectors (`data-testid`, `aria-label`, `name`) over
//!   fragile nth-of-type chains.
//! - Optional numbered highlight overlay for screenshots.

use chromiumoxide::Page;
use rustykrab_core::{Error, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

/// Maximum depth for accessibility tree traversal.
const DEFAULT_MAX_DEPTH: usize = 10;

/// Marker between segments of a shadow-DOM piercing selector.
#[allow(dead_code)]
pub(crate) const SHADOW_SEP: &str = " >>> ";
/// Marker between an iframe selector and the inner-document selector.
#[allow(dead_code)]
pub(crate) const IFRAME_SEP: &str = " ||| ";

/// A single element ref from a snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ElementRef {
    /// The ref identifier (numeric for ai mode, e-prefixed for aria mode).
    pub ref_id: String,
    /// Primary selector, possibly chained via `>>>` (shadow) or `|||` (iframe).
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
    /// If true, paint numbered overlay boxes on each snapshotted ref so a
    /// subsequent screenshot shows the labels visually. Overlays auto-clear on
    /// the next snapshot.
    pub highlight: bool,
}

impl Default for SnapshotOptions {
    fn default() -> Self {
        Self {
            mode: SnapshotMode::Ai,
            interactive_only: false,
            compact: false,
            max_depth: DEFAULT_MAX_DEPTH,
            selector: None,
            highlight: false,
        }
    }
}

/// JavaScript that extracts the accessibility tree from a page.
///
/// Walks the document, open shadow roots, and same-origin iframes. Returns an
/// array of objects with: tag, role, name, value, selector (possibly chained),
/// interactive, bounds (x, y, w, h), depth.
///
/// Args: [maxDepth, interactiveOnly, scopeSelector, highlight]
const SNAPSHOT_JS: &str = r#"
(function() {
    var INTERACTIVE_ROLES = new Set([
        'button', 'link', 'textbox', 'checkbox', 'radio', 'combobox',
        'listbox', 'menuitem', 'menuitemcheckbox', 'menuitemradio',
        'option', 'searchbox', 'slider', 'spinbutton', 'switch',
        'tab', 'treeitem'
    ]);
    var INTERACTIVE_TAGS = new Set([
        'A', 'BUTTON', 'INPUT', 'SELECT', 'TEXTAREA', 'DETAILS', 'SUMMARY'
    ]);
    var SHADOW_SEP = ' >>> ';
    var IFRAME_SEP = ' ||| ';

    var MAX_DEPTH = arguments[0] || 10;
    var INTERACTIVE_ONLY = arguments[1] || false;
    var SCOPE_SELECTOR = arguments[2] || null;
    var HIGHLIGHT = arguments[3] || false;

    // Always clear stale highlights from a previous snapshot, even if we are
    // not painting new ones this call.
    var STALE_HIGHLIGHT_ID = '__rustykrab_overlay__';
    var stale = document.getElementById(STALE_HIGHLIGHT_ID);
    if (stale) stale.remove();

    function csqEscape(s) {
        if (window.CSS && CSS.escape) return CSS.escape(s);
        return String(s).replace(/[^a-zA-Z0-9_-]/g, function(c) { return '\\' + c; });
    }

    // Build a CSS selector for an element, scoped to its owner Document or
    // ShadowRoot. Prefers stable attributes.
    function localSelector(el) {
        if (el.id && !/^[0-9]/.test(el.id)) return '#' + csqEscape(el.id);
        var tid = el.getAttribute && el.getAttribute('data-testid');
        if (tid) return el.tagName.toLowerCase() + '[data-testid="' + cssAttrEscape(tid) + '"]';
        var dataQa = el.getAttribute && el.getAttribute('data-qa');
        if (dataQa) return el.tagName.toLowerCase() + '[data-qa="' + cssAttrEscape(dataQa) + '"]';
        var name = el.getAttribute && el.getAttribute('name');
        if (name && (el.tagName === 'INPUT' || el.tagName === 'TEXTAREA' || el.tagName === 'SELECT' || el.tagName === 'BUTTON')) {
            return el.tagName.toLowerCase() + '[name="' + cssAttrEscape(name) + '"]';
        }
        var aria = el.getAttribute && el.getAttribute('aria-label');
        if (aria && aria.length < 100) {
            return el.tagName.toLowerCase() + '[aria-label="' + cssAttrEscape(aria) + '"]';
        }
        // Fallback: structural path within the local root.
        return structuralPath(el);
    }

    function cssAttrEscape(s) {
        return String(s).replace(/\\/g, '\\\\').replace(/"/g, '\\"');
    }

    function structuralPath(el) {
        var parts = [];
        var node = el;
        while (node && node.nodeType === 1) {
            var parent = node.parentElement;
            // Stop when we cross out of the local root (shadow/iframe boundary).
            if (!parent) break;
            var tag = node.tagName.toLowerCase();
            if (node.id && !/^[0-9]/.test(node.id)) {
                parts.unshift('#' + csqEscape(node.id));
                break;
            }
            var siblings = Array.from(parent.children).filter(function(c) {
                return c.tagName === node.tagName;
            });
            if (siblings.length === 1) {
                parts.unshift(tag);
            } else {
                var idx = siblings.indexOf(node) + 1;
                parts.unshift(tag + ':nth-of-type(' + idx + ')');
            }
            node = parent;
        }
        return parts.join(' > ') || el.tagName.toLowerCase();
    }

    // Compose a chained selector that pierces shadow/iframe boundaries.
    // chain is an array like [{kind:'doc', el:host}, {kind:'shadow', host:host}, {kind:'iframe', host:iframe}]
    // Each segment contributes a localSelector(el) plus an appropriate separator.
    function chainedSelector(el, chain) {
        var localPart = localSelector(el);
        if (!chain.length) return localPart;
        var s = '';
        for (var i = 0; i < chain.length; i++) {
            var seg = chain[i];
            var hostSel = localSelector(seg.host);
            if (i === 0) {
                s = hostSel;
            } else {
                s = s + (chain[i - 1].kind === 'shadow' ? SHADOW_SEP : IFRAME_SEP) + hostSel;
            }
        }
        var lastBoundary = chain[chain.length - 1].kind === 'shadow' ? SHADOW_SEP : IFRAME_SEP;
        return s + lastBoundary + localPart;
    }

    function isInteractive(el) {
        var role = (el.getAttribute && (el.getAttribute('role') || '')).toLowerCase();
        if (INTERACTIVE_ROLES.has(role)) return true;
        if (INTERACTIVE_TAGS.has(el.tagName)) return true;
        if (el.hasAttribute && (el.hasAttribute('onclick') || el.hasAttribute('tabindex'))) return true;
        if (el.tagName === 'DIV' || el.tagName === 'SPAN') {
            var style = window.getComputedStyle(el);
            if (style.cursor === 'pointer') return true;
        }
        return false;
    }

    function getRole(el) {
        var explicit = el.getAttribute && el.getAttribute('role');
        if (explicit) return explicit.toLowerCase();
        var tag = el.tagName;
        if (tag === 'A') return 'link';
        if (tag === 'BUTTON') return 'button';
        if (tag === 'INPUT') {
            var type = (el.type || 'text').toLowerCase();
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
        if (!el.getAttribute) return '';
        var ariaLabel = el.getAttribute('aria-label');
        if (ariaLabel) return ariaLabel;
        var labelledBy = el.getAttribute('aria-labelledby');
        if (labelledBy) {
            var label = (el.getRootNode && el.getRootNode().getElementById)
                ? el.getRootNode().getElementById(labelledBy)
                : document.getElementById(labelledBy);
            if (label) return (label.textContent || '').trim().substring(0, 100);
        }
        if (el.tagName === 'INPUT' || el.tagName === 'TEXTAREA' || el.tagName === 'SELECT') {
            var id = el.id;
            if (id) {
                var root = el.getRootNode ? el.getRootNode() : document;
                var assoc = root.querySelector ? root.querySelector('label[for="' + cssAttrEscape(id) + '"]') : null;
                if (assoc) return (assoc.textContent || '').trim().substring(0, 100);
            }
            var placeholder = el.getAttribute('placeholder');
            if (placeholder) return placeholder;
            var title = el.getAttribute('title');
            if (title) return title;
        }
        if (el.tagName === 'IMG') return el.alt || '';
        if (el.tagName === 'A' || el.tagName === 'BUTTON') {
            return (el.textContent || '').trim().substring(0, 100);
        }
        return (el.textContent || '').trim().substring(0, 80);
    }

    // Visibility check: layout box, computed style, opacity, viewport overlap,
    // and a center-point occlusion probe.
    function isVisible(el) {
        var style = window.getComputedStyle(el);
        if (style.display === 'none' || style.visibility === 'hidden') return false;
        if (parseFloat(style.opacity || '1') === 0) return false;
        var rect = el.getBoundingClientRect();
        if (rect.width <= 0 || rect.height <= 0) return false;
        // Off the document entirely (negative side, beyond doc) — keep, the
        // page may scroll. We only filter purely degenerate cases above.
        return true;
    }

    // Returns true if `el` is occluded at its center by a non-descendant node.
    // Skipped for elements outside the viewport (we cannot probe those).
    function isOccluded(el) {
        var rect = el.getBoundingClientRect();
        if (rect.width <= 0 || rect.height <= 0) return true;
        var vw = window.innerWidth || document.documentElement.clientWidth;
        var vh = window.innerHeight || document.documentElement.clientHeight;
        // If the center is outside the viewport, we cannot probe; consider visible.
        var cx = rect.x + rect.width / 2;
        var cy = rect.y + rect.height / 2;
        if (cx < 0 || cy < 0 || cx > vw || cy > vh) return false;
        var root = el.getRootNode ? el.getRootNode() : document;
        var topEl = root.elementFromPoint ? root.elementFromPoint(cx, cy) : document.elementFromPoint(cx, cy);
        if (!topEl) return false;
        if (topEl === el) return false;
        if (el.contains && el.contains(topEl)) return false;
        if (topEl.contains && topEl.contains(el)) return false;
        return true;
    }

    var results = [];
    var refCounter = 0;

    var rootDoc = SCOPE_SELECTOR ? document.querySelector(SCOPE_SELECTOR) : document.body;
    if (!rootDoc) return JSON.stringify({ elements: [], note: 'scope selector did not match' });

    function walk(node, depth, chain) {
        if (depth > MAX_DEPTH) return;
        if (!node) return;
        // Element-like node.
        if (node.nodeType !== 1) return;

        if (!isVisible(node)) return;
        // Skip occluded interactive candidates; non-interactive structural nodes
        // we still descend into (their children may be visible).
        var occluded = isOccluded(node);

        var interactive = isInteractive(node);
        var role = getRole(node);
        var collect = (interactive || role !== 'generic') && !occluded;
        if (INTERACTIVE_ONLY && !interactive) collect = false;

        if (collect) {
            var rect = node.getBoundingClientRect();
            results.push({
                node: node,
                chain: chain.slice(),
                tag: node.tagName.toLowerCase(),
                role: role,
                name: getName(node),
                value: (node.tagName === 'INPUT' || node.tagName === 'TEXTAREA' || node.tagName === 'SELECT') ? (node.value || '') : null,
                selector: chainedSelector(node, chain),
                interactive: interactive,
                bounds: [Math.round(rect.x), Math.round(rect.y), Math.round(rect.width), Math.round(rect.height)],
                depth: depth
            });
        }

        // Descend into open shadow root, if any.
        if (node.shadowRoot && node.shadowRoot.mode !== 'closed') {
            var children = node.shadowRoot.children;
            for (var i = 0; i < children.length; i++) {
                walk(children[i], depth + 1, chain.concat([{ kind: 'shadow', host: node }]));
            }
        }

        // Descend into same-origin iframe contentDocument.
        if (node.tagName === 'IFRAME') {
            try {
                var doc = node.contentDocument;
                if (doc && doc.body) {
                    var ic = doc.body.children;
                    for (var j = 0; j < ic.length; j++) {
                        walk(ic[j], depth + 1, chain.concat([{ kind: 'iframe', host: node }]));
                    }
                }
            } catch (e) { /* cross-origin: cannot pierce */ }
        }

        // Light DOM children.
        var lc = node.children;
        for (var k = 0; k < lc.length; k++) {
            walk(lc[k], depth + 1, chain);
        }
    }

    walk(rootDoc, 0, []);

    // Optional highlight overlay: numbered boxes anchored in document space.
    if (HIGHLIGHT) {
        var overlay = document.createElement('div');
        overlay.id = STALE_HIGHLIGHT_ID;
        overlay.style.cssText = 'position:fixed;inset:0;pointer-events:none;z-index:2147483647;';
        for (var r = 0; r < results.length; r++) {
            var item = results[r];
            // Only highlight elements visible in the current viewport.
            var b = item.bounds;
            if (!b) continue;
            var box = document.createElement('div');
            box.style.cssText =
                'position:absolute;border:2px solid #ff3b30;outline:1px solid #fff;' +
                'left:' + b[0] + 'px;top:' + b[1] + 'px;' +
                'width:' + b[2] + 'px;height:' + b[3] + 'px;' +
                'box-sizing:border-box;';
            var label = document.createElement('div');
            label.textContent = String(r + 1);
            label.style.cssText =
                'position:absolute;left:0;top:-16px;background:#ff3b30;color:#fff;' +
                'font:600 11px/14px system-ui,sans-serif;padding:0 4px;border-radius:2px;';
            box.appendChild(label);
            overlay.appendChild(box);
        }
        (document.body || document.documentElement).appendChild(overlay);
    }

    // Strip non-serializable fields before returning.
    var out = results.map(function(e) {
        return {
            tag: e.tag,
            role: e.role,
            name: e.name,
            value: e.value,
            selector: e.selector,
            interactive: e.interactive,
            bounds: e.bounds,
            depth: e.depth
        };
    });
    return JSON.stringify(out);
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

    let eval_js = format!(
        "({SNAPSHOT_JS}).apply(null, [{}, {}, {}, {}])",
        options.max_depth,
        if options.interactive_only {
            "true"
        } else {
            "false"
        },
        selector_arg,
        if options.highlight { "true" } else { "false" },
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
        "highlight": options.highlight,
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
