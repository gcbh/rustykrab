//! Internal wiki — a filesystem-backed, interlinked HTML knowledge base.
//!
//! The agent uses this to turn documents and messages it reads into durable
//! reference material it can come back to later. Pages are plain `.html` files
//! laid out under a wiki directory, cross-linked with relative `<a href>` links
//! and a regenerated `index.html`, so the whole thing is a browsable static
//! site on disk — no database, no server.
//!
//! Retrieval is layered on top of the hybrid memory system: every page write is
//! also indexed as a memory fact (tagged `wiki`, carrying a `[[wiki:<slug>]]`
//! back-reference), so `wiki(action="search", ...)` recalls pages semantically
//! and resolves the hits back to files on disk. A direct filesystem text scan
//! runs as a fallback so freshly written pages are findable immediately, before
//! the memory index has caught up.
//!
//! Discoverability does not lean on the directory tree (folders are too rigid —
//! a page lives in exactly one, and you outgrow the hierarchy). Instead it leans
//! on the things mature knowledge bases converge on:
//!   * a regenerated `index.html` that carries a one-line **summary** per page,
//!     so the index can be scanned, not just enumerated;
//!   * **backlinks** ("Referenced by") computed from the link graph, kept fresh
//!     in each page and returned live by the `read` action — plus orphan
//!     surfacing in `list` for pages nothing points at;
//!   * **related-page suggestions** on write, so the agent updates an existing
//!     page instead of forking a near-duplicate.
//!
//! Security: slugs are sanitized to stay strictly inside the wiki directory
//! (no `..`, no absolute paths, no exotic characters), and the page body the
//! agent supplies is written verbatim — this is a trusted, local, single-user
//! store, not a public web server.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::{Arc, LazyLock};

use async_trait::async_trait;
use regex::Regex;
use rustykrab_core::types::ToolSchema;
use rustykrab_core::{Error, Result, SandboxRequirements, Tool};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::memory_backend::MemoryBackend;

/// Filename of the regenerated table-of-contents page.
const INDEX_FILE: &str = "index.html";
/// Hidden sidecar that maps slugs to their title / memory id / metadata.
const SIDECAR_FILE: &str = ".wiki.json";
/// Cap on the plaintext we hand to the memory system per page.
const MEMORY_EXCERPT_LIMIT: usize = 4000;
/// Cap on a derived one-line summary.
const SUMMARY_LIMIT: usize = 200;
/// Markers delimiting the auto-maintained "Referenced by" region of a page, so
/// it can be refreshed in place when the link graph changes without re-rendering
/// the agent-authored body.
const BACKLINKS_START: &str = "<!--wiki:backlinks-->";
const BACKLINKS_END: &str = "<!--wiki:backlinks:end-->";

// Compiled once and reused — these run on every page read/write/search hit.
static SCRIPT_STYLE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?is)<(script|style)\b[^>]*>.*?</(script|style)>").expect("static regex")
});
static TAG_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)<[^>]+>").expect("static regex"));
static WHITESPACE_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s+").expect("static regex"));
static ANCHOR_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?is)<a\b[^>]*href\s*=\s*["']([^"']*)["'][^>]*>(.*?)</a>"#).expect("static regex")
});
static TITLE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?is)<title>(.*?)</title>").expect("static regex"));
static WIKI_MARKER_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\[\[wiki:([^\]]+)\]\]").expect("static regex"));
static BACKLINKS_REGION_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        "(?s){}.*?{}",
        regex::escape(BACKLINKS_START),
        regex::escape(BACKLINKS_END)
    ))
    .expect("static regex")
});

// ---------------------------------------------------------------------------
// Sidecar index
// ---------------------------------------------------------------------------

/// Per-page metadata persisted alongside the HTML. This is the structured map
/// of the wiki: it powers listing, search-hit resolution, the link graph
/// (backlinks/orphans), and cleanup of a page's memory entry on rewrite/delete.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct PageMeta {
    title: String,
    #[serde(default)]
    summary: String,
    #[serde(default)]
    memory_id: Option<String>,
    updated_at: String,
    #[serde(default)]
    tags: Vec<String>,
    /// Outgoing internal links (target slugs), the edges of the link graph.
    #[serde(default)]
    links: Vec<String>,
}

/// The sidecar document: slug -> metadata, kept sorted for stable index output.
#[derive(Debug, Default, Serialize, Deserialize)]
struct WikiIndex {
    #[serde(default)]
    pages: BTreeMap<String, PageMeta>,
}

// ---------------------------------------------------------------------------
// Tool struct
// ---------------------------------------------------------------------------

/// A single multi-action tool for the agent's internal wiki.
pub struct WikiTool {
    wiki_dir: PathBuf,
    memory: Arc<dyn MemoryBackend>,
    /// Serializes sidecar read-modify-write so concurrent tool calls don't
    /// clobber each other's index updates.
    index_lock: tokio::sync::Mutex<()>,
}

impl WikiTool {
    pub fn new(wiki_dir: PathBuf, memory: Arc<dyn MemoryBackend>) -> Self {
        Self {
            wiki_dir,
            memory,
            index_lock: tokio::sync::Mutex::new(()),
        }
    }

    fn sidecar_path(&self) -> PathBuf {
        self.wiki_dir.join(SIDECAR_FILE)
    }

    fn load_index(&self) -> WikiIndex {
        let path = self.sidecar_path();
        match std::fs::read_to_string(&path) {
            Ok(raw) => serde_json::from_str(&raw).unwrap_or_default(),
            Err(_) => WikiIndex::default(),
        }
    }

