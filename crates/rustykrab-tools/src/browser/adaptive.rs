//! Adaptive selector store — Scrapling's `auto_save`/`auto_match` analogue.
//!
//! When `select` is called with `auto_save=true`, we persist a
//! lightweight fingerprint of every match (tag, attributes, text snippet,
//! structural path) keyed by a user-provided `auto_save_id`.
//!
//! On a later call with the same `auto_save_id` and `auto_match=true`,
//! if the requested CSS selector returns *no* matches we re-scan the
//! current document for the elements with the highest fingerprint
//! similarity and return those instead. This lets a saved scrape survive
//! benign DOM changes (renamed classes, reordered siblings).
//!
//! v1 of this store is in-memory. The `to_dump`/`from_dump` helpers exist
//! so persistence can be added later without changing call sites.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

use super::selectors::Match;

/// Lightweight fingerprint stored per saved element.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Fingerprint {
    pub tag: String,
    pub path: String,
    pub text_snippet: String,
    pub attributes: HashMap<String, String>,
}

impl Fingerprint {
    pub fn from_match(m: &Match) -> Self {
        let snippet = if m.text.len() > 120 {
            // Cut at char boundary to avoid panicking on multi-byte chars.
            let mut end = 120;
            while end > 0 && !m.text.is_char_boundary(end) {
                end -= 1;
            }
            m.text[..end].to_string()
        } else {
            m.text.clone()
        };
        let mut attrs = HashMap::new();
        for (k, v) in &m.attributes {
            if let Some(s) = v.as_str() {
                attrs.insert(k.clone(), s.to_string());
            }
        }
        Self {
            tag: m.tag.clone(),
            path: m.path.clone(),
            text_snippet: snippet,
            attributes: attrs,
        }
    }

    /// Compute a similarity score against another fingerprint in [0, 1].
    pub fn similarity(&self, other: &Self) -> f64 {
        let mut score = 0.0;
        let mut weight = 0.0;

        // Tag agreement is a hard prior — same tag adds 0.2.
        weight += 0.2;
        if self.tag == other.tag {
            score += 0.2;
        }

        // Path overlap (longest-common path tokens).
        weight += 0.2;
        score += 0.2 * path_overlap(&self.path, &other.path);

        // Attribute Jaccard.
        weight += 0.3;
        score += 0.3 * attr_jaccard(&self.attributes, &other.attributes);

        // Text snippet token Jaccard.
        weight += 0.3;
        score += 0.3 * text_jaccard(&self.text_snippet, &other.text_snippet);

        if weight == 0.0 {
            0.0
        } else {
            score / weight
        }
    }
}

fn path_overlap(a: &str, b: &str) -> f64 {
    let aa: Vec<&str> = a.split(" > ").filter(|s| !s.is_empty()).collect();
    let bb: Vec<&str> = b.split(" > ").filter(|s| !s.is_empty()).collect();
    if aa.is_empty() || bb.is_empty() {
        return 0.0;
    }
    let mut common = 0usize;
    for (x, y) in aa.iter().rev().zip(bb.iter().rev()) {
        if x == y {
            common += 1;
        } else {
            break;
        }
    }
    common as f64 / aa.len().max(bb.len()) as f64
}

fn attr_jaccard(a: &HashMap<String, String>, b: &HashMap<String, String>) -> f64 {
    let mut both = 0usize;
    let mut either = 0usize;
    let mut seen = std::collections::HashSet::new();
    for k in a.keys().chain(b.keys()) {
        if !seen.insert(k.clone()) {
            continue;
        }
        let av = a.get(k);
        let bv = b.get(k);
        match (av, bv) {
            (Some(x), Some(y)) if x == y => {
                both += 1;
                either += 1;
            }
            (Some(_), Some(_)) | (Some(_), None) | (None, Some(_)) => {
                either += 1;
            }
            _ => {}
        }
    }
    if either == 0 {
        0.0
    } else {
        both as f64 / either as f64
    }
}

fn text_jaccard(a: &str, b: &str) -> f64 {
    let toks_a: std::collections::HashSet<String> = tokenize(a);
    let toks_b: std::collections::HashSet<String> = tokenize(b);
    if toks_a.is_empty() && toks_b.is_empty() {
        return 0.0;
    }
    let inter = toks_a.intersection(&toks_b).count();
    let union = toks_a.union(&toks_b).count();
    if union == 0 {
        0.0
    } else {
        inter as f64 / union as f64
    }
}

fn tokenize(s: &str) -> std::collections::HashSet<String> {
    s.split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(|t| t.to_lowercase())
        .collect()
}

/// Process-wide adaptive store keyed by `auto_save_id`.
#[derive(Default)]
pub struct AdaptiveStore {
    inner: Arc<Mutex<HashMap<String, Vec<Fingerprint>>>>,
}

