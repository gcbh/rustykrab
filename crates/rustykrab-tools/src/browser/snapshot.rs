//! Accessibility-tree snapshot system modeled after OpenClaw's snapshot/ref pattern,
//! with browser-use-inspired enhancements.
//!
//! Takes a snapshot of the page's accessibility tree, assigns snapshot-scoped
//! refs to interactive elements, and returns a structured representation that
//! the agent can use for targeted actions (click ref s4-12, type ref s4-23
//! "hello").
//!
//! Two snapshot modes:
//! - **ai**: Compact text summary with snapshot-scoped refs (default)
//! - **aria**: Full accessibility tree with `e`-marked refs (e.g., s4-e12)
//!
//! Enhancements over the baseline AX-tree extractor:
//! - Pierces open shadow roots (Web Components, Angular Material, etc.).
//! - Captures each reachable frame in its own execution context, including
//!   cross-origin frames when Chrome exposes that context through this target.
//! - Filters out occluded / zero-size / fully transparent elements.
//! - Uses a preferred attribute selector only when it uniquely identifies the
//!   element in its document/shadow root; duplicated framework attributes fall
//!   back to an exact structural path.
//! - Optional numbered highlight overlay for screenshots.

use chromiumoxide::cdp::browser_protocol::page::FrameId;
use chromiumoxide::cdp::js_protocol::runtime::{EvaluateParams, ExecutionContextId};
use chromiumoxide::Page;
use rustykrab_core::{Error, Result};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::time::{timeout, Duration, Instant};

/// Maximum depth for accessibility tree traversal.
///
/// Modern React/Angular/Vue SPAs nest interactive elements deeply — Instagram's
/// login inputs sit ~32 levels down, and Google/Amazon are similar. A shallow
/// limit makes the walker stop before reaching them, so a snapshot reports 0
/// interactive elements even though they're visible and functional. 50 covers
/// all practical cases; the walker is O(n) in DOM nodes, so the extra headroom
/// is cheap. Callers can still override via the `depth` snapshot parameter.
const DEFAULT_MAX_DEPTH: usize = 50;

/// Frame traversal is deliberately bounded. Ad-heavy pages can create hundreds
/// of short-lived frames; a snapshot must remain useful even when some are
/// detached or unresponsive.
const MAX_SNAPSHOT_FRAMES: usize = 100;
const MAX_FRAME_DEPTH: usize = 5;
const SNAPSHOT_DEADLINE: Duration = Duration::from_secs(10);
const PER_FRAME_DEADLINE: Duration = Duration::from_secs(2);

/// Marker between segments of a shadow-DOM piercing selector.
#[allow(dead_code)]
pub(crate) const SHADOW_SEP: &str = " >>> ";
/// Marker between an iframe selector and the inner-document selector.
#[allow(dead_code)]
pub(crate) const IFRAME_SEP: &str = " ||| ";

/// A single element ref from a snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ElementRef {
    /// The ref identifier, including the snapshot generation that produced it.
    pub ref_id: String,
    /// Primary selector, possibly chained via `>>>` (shadow) or `|||` (iframe).
    pub selector: String,
    /// CDP frame identifier when the element belongs to a child frame. Refs
    /// remain snapshot-scoped because frame identifiers can change on reload.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame_id: Option<String>,
    /// Frame URL captured for diagnostics and conservative stale-ref healing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame_url: Option<String>,
    /// OOPIF target that owns this element. Absent for the top-level document
    /// and in-process frames handled by chromiumoxide directly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_id: Option<String>,
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

/// Upper bound on cached snapshot keys. Each entry holds a full ref map
/// (potentially hundreds of `ElementRef`s with selectors and bounds), so
/// without a cap the store grows per `(profile, tab)` ever snapshotted.
const MAX_SNAPSHOT_KEYS: usize = 64;

/// Stores ref mappings from the most recent snapshot for each profile+tab.
///
/// LRU-bounded so closed tabs / stale profiles eventually drop out.
pub struct SnapshotStore {
    inner: Arc<Mutex<SnapshotInner>>,
}