    fn save_index(&self, index: &WikiIndex) -> Result<()> {
        let raw = serde_json::to_string_pretty(index).map_err(|e| {
            Error::ToolExecution(format!("failed to encode wiki index: {e}").into())
        })?;
        std::fs::write(self.sidecar_path(), raw)
            .map_err(|e| Error::ToolExecution(format!("failed to write wiki index: {e}").into()))
    }

    fn slug_to_path(&self, slug: &str) -> PathBuf {
        self.wiki_dir.join(format!("{slug}.html"))
    }

    // -- actions -----------------------------------------------------------

    async fn write_page(&self, args: &Value) -> Result<Value> {
        let slug = sanitize_slug(require_str(args, "slug")?)?;
        let title = require_str(args, "title")?.to_string();
        let body = require_str(args, "html")?.to_string();
        let tags = parse_string_array(&args["tags"]);
        let see_also = parse_links(&args["links"]);

        let plaintext = html_to_text(&body);
        let summary = match args["summary"].as_str() {
            Some(s) if !s.trim().is_empty() => snippet(s.trim(), SUMMARY_LIMIT),
            _ => derive_summary(&plaintext),
        };
        // The link graph draws on both the structured "see also" links and any
        // internal anchors the agent put inline in the body.
        let outgoing = collect_outgoing_links(&slug, &body, &see_also);

        // Index into hybrid memory. Replace any prior memory entry for this slug
        // so rewrites don't pile up stale facts.
        let fact = build_memory_fact(&slug, &title, &tags, &summary, &plaintext);
        let mut mem_tags = vec!["wiki".to_string(), slug.clone()];
        mem_tags.extend(tags.iter().cloned());

        let guard = self.index_lock.lock().await;
        let mut index = self.load_index();
        let prev_outgoing = index
            .pages
            .get(&slug)
            .map(|p| p.links.clone())
            .unwrap_or_default();
        if let Some(prev) = index.pages.get(&slug) {
            if let Some(old_id) = &prev.memory_id {
                // Best-effort: a failed cleanup shouldn't block the write.
                let _ = self.memory.delete(old_id).await;
            }
        }
        let memory_id = match self.memory.save(&fact, &mem_tags).await {
            Ok(v) => v.get("id").and_then(|i| i.as_str()).map(String::from),
            Err(e) => {
                tracing::warn!(%slug, error = %e, "wiki page saved to disk but memory indexing failed");
                None
            }
        };
        index.pages.insert(
            slug.clone(),
            PageMeta {
                title: title.clone(),
                summary: summary.clone(),
                memory_id: memory_id.clone(),
                updated_at: now_rfc3339(),
                tags: tags.clone(),
                links: outgoing.clone(),
            },
        );

        // Render the page itself, including its current backlinks.
        let backlinks = backlinks_for(&index, &slug);
        let document = render_page(
            &title,
            &body,
            &see_also,
            &tags,
            index.pages[&slug].updated_at.as_str(),
            &rel_root_for(&slug),
            &backlinks,
        );
        let page_path = self.slug_to_path(&slug);
        if let Some(parent) = page_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                Error::ToolExecution(format!("failed to create wiki directory: {e}").into())
            })?;
        }
        std::fs::write(&page_path, &document).map_err(|e| {
            Error::ToolExecution(format!("failed to write wiki page {slug}: {e}").into())
        })?;

        self.save_index(&index)?;
        self.regenerate_index(&index)?;

        // Refresh the "Referenced by" region of every page whose inbound set
        // this write could have changed: links added, links removed, and the
        // pages still linked (in case this page's title changed). Bounded by the
        // number of links on the page.
        let mut affected: BTreeSet<&String> = BTreeSet::new();
        affected.extend(prev_outgoing.iter());
        affected.extend(outgoing.iter());
        for target in affected {
            if *target != slug {
                self.refresh_page_backlinks(target, &index);
            }
        }
        drop(guard);

        // Related-page suggestions: nudge the agent to reuse an existing page
        // rather than fork a near-duplicate. Excludes the page just written.
        let related = self
            .find_related(&format!("{title}. {summary}"), Some(&slug), 5)
            .await;

        Ok(json!({
            "status": "saved",
            "slug": slug,
            "title": title,
            "summary": summary,
            "path": page_path.to_string_lossy(),
            "url": format!("{slug}.html"),
            "indexed": memory_id.is_some(),
            "memory_id": memory_id,
            "outgoing_links": outgoing,
            "backlink_count": backlinks.len(),
            "related": related,
        }))
    }

    async fn read_page(&self, args: &Value) -> Result<Value> {
        let slug = sanitize_slug(require_str(args, "slug")?)?;
        let page_path = self.slug_to_path(&slug);
        let document = std::fs::read_to_string(&page_path)
            .map_err(|_| Error::NotFound(format!("wiki page '{slug}'")))?;

        let index = self.load_index();
        let meta = index.pages.get(&slug);
        let title = meta
            .map(|m| m.title.clone())
            .or_else(|| extract_title(&document))
            .unwrap_or_else(|| slug.clone());
        // Backlinks computed live from the graph so they're never stale.
        let backlinks: Vec<Value> = backlinks_for(&index, &slug)
            .into_iter()
            .map(|(s, t)| json!({ "slug": s, "title": t }))
            .collect();

        Ok(json!({
            "slug": slug,
            "title": title,
            "summary": meta.map(|m| m.summary.clone()).unwrap_or_default(),
            "html": document,
            "text": html_to_text(&document),
            "links": extract_links(&document)
                .into_iter()
                .map(|(href, text)| json!({ "href": href, "text": text }))
                .collect::<Vec<_>>(),
            "backlinks": backlinks,
            "tags": meta.map(|m| m.tags.clone()).unwrap_or_default(),
            "updated_at": meta.map(|m| m.updated_at.clone()),
        }))
    }

    async fn search_pages(&self, args: &Value) -> Result<Value> {
        let query = require_str(args, "query")?.to_string();
        let limit = args["limit"].as_u64().unwrap_or(5).clamp(1, 50) as usize;
        let results = self.find_related(&query, None, limit).await;
        Ok(json!({
            "query": query,
            "count": results.len(),
            "results": results,
        }))
    }

    async fn list_pages(&self, _args: &Value) -> Result<Value> {
        let index = self.load_index();
        let mut orphans: Vec<String> = Vec::new();
        let pages: Vec<Value> = index
            .pages
            .iter()
            .map(|(slug, meta)| {
                let inbound = backlinks_for(&index, slug).len();
                if inbound == 0 {
                    orphans.push(slug.clone());
                }
                json!({
                    "slug": slug,
                    "title": meta.title,
                    "summary": meta.summary,
                    "url": format!("{slug}.html"),
                    "tags": meta.tags,
                    "inbound": inbound,
                    "outbound": meta.links.len(),
                    "orphan": inbound == 0,
                    "updated_at": meta.updated_at,
                })
            })
            .collect();
        Ok(json!({
            "count": pages.len(),
            "pages": pages,
            "orphans": orphans,
            "index_url": INDEX_FILE,
        }))
    }

    async fn delete_page(&self, args: &Value) -> Result<Value> {
        let slug = sanitize_slug(require_str(args, "slug")?)?;
        let page_path = self.slug_to_path(&slug);

        let guard = self.index_lock.lock().await;
        let mut index = self.load_index();
        let existed = index.pages.remove(&slug);
        if let Some(meta) = &existed {
            if let Some(id) = &meta.memory_id {
                let _ = self.memory.delete(id).await;
            }
        }
        let removed_file = std::fs::remove_file(&page_path).is_ok();
        self.save_index(&index)?;
        self.regenerate_index(&index)?;
        // The pages this one used to point at lost an inbound link — refresh them.
        if let Some(meta) = &existed {
            for target in &meta.links {
                if *target != slug {
                    self.refresh_page_backlinks(target, &index);
                }
            }
        }
        drop(guard);

        if existed.is_none() && !removed_file {
            return Err(Error::NotFound(format!("wiki page '{slug}'")));
        }
        Ok(json!({
            "status": "deleted",
            "slug": slug,
        }))
    }

    // -- helpers -----------------------------------------------------------

    /// Find wiki pages relevant to `query`: hybrid memory recall first (resolved
    /// back to pages via the `[[wiki:<slug>]]` marker), then a direct text scan
    /// as a fallback so pages are findable before async memory extraction lands.
    /// `exclude` drops a slug (used so a write doesn't recommend itself).
    async fn find_related(&self, query: &str, exclude: Option<&str>, limit: usize) -> Vec<Value> {
        let index = self.load_index();
        let mut hits: Vec<Value> = Vec::new();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        if let Some(ex) = exclude {
            seen.insert(ex.to_string());
        }

        // Primary: hybrid memory recall. Over-fetch because the backend's hybrid
        // search ignores tag filters, so non-wiki hits are mixed in and dropped.
        if let Ok(result) = self
            .memory
            .search(query, &["wiki".to_string()], limit * 4)
            .await
        {
            if let Some(items) = result.get("results").and_then(|r| r.as_array()) {
                for item in items {
                    let content = item.get("content").and_then(|c| c.as_str()).unwrap_or("");
                    let Some(slug) = extract_wiki_marker(content) else {
                        continue;
                    };
                    if !seen.insert(slug.clone()) || !self.slug_to_path(&slug).exists() {
                        continue;
                    }
                    hits.push(json!({
                        "slug": slug,
                        "title": index.pages.get(&slug).map(|m| m.title.clone()).unwrap_or_else(|| slug.clone()),
                        "url": format!("{slug}.html"),
                        "snippet": index.pages.get(&slug).map(|m| m.summary.clone()).filter(|s| !s.is_empty()).unwrap_or_else(|| snippet(content, 240)),
                        "score": item.get("score").cloned().unwrap_or(Value::Null),
                        "source": "memory",
                    }));
                    if hits.len() >= limit {
                        break;
                    }
                }
            }
        }

        // Fallback / supplement: a direct text scan over page contents.
        if hits.len() < limit {
            let needles: Vec<String> = query.split_whitespace().map(|w| w.to_lowercase()).collect();
            for (slug, meta) in &index.pages {
                if seen.contains(slug) {
                    continue;
                }
                let Ok(document) = std::fs::read_to_string(self.slug_to_path(slug)) else {
                    continue;
                };
                let text = html_to_text(&document);
                let haystack = text.to_lowercase();
                let matches = needles.iter().filter(|n| haystack.contains(*n)).count();
                if matches == 0 {
                    continue;
                }
                seen.insert(slug.clone());
                hits.push(json!({
                    "slug": slug,
                    "title": meta.title,
                    "url": format!("{slug}.html"),
                    "snippet": if meta.summary.is_empty() { snippet(&text, 240) } else { meta.summary.clone() },
                    "score": matches,
                    "source": "filesystem",
                }));
                if hits.len() >= limit {
                    break;
                }
            }
        }
        hits
    }

    /// Rewrite just the "Referenced by" region of an existing page in place.
    /// Best-effort: a missing file or missing markers is silently skipped.
    fn refresh_page_backlinks(&self, target: &str, index: &WikiIndex) {
        let path = self.slug_to_path(target);
        let Ok(document) = std::fs::read_to_string(&path) else {
            return;
        };
        let backlinks = backlinks_for(index, target);
        let region = render_backlinks_region(&backlinks, &rel_root_for(target));
        let updated = BACKLINKS_REGION_RE.replace(&document, region.as_str());
        if updated != document {
            let _ = std::fs::write(&path, updated.as_ref());
        }
    }

    /// Rewrite `index.html` as a scannable table of contents: title, one-line
    /// summary, tags, and inbound-reference count per page.
    fn regenerate_index(&self, index: &WikiIndex) -> Result<()> {
        let mut items = String::new();
        for (slug, meta) in &index.pages {
            let inbound = backlinks_for(index, slug).len();
            let mut meta_bits = Vec::new();
            if inbound == 0 {
                meta_bits.push("orphan".to_string());
            } else {
                meta_bits.push(format!("{inbound} ref(s)"));
            }
            if !meta.tags.is_empty() {
                meta_bits.push(esc(&meta.tags.join(", ")));
            }
            items.push_str(&format!(
                "    <li>\n      <a href=\"{slug}.html\">{title}</a> \
                 <span class=\"meta\">{meta_bits}</span>\n{summary}    </li>\n",
                slug = esc(slug),
                title = esc(&meta.title),
                meta_bits = meta_bits.join(" · "),
                summary = if meta.summary.is_empty() {
                    String::new()
                } else {
                    format!("      <p class=\"summary\">{}</p>\n", esc(&meta.summary))
                },
            ));
        }
        if items.is_empty() {
            items.push_str("    <li><em>No pages yet.</em></li>\n");
        }
        let body = format!(
            "<h1>Wiki</h1>\n<p>{count} page(s).</p>\n<ul class=\"wiki-index\">\n{items}</ul>",
            count = index.pages.len(),
        );
        let document = render_document("Wiki", &body, "");
        std::fs::write(self.wiki_dir.join(INDEX_FILE), document).map_err(|e| {
            Error::ToolExecution(format!("failed to write wiki index page: {e}").into())
        })
    }
}