impl AdaptiveStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Save fingerprints under `id`, replacing any prior set.
    pub async fn save(&self, id: &str, matches: &[Match]) {
        let prints: Vec<Fingerprint> = matches.iter().map(Fingerprint::from_match).collect();
        let mut g = self.inner.lock().await;
        g.insert(id.to_string(), prints);
    }

    /// Look up saved fingerprints.
    pub async fn load(&self, id: &str) -> Option<Vec<Fingerprint>> {
        let g = self.inner.lock().await;
        g.get(id).cloned()
    }

    /// Find the best matching candidate from `candidates` for each saved
    /// fingerprint at `id`. Returns one match per saved fingerprint above
    /// `threshold`. If `id` is not in the store, returns an empty vec.
    pub async fn match_against(
        &self,
        id: &str,
        candidates: &[Match],
        threshold: f64,
    ) -> Vec<(Match, f64)> {
        let saved = match self.load(id).await {
            Some(v) => v,
            None => return Vec::new(),
        };
        let cand_prints: Vec<Fingerprint> =
            candidates.iter().map(Fingerprint::from_match).collect();

        let mut out = Vec::with_capacity(saved.len());
        for s in &saved {
            let mut best: Option<(usize, f64)> = None;
            for (i, c) in cand_prints.iter().enumerate() {
                let sim = s.similarity(c);
                if sim > best.map(|(_, b)| b).unwrap_or(-1.0) {
                    best = Some((i, sim));
                }
            }
            if let Some((idx, sim)) = best {
                if sim >= threshold {
                    out.push((candidates[idx].clone(), sim));
                }
            }
        }
        out
    }

    /// Dump the entire store (for diagnostics / future persistence).
    #[allow(dead_code)]
    pub async fn to_dump(&self) -> Value {
        let g = self.inner.lock().await;
        let mut out = Map::new();
        for (id, prints) in g.iter() {
            let arr: Vec<Value> = prints
                .iter()
                .map(|p| serde_json::to_value(p).unwrap_or(Value::Null))
                .collect();
            out.insert(id.clone(), Value::Array(arr));
        }
        Value::Object(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value as JsonValue;

    fn mk_match(tag: &str, path: &str, text: &str, attrs: &[(&str, &str)]) -> Match {
        let mut a = serde_json::Map::new();
        for (k, v) in attrs {
            a.insert((*k).to_string(), JsonValue::String((*v).to_string()));
        }
        Match {
            tag: tag.to_string(),
            text: text.to_string(),
            html: None,
            attributes: a,
            value: None,
            path: path.to_string(),
        }
    }

    #[test]
    fn similarity_identical_is_one() {
        let m = mk_match("a", "body > a", "click here", &[("href", "/x")]);
        let f = Fingerprint::from_match(&m);
        let sim = f.similarity(&f);
        assert!((sim - 1.0).abs() < 1e-9, "got {sim}");
    }

    #[test]
    fn similarity_different_tag_lowers_score() {
        let a = Fingerprint::from_match(&mk_match("a", "body > a", "click", &[("href", "/x")]));
        let b = Fingerprint::from_match(&mk_match(
            "button",
            "body > button",
            "click",
            &[("type", "submit")],
        ));
        let s = a.similarity(&b);
        assert!(s < 0.5, "expected lowish similarity, got {s}");
    }

    #[tokio::test]
    async fn adaptive_relocates_after_dom_drift() {
        let store = AdaptiveStore::new();
        // Save the original element.
        let original = mk_match(
            "a",
            "html > body > nav > a:nth-of-type(2)",
            "Pricing page",
            &[("href", "/pricing"), ("class", "nav-link")],
        );
        store.save("scrape1", &[original]).await;

        // Imagine the page changed: same link is now at a different
        // position and a class was renamed, but text and href survive.
        let drifted = mk_match(
            "a",
            "html > body > header > nav > a:nth-of-type(3)",
            "Pricing page",
            &[("href", "/pricing"), ("class", "navbar__link")],
        );
        let unrelated = mk_match(
            "a",
            "html > body > footer > a",
            "Contact",
            &[("href", "/contact")],
        );

        let candidates = vec![drifted.clone(), unrelated];
        let scored = store.match_against("scrape1", &candidates, 0.4).await;
        assert_eq!(scored.len(), 1);
        assert_eq!(scored[0].0.text, "Pricing page");
    }

    #[tokio::test]
    async fn adaptive_below_threshold_returns_empty() {
        let store = AdaptiveStore::new();
        let saved = mk_match("a", "body > a", "buy", &[("href", "/buy")]);
        store.save("s", &[saved]).await;

        let unrelated = vec![mk_match(
            "footer",
            "body > footer",
            "completely different text",
            &[],
        )];
        let scored = store.match_against("s", &unrelated, 0.95).await;
        assert!(scored.is_empty());
    }

    #[tokio::test]
    async fn adaptive_unknown_id_returns_empty() {
        let store = AdaptiveStore::new();
        let scored = store
            .match_against("never-saved", &[mk_match("a", "body > a", "x", &[])], 0.0)
            .await;
        assert!(scored.is_empty());
    }
}