struct SnapshotInner {
    /// Maps (profile, tab_key) -> { ref_id -> ElementRef }
    refs: HashMap<String, HashMap<String, ElementRef>>,
    /// Recency order: front = least-recently-used, back = most-recent.
    order: VecDeque<String>,
    /// Monotonic snapshot generation. Including this in every ref prevents an
    /// old numeric ref from silently selecting a different element after a
    /// later snapshot replaces the map for the same tab.
    next_generation: u64,
    /// Raw-CDP context for live page snapshots. Standalone tests and helpers
    /// intentionally remain valid without it.
    oopif_contexts: HashMap<String, OopifContext>,
}

#[derive(Debug, Clone)]
pub(crate) struct OopifContext {
    pub websocket_url: String,
    pub policy: super::config::SsrfPolicy,
}

impl SnapshotInner {
    fn touch(&mut self, key: &str) {
        if let Some(pos) = self.order.iter().position(|k| k == key) {
            self.order.remove(pos);
        }
        self.order.push_back(key.to_string());
    }

    fn evict_to_capacity(&mut self) {
        while self.refs.len() > MAX_SNAPSHOT_KEYS {
            match self.order.pop_front() {
                Some(oldest) => {
                    self.refs.remove(&oldest);
                    self.oopif_contexts.remove(&oldest);
                }
                None => break,
            }
        }
    }
}