#[async_trait]
impl Tool for WikiTool {
    fn name(&self) -> &str {
        "wiki"
    }

    fn description(&self) -> &str {
        "Internal wiki — a persistent, interlinked HTML knowledge base on disk. \
         Use it to capture context from documents and messages you read so you \
         can retrieve it later. Actions: 'write' creates/updates a page (HTML \
         body, optional one-line summary, cross-links and tags) and returns \
         related existing pages so you can update instead of duplicating; 'read' \
         returns a page's content, outgoing links, and backlinks; 'search' finds \
         relevant pages by meaning or text; 'list' enumerates pages with \
         inbound/outbound link counts and flags orphans; 'delete' removes a \
         page. Pages are addressed by a 'slug' path like 'projects/rustykrab'. \
         Link pages together with <a href=\"other-slug.html\"> in the body or \
         via the 'links' field — backlinks are maintained automatically."
    }

    fn sandbox_requirements(&self) -> SandboxRequirements {
        SandboxRequirements {
            needs_fs_read: true,
            needs_fs_write: true,
            ..SandboxRequirements::default()
        }
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
                        "enum": ["write", "read", "search", "list", "delete"],
                        "description": "The wiki operation to perform"
                    },
                    "slug": {
                        "type": "string",
                        "description": "Page address, e.g. 'projects/rustykrab' (no extension). Required for write/read/delete."
                    },
                    "title": {
                        "type": "string",
                        "description": "Human-readable page title (required for write)"
                    },
                    "html": {
                        "type": "string",
                        "description": "The page body as an HTML fragment (required for write). Written verbatim inside a page template. Use <a href=\"other-slug.html\"> to link to other wiki pages."
                    },
                    "summary": {
                        "type": "string",
                        "description": "Optional one-line summary shown in the index and search results. Defaults to the first sentence of the body (write only)."
                    },
                    "links": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "slug": { "type": "string", "description": "Target page slug" },
                                "label": { "type": "string", "description": "Link text (defaults to the slug)" }
                            },
                            "required": ["slug"]
                        },
                        "description": "Optional related pages, rendered as a 'See also' section and counted in the link graph (write only)"
                    },
                    "tags": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Optional topic tags for the page (write only)"
                    },
                    "query": {
                        "type": "string",
                        "description": "Search query (required for search)"
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Maximum search results to return (default 5)"
                    }
                },
                "required": ["action"]
            }),
        }
    }

    async fn execute(&self, args: Value) -> Result<Value> {
        let action = require_str(&args, "action")?;
        match action {
            "write" => self.write_page(&args).await,
            "read" => self.read_page(&args).await,
            "search" => self.search_pages(&args).await,
            "list" => self.list_pages(&args).await,
            "delete" => self.delete_page(&args).await,
            other => Err(Error::ToolExecution(
                format!("unknown wiki action '{other}' (expected write/read/search/list/delete)")
                    .into(),
            )),
        }
    }
}

