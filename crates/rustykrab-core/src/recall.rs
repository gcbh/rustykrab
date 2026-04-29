//! Per-conversation recall store for compaction-displaced history.
//!
//! Compaction in `rustykrab-agent` summarises long conversations into a
//! short bullet list, then drops the original messages from the prompt.
//! That keeps prompts cheap, but the dropped detail (concrete numbers,
//! file paths, intermediate tool outputs) is gone from the model's view.
//!
//! [`RecallStore`] preserves that detail out-of-band: the runner appends
//! the rendered text of every dropped batch into a per-conversation
//! buffer here, and a small set of REPL-style `recall_*` tools lets the
//! agent inspect it on demand. This mirrors the foundational RLM paper
//! (Zhang, Kraska, Khattab — arXiv 2512.24601): the long context lives
//! outside the prompt, and the model navigates it via tools rather than
//! re-reading it wholesale.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use uuid::Uuid;

/// Thread-safe map from conversation id to the rendered text of all
/// messages that have been displaced by compaction.
///
/// Append-only within a conversation: each compaction appends the
/// rendered batch, joined to the previous archive with a blank line.
#[derive(Debug, Default)]
pub struct RecallStore {
    inner: RwLock<HashMap<Uuid, Arc<String>>>,
}

impl RecallStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append `text` to the archive for `conversation_id`. If an entry
    /// already exists, the new text is concatenated with a blank-line
    /// separator so successive batches stay readable.
    pub fn append(&self, conversation_id: Uuid, text: &str) {
        if text.is_empty() {
            return;
        }
        let mut guard = self.inner.write().unwrap_or_else(|e| e.into_inner());
        let entry = guard.entry(conversation_id).or_default();
        let combined = if entry.is_empty() {
            text.to_string()
        } else {
            format!("{}\n\n{}", entry.as_str(), text)
        };
        *entry = Arc::new(combined);
    }

    /// Return a cheap clone of the archive for `conversation_id`, or
    /// `None` if nothing has been archived yet.
    pub fn get(&self, conversation_id: Uuid) -> Option<Arc<String>> {
        let guard = self.inner.read().unwrap_or_else(|e| e.into_inner());
        guard.get(&conversation_id).cloned()
    }

    /// Forget the archive for a conversation (used on session teardown).
    pub fn clear(&self, conversation_id: Uuid) {
        let mut guard = self.inner.write().unwrap_or_else(|e| e.into_inner());
        guard.remove(&conversation_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_creates_entry() {
        let store = RecallStore::new();
        let conv = Uuid::new_v4();
        store.append(conv, "first batch");
        let got = store.get(conv).expect("archive should exist");
        assert_eq!(got.as_str(), "first batch");
    }

    #[test]
    fn append_concatenates_with_blank_line() {
        let store = RecallStore::new();
        let conv = Uuid::new_v4();
        store.append(conv, "first");
        store.append(conv, "second");
        let got = store.get(conv).unwrap();
        assert_eq!(got.as_str(), "first\n\nsecond");
    }

    #[test]
    fn empty_text_is_a_noop() {
        let store = RecallStore::new();
        let conv = Uuid::new_v4();
        store.append(conv, "");
        assert!(store.get(conv).is_none());
    }

    #[test]
    fn conversations_are_isolated() {
        let store = RecallStore::new();
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        store.append(a, "a-text");
        store.append(b, "b-text");
        assert_eq!(store.get(a).unwrap().as_str(), "a-text");
        assert_eq!(store.get(b).unwrap().as_str(), "b-text");
    }

    #[test]
    fn clear_drops_entry() {
        let store = RecallStore::new();
        let conv = Uuid::new_v4();
        store.append(conv, "x");
        store.clear(conv);
        assert!(store.get(conv).is_none());
    }
}