impl SnapshotStore {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(SnapshotInner {
                refs: HashMap::new(),
                order: VecDeque::new(),
                next_generation: 1,
                oopif_contexts: HashMap::new(),
            })),
        }
    }

    pub(crate) async fn register_oopif_context(
        &self,
        key: &str,
        websocket_url: String,
        policy: super::config::SsrfPolicy,
    ) {
        let mut g = self.inner.lock().await;
        g.oopif_contexts.insert(
            key.to_string(),
            OopifContext {
                websocket_url,
                policy,
            },
        );
    }

    pub(crate) async fn oopif_context(&self, key: &str) -> Option<OopifContext> {
        self.inner.lock().await.oopif_contexts.get(key).cloned()
    }

    /// Allocate a generation for one snapshot.
    async fn allocate_generation(&self) -> u64 {
        let mut g = self.inner.lock().await;
        let generation = g.next_generation;
        g.next_generation = g.next_generation.saturating_add(1);
        generation
    }

    /// Store refs from a snapshot.
    pub async fn store(&self, key: &str, refs: HashMap<String, ElementRef>) {
        let mut g = self.inner.lock().await;
        g.refs.insert(key.to_string(), refs);
        g.touch(key);
        g.evict_to_capacity();
    }

    /// Look up an element ref.
    pub async fn get_ref(&self, key: &str, ref_id: &str) -> Option<ElementRef> {
        let mut g = self.inner.lock().await;
        let hit = g.refs.get(key).and_then(|m| m.get(ref_id)).cloned();
        if hit.is_some() {
            g.touch(key);
        }
        hit
    }

    /// Find all refs under `key` whose role and name match the given identity.
    ///
    /// Used by `act`'s self-heal: after a stale-ref failure we re-snapshot and
    /// look for the *same logical element* by role+name (ref ids are positional
    /// and change between snapshots, so they can't be reused). The caller heals
    /// only on a unique match and escalates on none or several.
    pub async fn find_by_identity(
        &self,
        key: &str,
        role: &str,
        name: &str,
        frame_url: Option<&str>,
    ) -> Vec<ElementRef> {
        let g = self.inner.lock().await;
        g.refs
            .get(key)
            .map(|m| {
                m.values()
                    .filter(|r| {
                        r.role == role
                            && r.name == name
                            && frame_url.is_none_or(|url| r.frame_url.as_deref() == Some(url))
                    })
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Clear refs for a key.
    #[allow(dead_code)]
    pub async fn clear(&self, key: &str) {
        let mut g = self.inner.lock().await;
        g.refs.remove(key);
        g.oopif_contexts.remove(key);
        if let Some(pos) = g.order.iter().position(|k| k == key) {
            g.order.remove(pos);
        }
    }

    /// Clear every snapshot associated with a browser profile.
    ///
    /// Recovering a profile replaces its CDP connection and, for managed
    /// browsers, the Chrome process itself. Refs held by *other* conversations
    /// are just as stale as the ref that detected the failure, so invalidating
    /// only the initiating tab would leave cross-session capabilities pointing
    /// into a browser that no longer exists.
    pub async fn clear_profile(&self, profile: &str) {
        let belongs_to_profile = |key: &str| {
            key.split_once(':')
                .and_then(|(_, rest)| rest.split_once(':'))
                .is_some_and(|(key_profile, _)| key_profile == profile)
        };
        let mut g = self.inner.lock().await;
        g.refs.retain(|key, _| !belongs_to_profile(key));
        g.oopif_contexts.retain(|key, _| !belongs_to_profile(key));
        g.order.retain(|key| !belongs_to_profile(key));
    }
}

/// Snapshot mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapshotMode {
    /// Compact AI-friendly format with snapshot-scoped refs.
    Ai,
    /// Full accessibility tree with e-marked snapshot-scoped refs.
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
/// Walks one document and its open shadow roots. Rust invokes this once per
/// reachable frame execution context, which avoids the browser same-origin
/// restriction on `iframe.contentDocument`. Returns an
/// array of objects with: tag, role, name, value, selector (possibly chained),
/// interactive, bounds (x, y, w, h), depth.
///
/// Args: [maxDepth, interactiveOnly, scopeSelector, highlight]
pub(crate) const SNAPSHOT_JS: &str = r#"
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

    var MAX_DEPTH = arguments[0] || 50;
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

    // A selector is useful only if it names this element and no other one in
    // its local root. Component frameworks such as Wix deliberately reuse
    // data-testid="linkElement" across a page; accepting that as an identity
    // made several different refs click the first link in the document.
    function uniqueInLocalRoot(el, selector) {
        try {
            var root = el.getRootNode ? el.getRootNode() : document;
            if (!root || !root.querySelectorAll) return false;
            var matches = root.querySelectorAll(selector);
            return matches.length === 1 && matches[0] === el;
        } catch (e) {
            return false;
        }
    }

    // Build a CSS selector for an element, scoped to its owner Document or
    // ShadowRoot. Stable attributes are preferred only when unique.
    function localSelector(el) {
        if (el.id && !/^[0-9]/.test(el.id)) {
            var idSelector = '#' + csqEscape(el.id);
            if (uniqueInLocalRoot(el, idSelector)) return idSelector;
        }
        var tid = el.getAttribute && el.getAttribute('data-testid');
        if (tid) {
            var testSelector = el.tagName.toLowerCase() + '[data-testid="' + cssAttrEscape(tid) + '"]';
            if (uniqueInLocalRoot(el, testSelector)) return testSelector;
        }
        var dataQa = el.getAttribute && el.getAttribute('data-qa');
        if (dataQa) {
            var qaSelector = el.tagName.toLowerCase() + '[data-qa="' + cssAttrEscape(dataQa) + '"]';
            if (uniqueInLocalRoot(el, qaSelector)) return qaSelector;
        }
        var name = el.getAttribute && el.getAttribute('name');
        if (name && (el.tagName === 'INPUT' || el.tagName === 'TEXTAREA' || el.tagName === 'SELECT' || el.tagName === 'BUTTON')) {
            var nameSelector = el.tagName.toLowerCase() + '[name="' + cssAttrEscape(name) + '"]';
            if (uniqueInLocalRoot(el, nameSelector)) return nameSelector;
        }
        var aria = el.getAttribute && el.getAttribute('aria-label');
        if (aria && aria.length < 100) {
            var ariaSelector = el.tagName.toLowerCase() + '[aria-label="' + cssAttrEscape(aria) + '"]';
            if (uniqueInLocalRoot(el, ariaSelector)) return ariaSelector;
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

    // Compose a chained selector that pierces shadow boundaries.
    // Iframes are captured independently by their CDP execution context.
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
                s = s + SHADOW_SEP + hostSel;
            }
        }
        return s + SHADOW_SEP + localPart;
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
            if (type === 'file') return 'filechooser';
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

        var fileInput = node.tagName === 'INPUT' && (node.type || '').toLowerCase() === 'file';
        // Hidden file inputs are intentionally retained. Upload controls often
        // hide the native input behind a styled button, while CDP can still set
        // files on the input safely without opening the native chooser.
        if (!fileInput && !isVisible(node)) return;
        // Skip occluded interactive candidates; non-interactive structural nodes
        // we still descend into (their children may be visible).
        var occluded = fileInput ? false : isOccluded(node);

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
                value: fileInput ? null : ((node.tagName === 'INPUT' || node.tagName === 'TEXTAREA' || node.tagName === 'SELECT') ? (node.value || '') : null),
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
})
"#;