// ---------------------------------------------------------------------------
// Free helpers (kept standalone for unit testing)
// ---------------------------------------------------------------------------

fn require_str<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    args[key]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| Error::ToolExecution(format!("missing required '{key}'").into()))
}

fn parse_string_array(value: &Value) -> Vec<String> {
    value
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

/// A "see also" link: (target slug, display label).
fn parse_links(value: &Value) -> Vec<(String, String)> {
    let Some(arr) = value.as_array() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for item in arr {
        let raw_slug = if let Some(s) = item.as_str() {
            s.to_string()
        } else if let Some(s) = item.get("slug").and_then(|v| v.as_str()) {
            s.to_string()
        } else {
            continue;
        };
        // Skip links we can't address safely rather than failing the whole write.
        let Ok(slug) = sanitize_slug(&raw_slug) else {
            continue;
        };
        let label = item
            .get("label")
            .and_then(|v| v.as_str())
            .map(String::from)
            .unwrap_or_else(|| slug.clone());
        out.push((slug, label));
    }
    out
}

/// Normalize a user-supplied slug into a safe relative path with no extension.
///
/// Confines the page strictly inside the wiki directory: rejects absolute
/// paths, `.`/`..` components, empty input, and anything outside a conservative
/// `[A-Za-z0-9._-]` + `/` character set.
fn sanitize_slug(raw: &str) -> Result<String> {
    let trimmed = raw.trim().trim_matches('/');
    let trimmed = trimmed.strip_suffix(".html").unwrap_or(trimmed);
    if trimmed.is_empty() {
        return Err(Error::ToolExecution("slug must not be empty".into()));
    }
    let mut parts = Vec::new();
    for part in trimmed.split('/') {
        if part.is_empty() || part == "." || part == ".." {
            return Err(Error::ToolExecution(
                format!("invalid slug '{raw}': path traversal and empty segments are not allowed")
                    .into(),
            ));
        }
        if !part
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        {
            return Err(Error::ToolExecution(
                format!(
                    "invalid slug '{raw}': only letters, digits, '-', '_', '.' and '/' allowed"
                )
                .into(),
            ));
        }
        parts.push(part);
    }
    Ok(parts.join("/"))
}

/// Collect the outgoing internal links of a page: the structured "see also"
/// slugs plus any internal anchors (`href="...html"`) found inline in the body,
/// resolved relative to the page's own location. The page never links to itself.
fn collect_outgoing_links(
    current_slug: &str,
    body: &str,
    see_also: &[(String, String)],
) -> Vec<String> {
    let mut set: BTreeSet<String> = BTreeSet::new();
    for (slug, _) in see_also {
        set.insert(slug.clone());
    }
    for (href, _) in extract_links(body) {
        if let Some(slug) = resolve_href_to_slug(current_slug, &href) {
            set.insert(slug);
        }
    }
    set.remove(current_slug);
    set.into_iter().collect()
}

/// Resolve a relative `<a href>` to an internal wiki slug, or `None` if it isn't
/// an internal page link (external URL, anchor, non-`.html`, or escapes root).
fn resolve_href_to_slug(current_slug: &str, href: &str) -> Option<String> {
    let href = href.split(['#', '?']).next().unwrap_or(href).trim();
    if href.is_empty() || href.contains("://") || href.starts_with("mailto:") {
        return None;
    }
    let target = href.strip_suffix(".html")?;
    // Absolute (root-relative) hrefs resolve from the wiki root; otherwise from
    // the current page's directory.
    let mut parts: Vec<String> = if target.starts_with('/') {
        Vec::new()
    } else if let Some((dir, _)) = current_slug.rsplit_once('/') {
        dir.split('/').map(String::from).collect()
    } else {
        Vec::new()
    };
    for seg in target.trim_start_matches('/').split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            s => parts.push(s.to_string()),
        }
    }
    if parts.is_empty() {
        return None;
    }
    sanitize_slug(&parts.join("/")).ok()
}

