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
//! Security: slugs are sanitized to stay strictly inside the wiki directory
//! (no `..`, no absolute paths, no exotic characters), and the page body the
//! agent supplies is written verbatim — this is a trusted, local, single-user
//! store, not a public web server.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

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

// ---------------------------------------------------------------------------
// Sidecar index
// ---------------------------------------------------------------------------

/// Per-page metadata persisted alongside the HTML so we can list pages, resolve
/// search hits to titles, and clean up the page's memory entry on rewrite/delete.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct PageMeta {
    title: String,
    #[serde(default)]
    memory_id: Option<String>,
    updated_at: String,
    #[serde(default)]
    tags: Vec<String>,
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

    // -- actions -----------------------------------------------------------

    async fn write_page(&self, args: &Value) -> Result<Value> {
        let slug = sanitize_slug(require_str(args, "slug")?)?;
        let title = require_str(args, "title")?.to_string();
        let body = require_str(args, "html")?.to_string();
        let tags = parse_string_array(&args["tags"]);
        let links = parse_links(&args["links"]);

        let rel_root = rel_root_for(&slug);
        let updated_at = now_rfc3339();
        let document = render_page(&title, &body, &links, &tags, &updated_at, &rel_root);

        let page_path = self.slug_to_path(&slug);
        if let Some(parent) = page_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                Error::ToolExecution(format!("failed to create wiki directory: {e}").into())
            })?;
        }
        std::fs::write(&page_path, &document).map_err(|e| {
            Error::ToolExecution(format!("failed to write wiki page {slug}: {e}").into())
        })?;

        // Index into hybrid memory. Replace any prior memory entry for this
        // slug so rewrites don't pile up stale facts.
        let plaintext = html_to_text(&body);
        let fact = build_memory_fact(&slug, &title, &tags, &plaintext);
        let mut mem_tags = vec!["wiki".to_string(), slug.clone()];
        mem_tags.extend(tags.iter().cloned());

        let guard = self.index_lock.lock().await;
        let mut index = self.load_index();
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
                memory_id: memory_id.clone(),
                updated_at: updated_at.clone(),
                tags: tags.clone(),
            },
        );
        self.save_index(&index)?;
        self.regenerate_index(&index)?;
        drop(guard);

        Ok(json!({
            "status": "saved",
            "slug": slug,
            "title": title,
            "path": page_path.to_string_lossy(),
            "url": format!("{slug}.html"),
            "indexed": memory_id.is_some(),
            "memory_id": memory_id,
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

        Ok(json!({
            "slug": slug,
            "title": title,
            "html": document,
            "text": html_to_text(&document),
            "links": extract_links(&document)
                .into_iter()
                .map(|(href, text)| json!({ "href": href, "text": text }))
                .collect::<Vec<_>>(),
            "tags": meta.map(|m| m.tags.clone()).unwrap_or_default(),
            "updated_at": meta.map(|m| m.updated_at.clone()),
        }))
    }

    async fn search_pages(&self, args: &Value) -> Result<Value> {
        let query = require_str(args, "query")?.to_string();
        let limit = args["limit"].as_u64().unwrap_or(5).clamp(1, 50) as usize;
        let index = self.load_index();

        let mut hits: Vec<Value> = Vec::new();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

        // Primary: hybrid memory recall, resolved back to wiki pages via the
        // [[wiki:<slug>]] marker embedded at write time. Over-fetch because the
        // backend's hybrid search ignores tag filters, so non-wiki hits are
        // mixed in and dropped here.
        if let Ok(result) = self
            .memory
            .search(&query, &["wiki".to_string()], limit * 4)
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
                        "snippet": snippet(content, 240),
                        "score": item.get("score").cloned().unwrap_or(Value::Null),
                        "source": "memory",
                    }));
                    if hits.len() >= limit {
                        break;
                    }
                }
            }
        }

        // Fallback / supplement: a direct text scan so pages are findable
        // immediately, before async memory extraction has indexed them.
        if hits.len() < limit {
            let needles: Vec<String> = query.split_whitespace().map(|w| w.to_lowercase()).collect();
            for (slug, meta) in &index.pages {
                if seen.contains(slug) {
                    continue;
                }
                let path = self.slug_to_path(slug);
                let Ok(document) = std::fs::read_to_string(&path) else {
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
                    "snippet": snippet(&text, 240),
                    "score": matches,
                    "source": "filesystem",
                }));
                if hits.len() >= limit {
                    break;
                }
            }
        }

        Ok(json!({
            "query": query,
            "count": hits.len(),
            "results": hits,
        }))
    }

    async fn list_pages(&self, _args: &Value) -> Result<Value> {
        let index = self.load_index();
        let pages: Vec<Value> = index
            .pages
            .iter()
            .map(|(slug, meta)| {
                json!({
                    "slug": slug,
                    "title": meta.title,
                    "url": format!("{slug}.html"),
                    "tags": meta.tags,
                    "updated_at": meta.updated_at,
                })
            })
            .collect();
        Ok(json!({
            "count": pages.len(),
            "pages": pages,
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

    fn slug_to_path(&self, slug: &str) -> PathBuf {
        self.wiki_dir.join(format!("{slug}.html"))
    }

    /// Rewrite `index.html` as a sorted table of contents over all known pages.
    fn regenerate_index(&self, index: &WikiIndex) -> Result<()> {
        let mut items = String::new();
        for (slug, meta) in &index.pages {
            items.push_str(&format!(
                "    <li><a href=\"{slug}.html\">{title}</a>{tags}</li>\n",
                slug = esc(slug),
                title = esc(&meta.title),
                tags = if meta.tags.is_empty() {
                    String::new()
                } else {
                    format!(
                        " <span class=\"tags\">{}</span>",
                        esc(&meta.tags.join(", "))
                    )
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
         body, optional cross-links and tags); 'read' returns a page's content \
         and links; 'search' finds relevant pages by meaning or text; 'list' \
         enumerates all pages; 'delete' removes a page. Pages are addressed by a \
         'slug' path like 'projects/rustykrab' and may link to each other."
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
                        "description": "Optional related pages, rendered as a 'See also' section (write only)"
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

fn build_memory_fact(slug: &str, title: &str, tags: &[String], plaintext: &str) -> String {
    let excerpt = snippet(plaintext, MEMORY_EXCERPT_LIMIT);
    format!(
        "Wiki page: {title}\n[[wiki:{slug}]]\nTags: {tags}\n\n{excerpt}",
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
.tags{{color:#666;font-size:.85rem}}\n\
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

/// Render a full content page: title, agent-supplied body, then a footer with
/// "see also" links, tags, and the last-updated timestamp.
fn render_page(
    title: &str,
    body: &str,
    links: &[(String, String)],
    tags: &[String],
    updated_at: &str,
    rel_root: &str,
) -> String {
    let mut footer = String::from("<footer>\n");
    if !links.is_empty() {
        footer.push_str("<p><strong>See also:</strong></p>\n<ul>\n");
        for (slug, label) in links {
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

    let content = format!("<h1>{}</h1>\n{}\n{}", esc(title), body, footer);
    render_document(title, &content, rel_root)
}

/// Strip HTML to readable plaintext: drop script/style, remove tags, decode a
/// handful of common entities, and collapse whitespace.
fn html_to_text(html: &str) -> String {
    let without_blocks = Regex::new(r"(?is)<(script|style)\b[^>]*>.*?</(script|style)>")
        .expect("static regex")
        .replace_all(html, " ")
        .into_owned();
    let without_tags = Regex::new(r"(?s)<[^>]+>")
        .expect("static regex")
        .replace_all(&without_blocks, " ")
        .into_owned();
    let decoded = without_tags
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&nbsp;", " ")
        .replace("&amp;", "&");
    Regex::new(r"\s+")
        .expect("static regex")
        .replace_all(&decoded, " ")
        .trim()
        .to_string()
}

/// Extract `(href, text)` pairs from anchor tags.
fn extract_links(html: &str) -> Vec<(String, String)> {
    let re = Regex::new(r#"(?is)<a\b[^>]*href\s*=\s*["']([^"']*)["'][^>]*>(.*?)</a>"#)
        .expect("static regex");
    re.captures_iter(html)
        .map(|c| {
            let href = c.get(1).map(|m| m.as_str().to_string()).unwrap_or_default();
            let text = html_to_text(c.get(2).map(|m| m.as_str()).unwrap_or(""));
            (href, text)
        })
        .collect()
}

fn extract_title(html: &str) -> Option<String> {
    Regex::new(r"(?is)<title>(.*?)</title>")
        .expect("static regex")
        .captures(html)
        .and_then(|c| c.get(1))
        .map(|m| html_to_text(m.as_str()))
}

/// Pull the `[[wiki:<slug>]]` back-reference out of an indexed memory fact.
fn extract_wiki_marker(content: &str) -> Option<String> {
    Regex::new(r"\[\[wiki:([^\]]+)\]\]")
        .expect("static regex")
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
    fn extract_wiki_marker_roundtrips() {
        let fact = build_memory_fact("projects/foo", "Foo", &["bar".into()], "body text");
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

        // Index page was regenerated.
        assert!(dir.path().join("index.html").exists());

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