/// Raw element data from the JS snapshot.
#[derive(Debug, Deserialize)]
pub(crate) struct RawElement {
    #[allow(dead_code)]
    pub(crate) tag: String,
    pub(crate) role: String,
    pub(crate) name: String,
    pub(crate) value: Option<String>,
    pub(crate) selector: String,
    pub(crate) interactive: bool,
    pub(crate) bounds: Option<[f64; 4]>,
    #[allow(dead_code)]
    pub(crate) depth: usize,
}

struct CapturedElement {
    element: RawElement,
    frame_id: Option<String>,
    frame_url: Option<String>,
    target_id: Option<String>,
}

async fn evaluate_document_snapshot(
    page: &Page,
    eval_js: &str,
    context_id: Option<ExecutionContextId>,
) -> Result<Vec<RawElement>> {
    let mut params = EvaluateParams::builder()
        .expression(eval_js)
        .return_by_value(true);
    if let Some(context_id) = context_id {
        params = params.context_id(context_id);
    }
    let params = params
        .build()
        .map_err(|e| Error::ToolExecution(format!("invalid snapshot evaluation: {e}").into()))?;
    let result = page
        .evaluate_expression(params)
        .await
        .map_err(|e| Error::ToolExecution(format!("snapshot evaluation failed: {e}").into()))?;
    let raw_json: String = result.into_value().unwrap_or_else(|_| "[]".to_string());
    serde_json::from_str(&raw_json)
        .map_err(|e| Error::ToolExecution(format!("invalid snapshot result: {e}").into()))
}

async fn detect_captcha(page: &Page) -> serde_json::Value {
    let script = r#"(function() {
        var providers = new Set();
        var nodes = document.querySelectorAll('iframe[src], iframe[title], [data-sitekey], .g-recaptcha, .h-captcha, .cf-turnstile');
        for (var i = 0; i < nodes.length; i++) {
            var value = ((nodes[i].getAttribute('src') || '') + ' ' +
                (nodes[i].getAttribute('title') || '') + ' ' +
                (nodes[i].className || '')).toLowerCase();
            if (value.includes('recaptcha') || nodes[i].classList.contains('g-recaptcha')) providers.add('recaptcha');
            if (value.includes('hcaptcha') || nodes[i].classList.contains('h-captcha')) providers.add('hcaptcha');
            if (value.includes('turnstile') || nodes[i].classList.contains('cf-turnstile')) providers.add('cloudflare-turnstile');
            if (nodes[i].hasAttribute('data-sitekey') && providers.size === 0) providers.add('unknown');
        }
        return {detected: providers.size > 0, providers: Array.from(providers)};
    })()"#;
    match timeout(Duration::from_secs(1), page.evaluate(script)).await {
        Ok(Ok(result)) => result.into_value::<serde_json::Value>().unwrap_or_else(
            |_| serde_json::json!({"detected":false,"providers":[],"status":"unverified"}),
        ),
        _ => serde_json::json!({
            "detected": false,
            "providers": [],
            "status": "unverified",
        }),
    }
}