/// Pages that link to `target`, as (slug, title), excluding self-links.
fn backlinks_for(index: &WikiIndex, target: &str) -> Vec<(String, String)> {
    index
        .pages
        .iter()
        .filter(|(slug, meta)| slug.as_str() != target && meta.links.iter().any(|l| l == target))
        .map(|(slug, meta)| (slug.clone(), meta.title.clone()))
        .collect()
}

/// Relative prefix to reach the wiki root from a page at the given slug depth.
fn rel_root_for(slug: &str) -> String {
    "../".repeat(slug.matches('/').count())
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339()
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Derive a one-line summary from page plaintext: the first sentence if it's
/// reasonably short, otherwise a truncated prefix.
fn derive_summary(text: &str) -> String {
    let t = text.trim();
    if t.is_empty() {
        return String::new();
    }
    if let Some(idx) = t.find(". ") {
        if idx < SUMMARY_LIMIT {
            return t[..=idx].trim().to_string();
        }
    }
    snippet(t, SUMMARY_LIMIT)
}

fn build_memory_fact(
    slug: &str,
    title: &str,
    tags: &[String],
    summary: &str,
    plaintext: &str,
) -> String {
    let excerpt = snippet(plaintext, MEMORY_EXCERPT_LIMIT);
    format!(
        "Wiki page: {title}\n[[wiki:{slug}]]\nSummary: {summary}\nTags: {tags}\n\n{excerpt}",
        tags = tags.join(", "),
    )
}

/// The shared HTML scaffold for every wiki page.
fn render_document(title: &str, body: &str, rel_root: &str) -> String {
    format!(
        "<!doctype html>\n\
<html lang=\"en\">\n\
<head>\n\
<meta charset=\"utf-8\">\n\
<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
<title>{title}</title>\n\
<style>\n\
body{{font-family:system-ui,sans-serif;max-width:48rem;margin:2rem auto;padding:0 1rem;line-height:1.6;color:#1a1a1a}}\n\
nav{{margin-bottom:1.5rem;font-size:.9rem}}\n\
.tags,.meta{{color:#666;font-size:.85rem}}\n\
.summary{{margin:.2rem 0 .6rem;color:#444}}\n\
.backlinks{{margin-top:1.5rem}}\n\
footer{{margin-top:2rem;padding-top:1rem;border-top:1px solid #ddd;color:#666;font-size:.85rem}}\n\
a{{color:#0366d6}}\n\
</style>\n\
</head>\n\
<body>\n\
<nav><a href=\"{rel_root}{index}\">\u{2190} Wiki index</a></nav>\n\
{body}\n\
</body>\n\
</html>\n",
        title = esc(title),
        index = INDEX_FILE,
    )
}

/// Render the auto-maintained "Referenced by" region, always wrapped in markers
/// so it can later be refreshed in place. Empty when nothing links here.
fn render_backlinks_region(backlinks: &[(String, String)], rel_root: &str) -> String {
    let mut s = String::from(BACKLINKS_START);
    if !backlinks.is_empty() {
        s.push_str("\n<section class=\"backlinks\">\n<h2>Referenced by</h2>\n<ul>\n");
        for (slug, title) in backlinks {
            s.push_str(&format!(
                "<li><a href=\"{rel_root}{slug}.html\">{title}</a></li>\n",
                slug = esc(slug),
                title = esc(title),
            ));
        }
        s.push_str("</ul>\n</section>\n");
    }
    s.push_str(BACKLINKS_END);
    s
}

/// Render a full content page: title, agent-supplied body, a footer with "see
/// also" links / tags / timestamp, and the auto-maintained backlinks region.
#[allow(clippy::too_many_arguments)]
fn render_page(
    title: &str,
    body: &str,
    see_also: &[(String, String)],
    tags: &[String],
    updated_at: &str,
    rel_root: &str,
    backlinks: &[(String, String)],
) -> String {
    let mut footer = String::from("<footer>\n");
    if !see_also.is_empty() {
        footer.push_str("<p><strong>See also:</strong></p>\n<ul>\n");
        for (slug, label) in see_also {
            footer.push_str(&format!(
                "<li><a href=\"{rel_root}{slug}.html\">{label}</a></li>\n",
                slug = esc(slug),
                label = esc(label),
            ));
        }
        footer.push_str("</ul>\n");
    }
    if !tags.is_empty() {
        footer.push_str(&format!(
            "<p class=\"tags\">Tags: {}</p>\n",
            esc(&tags.join(", "))
        ));
    }
    footer.push_str(&format!("<p>Updated {}</p>\n</footer>", esc(updated_at)));

    let region = render_backlinks_region(backlinks, rel_root);
    let content = format!("<h1>{}</h1>\n{}\n{}\n{}", esc(title), body, footer, region);
    render_document(title, &content, rel_root)
}

/// Strip HTML to readable plaintext: drop script/style, remove tags, decode a
/// handful of common entities, and collapse whitespace.
fn html_to_text(html: &str) -> String {
    let without_blocks = SCRIPT_STYLE_RE.replace_all(html, " ").into_owned();
    let without_tags = TAG_RE.replace_all(&without_blocks, " ").into_owned();
    let decoded = without_tags
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&nbsp;", " ")
        .replace("&amp;", "&");
    WHITESPACE_RE.replace_all(&decoded, " ").trim().to_string()
}

/// Extract `(href, text)` pairs from anchor tags.
fn extract_links(html: &str) -> Vec<(String, String)> {
    ANCHOR_RE
        .captures_iter(html)
        .map(|c| {
            let href = c.get(1).map(|m| m.as_str().to_string()).unwrap_or_default();
            let text = html_to_text(c.get(2).map(|m| m.as_str()).unwrap_or(""));
            (href, text)
        })
        .collect()
}

fn extract_title(html: &str) -> Option<String> {
    TITLE_RE
        .captures(html)
        .and_then(|c| c.get(1))
        .map(|m| html_to_text(m.as_str()))
}

/// Pull the `[[wiki:<slug>]]` back-reference out of an indexed memory fact.
fn extract_wiki_marker(content: &str) -> Option<String> {
    WIKI_MARKER_RE
        .captures(content)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().to_string())
}

/// Truncate `text` to at most `max` characters on a char boundary.
fn snippet(text: &str, max: usize) -> String {
    let trimmed = text.trim();
    if trimmed.chars().count() <= max {
        return trimmed.to_string();
    }
    let mut out: String = trimmed.chars().take(max).collect();
    out.push('\u{2026}');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Records memory calls and serves canned search results.
    #[derive(Default)]
    struct MockMemory {
        saved: Mutex<Vec<(String, Vec<String>)>>,
        deleted: Mutex<Vec<String>>,
        search_results: Mutex<Vec<Value>>,
    }

    #[async_trait]
    impl MemoryBackend for MockMemory {
        async fn search(&self, _query: &str, _tags: &[String], _limit: usize) -> Result<Value> {
            let results = self.search_results.lock().unwrap().clone();
            Ok(json!({ "results": results, "count": results.len() }))
        }
        async fn get(&self, _memory_id: &str) -> Result<Value> {
            Ok(json!({}))
        }
        async fn save(&self, fact: &str, tags: &[String]) -> Result<Value> {
            self.saved
                .lock()
                .unwrap()
                .push((fact.to_string(), tags.to_vec()));
            Ok(json!({ "id": "mem-123", "status": "saved" }))
        }
        async fn delete(&self, memory_id: &str) -> Result<Value> {
            self.deleted.lock().unwrap().push(memory_id.to_string());
            Ok(json!({ "status": "deleted" }))
        }
        async fn list(&self) -> Result<Value> {
            Ok(json!({ "memories": [], "count": 0 }))
        }
    }

    fn tool() -> (WikiTool, tempfile::TempDir, Arc<MockMemory>) {
        let dir = tempfile::tempdir().unwrap();
        let mem = Arc::new(MockMemory::default());
        let tool = WikiTool::new(dir.path().to_path_buf(), mem.clone());
        (tool, dir, mem)
    }

    #[test]
    fn sanitize_slug_accepts_nested() {
        assert_eq!(
            sanitize_slug("projects/rusty-krab").unwrap(),
            "projects/rusty-krab"
        );
        assert_eq!(sanitize_slug("Notes/2026.06").unwrap(), "Notes/2026.06");
        assert_eq!(sanitize_slug("foo.html").unwrap(), "foo");
    }

    #[test]
    fn sanitize_slug_rejects_traversal() {
        assert!(sanitize_slug("../etc/passwd").is_err());
        assert!(sanitize_slug("a/../b").is_err());
        assert!(sanitize_slug("").is_err());
        assert!(sanitize_slug("a/b c").is_err());
        assert!(sanitize_slug("a/b$c").is_err());
    }

    #[test]
    fn rel_root_depth() {
        assert_eq!(rel_root_for("top"), "");
        assert_eq!(rel_root_for("a/b"), "../");
        assert_eq!(rel_root_for("a/b/c"), "../../");
    }

    #[test]
    fn html_to_text_strips_markup() {
        let html = "<p>Hello <b>world</b></p><script>evil()</script>&amp; more";
        assert_eq!(html_to_text(html), "Hello world & more");
    }

    #[test]
    fn extract_links_finds_anchors() {
        let html = r#"<a href="a.html">First</a> and <a href='b.html'>Second</a>"#;
        let links = extract_links(html);
        assert_eq!(links.len(), 2);
        assert_eq!(links[0], ("a.html".to_string(), "First".to_string()));
        assert_eq!(links[1], ("b.html".to_string(), "Second".to_string()));
    }

    #[test]
    fn resolve_href_handles_relative_paths() {
        // sibling in same dir
        assert_eq!(
            resolve_href_to_slug("a/b/c", "x.html"),
            Some("a/b/x".to_string())
        );
        // round-trips a generated rel_root link from depth 2 back to root slug
        assert_eq!(
            resolve_href_to_slug("a/b/c", "../../t.html"),
            Some("t".to_string())
        );
        // external / non-page links are ignored
        assert_eq!(resolve_href_to_slug("a", "https://example.com"), None);
        assert_eq!(resolve_href_to_slug("a", "notes.txt"), None);
        assert_eq!(resolve_href_to_slug("a", "#section"), None);
    }

    #[test]
    fn derive_summary_takes_first_sentence() {
        assert_eq!(
            derive_summary("A short intro. More detail follows here."),
            "A short intro."
        );
        assert!(derive_summary("").is_empty());
    }

    #[test]
    fn extract_wiki_marker_roundtrips() {
        let fact = build_memory_fact("projects/foo", "Foo", &["bar".into()], "sum", "body text");
        assert_eq!(extract_wiki_marker(&fact), Some("projects/foo".to_string()));
    }

    #[tokio::test]
    async fn write_then_read_roundtrip() {
        let (tool, dir, mem) = tool();
        let res = tool
            .execute(json!({
                "action": "write",
                "slug": "projects/foo",
                "title": "Foo Project",
                "html": "<p>An <b>important</b> note.</p>",
                "tags": ["project", "foo"],
                "links": [{ "slug": "projects/bar", "label": "Bar" }]
            }))
            .await
            .unwrap();
        assert_eq!(res["status"], "saved");
        assert_eq!(res["indexed"], true);

        // File exists on disk with the rendered template.
        let page = dir.path().join("projects/foo.html");
        let contents = std::fs::read_to_string(&page).unwrap();
        assert!(contents.contains("<title>Foo Project</title>"));
        assert!(contents.contains("An <b>important</b> note."));
        assert!(contents.contains("../index.html")); // nav uses correct rel root
        assert!(contents.contains("projects/bar.html")); // see-also link

        // Index page was regenerated and carries the summary.
        let index_html = std::fs::read_to_string(dir.path().join("index.html")).unwrap();
        assert!(index_html.contains("An important note."));

        // Memory indexing happened with wiki tags.
        {
            let saved = mem.saved.lock().unwrap();
            assert_eq!(saved.len(), 1);
            assert!(saved[0].1.contains(&"wiki".to_string()));
            assert!(saved[0].1.contains(&"projects/foo".to_string()));
        }

        let read = tool
            .execute(json!({ "action": "read", "slug": "projects/foo" }))
            .await
            .unwrap();
        assert_eq!(read["title"], "Foo Project");
        assert_eq!(read["summary"], "An important note.");
        assert!(read["text"].as_str().unwrap().contains("important note"));
    }

    #[tokio::test]
    async fn rewrite_replaces_memory_entry() {
        let (tool, _dir, mem) = tool();
        let args = json!({
            "action": "write", "slug": "n", "title": "N", "html": "<p>v1</p>"
        });
        tool.execute(args.clone()).await.unwrap();
        tool.execute(json!({
            "action": "write", "slug": "n", "title": "N", "html": "<p>v2</p>"
        }))
        .await
        .unwrap();
        // Second write deleted the first page's memory entry.
        assert_eq!(
            mem.deleted.lock().unwrap().as_slice(),
            &["mem-123".to_string()]
        );
    }

    #[tokio::test]
    async fn backlinks_are_tracked_and_refreshed() {
        let (tool, dir, _mem) = tool();
        // B has no inbound links yet.
        tool.execute(
            json!({ "action": "write", "slug": "b", "title": "Page B", "html": "<p>b</p>" }),
        )
        .await
        .unwrap();
        // A links to B via the structured links field.
        tool.execute(json!({
            "action": "write", "slug": "a", "title": "Page A", "html": "<p>see b</p>",
            "links": [{ "slug": "b" }]
        }))
        .await
        .unwrap();

        // read B reports the backlink live...
        let read_b = tool
            .execute(json!({ "action": "read", "slug": "b" }))
            .await
            .unwrap();
        assert_eq!(read_b["backlinks"][0]["slug"], "a");
        // ...and B's HTML was refreshed in place to show "Referenced by".
        let b_html = std::fs::read_to_string(dir.path().join("b.html")).unwrap();
        assert!(b_html.contains("Referenced by"));
        assert!(b_html.contains(">Page A</a>"));

        // A second referrer via an inline anchor also lands in B's backlinks.
        tool.execute(json!({
            "action": "write", "slug": "c", "title": "Page C",
            "html": "<p>also see <a href=\"b.html\">B</a></p>"
        }))
        .await
        .unwrap();
        let read_b = tool
            .execute(json!({ "action": "read", "slug": "b" }))
            .await
            .unwrap();
        let slugs: Vec<&str> = read_b["backlinks"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v["slug"].as_str().unwrap())
            .collect();
        assert!(slugs.contains(&"a"));
        assert!(slugs.contains(&"c"));
    }

    #[tokio::test]
    async fn list_flags_orphans() {
        let (tool, _dir, _mem) = tool();
        tool.execute(json!({ "action": "write", "slug": "hub", "title": "Hub", "html": "<p>x</p>", "links": [{ "slug": "leaf" }] }))
            .await
            .unwrap();
        tool.execute(
            json!({ "action": "write", "slug": "leaf", "title": "Leaf", "html": "<p>y</p>" }),
        )
        .await
        .unwrap();
        let listed = tool.execute(json!({ "action": "list" })).await.unwrap();
        // hub has no inbound links (orphan); leaf is referenced by hub.
        let orphans: Vec<&str> = listed["orphans"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(orphans, vec!["hub"]);
    }

    #[tokio::test]
    async fn write_returns_related_pages() {
        let (tool, _dir, mem) = tool();
        tool.execute(json!({ "action": "write", "slug": "existing", "title": "Existing", "html": "<p>kelp</p>" }))
            .await
            .unwrap();
        // Memory recall surfaces the existing page as related to the new write.
        *mem.search_results.lock().unwrap() = vec![json!({
            "content": "Wiki page: Existing\n[[wiki:existing]]\n\nkelp",
            "score": 0.8
        })];
        let res = tool
            .execute(json!({ "action": "write", "slug": "fresh", "title": "Fresh", "html": "<p>kelp too</p>" }))
            .await
            .unwrap();
        let related = res["related"].as_array().unwrap();
        assert_eq!(related.len(), 1);
        assert_eq!(related[0]["slug"], "existing");
        // It never recommends the page being written.
        assert_ne!(related[0]["slug"], "fresh");
    }

    #[tokio::test]
    async fn search_resolves_memory_hit_to_page() {
        let (tool, _dir, mem) = tool();
        tool.execute(json!({
            "action": "write", "slug": "topic", "title": "Topic", "html": "<p>content</p>"
        }))
        .await
        .unwrap();
        *mem.search_results.lock().unwrap() = vec![json!({
            "content": "Wiki page: Topic\n[[wiki:topic]]\n\ncontent",
            "score": 0.9
        })];
        let res = tool
            .execute(json!({ "action": "search", "query": "topic" }))
            .await
            .unwrap();
        assert_eq!(res["count"], 1);
        assert_eq!(res["results"][0]["slug"], "topic");
        assert_eq!(res["results"][0]["source"], "memory");
    }

    #[tokio::test]
    async fn search_filesystem_fallback() {
        let (tool, _dir, _mem) = tool();
        tool.execute(json!({
            "action": "write", "slug": "kelp", "title": "Kelp", "html": "<p>green seaweed forests</p>"
        }))
        .await
        .unwrap();
        // No memory results configured -> falls back to filesystem scan.
        let res = tool
            .execute(json!({ "action": "search", "query": "seaweed" }))
            .await
            .unwrap();
        assert_eq!(res["count"], 1);
        assert_eq!(res["results"][0]["source"], "filesystem");
    }

    #[tokio::test]
    async fn list_and_delete() {
        let (tool, dir, _mem) = tool();
        tool.execute(json!({ "action": "write", "slug": "a", "title": "A", "html": "<p>a</p>" }))
            .await
            .unwrap();
        tool.execute(json!({ "action": "write", "slug": "b", "title": "B", "html": "<p>b</p>" }))
            .await
            .unwrap();
        let listed = tool.execute(json!({ "action": "list" })).await.unwrap();
        assert_eq!(listed["count"], 2);

        tool.execute(json!({ "action": "delete", "slug": "a" }))
            .await
            .unwrap();
        assert!(!dir.path().join("a.html").exists());
        let listed = tool.execute(json!({ "action": "list" })).await.unwrap();
        assert_eq!(listed["count"], 1);
    }

    #[tokio::test]
    async fn read_missing_page_errors() {
        let (tool, _dir, _mem) = tool();
        let err = tool
            .execute(json!({ "action": "read", "slug": "nope" }))
            .await;
        assert!(err.is_err());
    }
}