async fn frame_depth(page: &Page, frame: &FrameId, main: &FrameId) -> Option<usize> {
    let mut current = frame.clone();
    for depth in 1..=(MAX_FRAME_DEPTH + 1) {
        let parent = timeout(
            Duration::from_millis(250),
            page.frame_parent(current.clone()),
        )
        .await
        .ok()?
        .ok()??;
        if &parent == main {
            return Some(depth);
        }
        current = parent;
    }
    Some(MAX_FRAME_DEPTH + 1)
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
        "({SNAPSHOT_JS})({}, {}, {}, {})",
        options.max_depth,
        if options.interactive_only {
            "true"
        } else {
            "false"
        },
        selector_arg,
        if options.highlight { "true" } else { "false" },
    );

    let started = Instant::now();
    let main_elements = timeout(
        PER_FRAME_DEADLINE,
        evaluate_document_snapshot(page, &eval_js, None),
    )
    .await
    .map_err(|_| Error::ToolExecution("main-frame snapshot timed out".into()))??;
    let mut elements: Vec<CapturedElement> = main_elements
        .into_iter()
        .map(|element| CapturedElement {
            element,
            frame_id: None,
            frame_url: None,
            target_id: None,
        })
        .collect();

    let main_frame = timeout(Duration::from_secs(1), page.mainframe())
        .await
        .ok()
        .and_then(std::result::Result::ok)
        .flatten();
    let frames = timeout(Duration::from_secs(1), page.frames())
        .await
        .ok()
        .and_then(std::result::Result::ok)
        .unwrap_or_default();
    let mut page_frame_ids: HashSet<String> = frames
        .iter()
        .map(|frame| frame.as_ref().to_string())
        .collect();
    page_frame_ids.insert(page.target_id().inner().clone());
    let mut frames_seen = frames
        .len()
        .saturating_sub(usize::from(main_frame.is_some()));
    let mut frames_included = 0usize;
    let mut frames_skipped = Vec::new();
    let mut included_frame_ids = HashSet::new();

    if let Some(main_frame) = main_frame {
        for frame in frames
            .into_iter()
            .filter(|frame| frame != &main_frame)
            .take(MAX_SNAPSHOT_FRAMES)
        {
            if started.elapsed() >= SNAPSHOT_DEADLINE {
                frames_skipped.push("snapshot deadline reached".to_string());
                break;
            }
            let frame_id = frame.as_ref().to_string();
            let depth = frame_depth(page, &frame, &main_frame).await;
            if depth.is_none_or(|depth| depth > MAX_FRAME_DEPTH) {
                frames_skipped.push(format!("{frame_id}: frame depth unavailable or too deep"));
                continue;
            }
            let context_id = match timeout(
                Duration::from_millis(750),
                page.frame_execution_context(frame.clone()),
            )
            .await
            {
                Ok(Ok(Some(context_id))) => context_id,
                _ => {
                    frames_skipped.push(format!("{frame_id}: execution context unavailable"));
                    continue;
                }
            };
            let frame_url = timeout(Duration::from_millis(500), page.frame_url(frame.clone()))
                .await
                .ok()
                .and_then(std::result::Result::ok)
                .flatten();
            match timeout(
                PER_FRAME_DEADLINE.min(SNAPSHOT_DEADLINE.saturating_sub(started.elapsed())),
                evaluate_document_snapshot(page, &eval_js, Some(context_id)),
            )
            .await
            {
                Ok(Ok(frame_elements)) => {
                    frames_included += 1;
                    included_frame_ids.insert(frame_id.clone());
                    elements.extend(frame_elements.into_iter().map(|element| CapturedElement {
                        element,
                        frame_id: Some(frame_id.clone()),
                        frame_url: frame_url.clone(),
                        target_id: None,
                    }));
                }
                Ok(Err(error)) => frames_skipped.push(format!("{frame_id}: {error}")),
                Err(_) => frames_skipped.push(format!("{frame_id}: snapshot timed out")),
            }
        }
    }

    // Site-isolated cross-origin frames have no execution context on the
    // top-level chromiumoxide Page. Its current target poller ignores `iframe`
    // targets entirely, so collect those documents through a bounded secondary
    // CDP session and retain their owning target for subsequent actions.
    if started.elapsed() < SNAPSHOT_DEADLINE {
        if let Some(context) = store.oopif_context(store_key).await {
            let remaining = SNAPSHOT_DEADLINE.saturating_sub(started.elapsed());
            match timeout(
                remaining,
                super::oopif::capture(
                    &context.websocket_url,
                    &page_frame_ids,
                    &eval_js,
                    &context.policy,
                ),
            )
            .await
            {
                Ok(oopif) => {
                    frames_seen = frames_seen.max(oopif.frames_seen);
                    frames_skipped.extend(oopif.frames_skipped);
                    for frame in oopif.frames {
                        if included_frame_ids.contains(&frame.frame_id) {
                            continue;
                        }
                        frames_included += 1;
                        included_frame_ids.insert(frame.frame_id.clone());
                        elements.extend(frame.elements.into_iter().map(|element| {
                            CapturedElement {
                                element,
                                frame_id: Some(frame.frame_id.clone()),
                                frame_url: Some(frame.frame_url.clone()),
                                target_id: Some(frame.target_id.clone()),
                            }
                        }));
                    }
                }
                Err(_) => frames_skipped.push("OOPIF snapshot deadline reached".to_string()),
            }
        }
    }
    frames_skipped.retain(|message| {
        !included_frame_ids
            .iter()
            .any(|frame_id| message.starts_with(&format!("{frame_id}:")))
    });

    // Assign refs and build the output. Ref ids carry a generation so a ref
    // from an earlier snapshot cannot collide with one from this snapshot.
    let generation = store.allocate_generation().await;
    let mut ref_map = HashMap::new();
    let mut output_elements = Vec::new();
    let mut ref_counter = 0usize;

    for captured in &elements {
        let elem = &captured.element;
        ref_counter += 1;
        let ref_id = match options.mode {
            SnapshotMode::Ai => format!("s{generation}-{ref_counter}"),
            SnapshotMode::Aria => format!("s{generation}-e{ref_counter}"),
        };

        let element_ref = ElementRef {
            ref_id: ref_id.clone(),
            selector: elem.selector.clone(),
            frame_id: captured.frame_id.clone(),
            frame_url: captured.frame_url.clone(),
            target_id: captured.target_id.clone(),
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
                if let Some(frame_url) = captured.frame_url.as_deref() {
                    line.push_str(&format!(" [frame: {}]", truncate(frame_url, 60)));
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
                "frame_id": captured.frame_id,
                "frame_url": captured.frame_url,
                "target_id": captured.target_id,
            }));
        }
    }

    // Store refs for later use by act actions
    store.store(store_key, ref_map).await;

    let url = page.url().await.ok().flatten().unwrap_or_default();
    let title = page.get_title().await.ok().flatten().unwrap_or_default();
    let captcha = detect_captcha(page).await;

    Ok(serde_json::json!({
        "url": url,
        "title": title,
        "mode": match options.mode { SnapshotMode::Ai => "ai", SnapshotMode::Aria => "aria" },
        "elements": output_elements,
        "count": ref_counter,
        "interactive_count": elements.iter().filter(|e| e.element.interactive).count(),
        "frames_seen": frames_seen,
        "frames_included": frames_included,
        "frames_skipped": frames_skipped,
        "captcha": captcha,
        "snapshot_generation": generation,
        "highlight": options.highlight,
        "note": "Use the complete string in each element's ref field with the 'act' action. Refs are valid only for this snapshot."
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

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_ref(id: &str) -> ElementRef {
        ElementRef {
            ref_id: id.to_string(),
            selector: "body > a".to_string(),
            frame_id: None,
            frame_url: None,
            target_id: None,
            role: "link".to_string(),
            name: "x".to_string(),
            value: None,
            interactive: true,
            bounds: None,
        }
    }

    fn mk_refs(id: &str) -> HashMap<String, ElementRef> {
        let mut m = HashMap::new();
        m.insert(id.to_string(), mk_ref(id));
        m
    }

    #[test]
    fn default_max_depth_reaches_deep_spa_elements() {
        // Modern React/Angular/Vue SPAs nest interactive elements deeply.
        // Instagram's login inputs sit ~32 levels down; the default must walk
        // past that or snapshots report 0 interactive elements on such pages.
        const DEEPEST_OBSERVED_SPA_NESTING: usize = 32;
        assert!(
            SnapshotOptions::default().max_depth > DEEPEST_OBSERVED_SPA_NESTING,
            "default max_depth ({}) must exceed real-world SPA nesting ({})",
            SnapshotOptions::default().max_depth,
            DEEPEST_OBSERVED_SPA_NESTING,
        );
    }

    #[tokio::test]
    async fn snapshot_evicts_lru_when_over_capacity() {
        let store = SnapshotStore::new();
        for i in 0..(MAX_SNAPSHOT_KEYS + 5) {
            store.store(&format!("k-{i}"), mk_refs("1")).await;
        }
        let inner = store.inner.lock().await;
        assert_eq!(inner.refs.len(), MAX_SNAPSHOT_KEYS);
        assert!(!inner.refs.contains_key("k-0"));
        assert!(inner
            .refs
            .contains_key(&format!("k-{}", MAX_SNAPSHOT_KEYS + 4)));
    }

    #[tokio::test]
    async fn snapshot_get_ref_refreshes_recency() {
        let store = SnapshotStore::new();
        for i in 0..MAX_SNAPSHOT_KEYS {
            store.store(&format!("k-{i}"), mk_refs("1")).await;
        }
        // Bump k-0 to most-recent via a successful get_ref.
        assert!(store.get_ref("k-0", "1").await.is_some());
        store.store("overflow", mk_refs("1")).await;

        let inner = store.inner.lock().await;
        assert!(inner.refs.contains_key("k-0"));
        assert!(!inner.refs.contains_key("k-1"));
        assert!(inner.refs.contains_key("overflow"));
    }

    #[tokio::test]
    async fn find_by_identity_distinguishes_unique_none_and_ambiguous() {
        let store = SnapshotStore::new();
        let mut refs = HashMap::new();
        for (id, sel, role, name) in [
            ("1", "#a", "button", "Submit"),
            ("2", "#b", "button", "Submit"),
            ("3", "#c", "link", "Home"),
        ] {
            refs.insert(
                id.to_string(),
                ElementRef {
                    ref_id: id.into(),
                    selector: sel.into(),
                    frame_id: None,
                    frame_url: None,
                    target_id: None,
                    role: role.into(),
                    name: name.into(),
                    value: None,
                    interactive: true,
                    bounds: None,
                },
            );
        }
        store.store("k", refs).await;

        // Unique match heals; ambiguous and absent both escalate.
        assert_eq!(
            store
                .find_by_identity("k", "link", "Home", None)
                .await
                .len(),
            1
        );
        assert_eq!(
            store
                .find_by_identity("k", "button", "Submit", None)
                .await
                .len(),
            2
        );
        assert!(store
            .find_by_identity("k", "button", "Cancel", None)
            .await
            .is_empty());
        assert!(store
            .find_by_identity("missing", "link", "Home", None)
            .await
            .is_empty());
    }

    #[tokio::test]
    async fn snapshot_clear_removes_from_order() {
        let store = SnapshotStore::new();
        store.store("a", mk_refs("1")).await;
        store.clear("a").await;
        let inner = store.inner.lock().await;
        assert!(!inner.refs.contains_key("a"));
        assert!(!inner.order.iter().any(|k| k == "a"));
    }

    #[tokio::test]
    async fn snapshot_generations_are_monotonic() {
        let store = SnapshotStore::new();
        assert_eq!(store.allocate_generation().await, 1);
        assert_eq!(store.allocate_generation().await, 2);
    }

    #[tokio::test]
    async fn profile_recovery_invalidates_every_sessions_refs() {
        let store = SnapshotStore::new();
        store
            .store("conversation-a:shared:target-1", mk_refs("s1-1"))
            .await;
        store
            .store("conversation-b:shared:target-2", mk_refs("s2-1"))
            .await;
        store
            .store("conversation-c:other:target-3", mk_refs("s3-1"))
            .await;

        store.clear_profile("shared").await;

        assert!(store
            .get_ref("conversation-a:shared:target-1", "s1-1")
            .await
            .is_none());
        assert!(store
            .get_ref("conversation-b:shared:target-2", "s2-1")
            .await
            .is_none());
        assert!(store
            .get_ref("conversation-c:other:target-3", "s3-1")
            .await
            .is_some());
    }
}
