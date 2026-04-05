use openclaw_core::Error;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// A stored memory entry — a summarized snapshot of a conversation
/// that can be recalled for context in future conversations.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryEntry {
    /// The conversation this memory was derived from.
    pub conversation_id: Uuid,
    /// The summary text.
    pub summary: String,
    /// Key topics/entities mentioned (for search/retrieval).
    pub tags: Vec<String>,
    /// When this memory was created.
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// Persistent memory store backed by a sled tree.
///
/// Stores conversation summaries so that context persists across
/// conversations. The agent can search memories by tag to recall
/// relevant context from previous interactions.
#[derive(Clone)]
pub struct MemoryStore {
    tree: sled::Tree,
}

impl MemoryStore {
    pub(crate) fn new(tree: sled::Tree) -> Self {
        Self { tree }
    }

    /// Store a memory entry.
    pub fn save(&self, entry: &MemoryEntry) -> Result<(), Error> {
        let key = entry.conversation_id.as_bytes().to_vec();
        let bytes = serde_json::to_vec(entry)?;
        self.tree
            .insert(key, bytes)
            .map_err(|e| Error::Storage(e.to_string()))?;
        Ok(())
    }

    /// Retrieve a memory by conversation ID.
    pub fn get(&self, conversation_id: Uuid) -> Result<MemoryEntry, Error> {
        let bytes = self
            .tree
            .get(conversation_id.as_bytes())
            .map_err(|e| Error::Storage(e.to_string()))?
            .ok_or_else(|| Error::NotFound(format!("memory for conversation {conversation_id}")))?;
        let entry: MemoryEntry = serde_json::from_slice(&bytes)?;
        Ok(entry)
    }

    /// Search memories by tag (simple substring match).
    /// Returns all memories that contain any of the given tags.
    pub fn search(&self, query_tags: &[String]) -> Result<Vec<MemoryEntry>, Error> {
        let mut results = Vec::new();
        for entry in self.tree.iter() {
            let (_, value) = entry.map_err(|e| Error::Storage(e.to_string()))?;
            let memory: MemoryEntry = serde_json::from_slice(&value)?;

            let matches = query_tags.iter().any(|qt| {
                let qt_lower = qt.to_lowercase();
                memory.tags.iter().any(|t| t.to_lowercase().contains(&qt_lower))
                    || memory.summary.to_lowercase().contains(&qt_lower)
            });

            if matches {
                results.push(memory);
            }
        }
        Ok(results)
    }

    /// List all memories (most recent first by convention).
    pub fn list_all(&self) -> Result<Vec<MemoryEntry>, Error> {
        let mut entries = Vec::new();
        for entry in self.tree.iter() {
            let (_, value) = entry.map_err(|e| Error::Storage(e.to_string()))?;
            let memory: MemoryEntry = serde_json::from_slice(&value)?;
            entries.push(memory);
        }
        entries.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        Ok(entries)
    }

    /// Delete a memory.
    pub fn delete(&self, conversation_id: Uuid) -> Result<(), Error> {
        self.tree
            .remove(conversation_id.as_bytes())
            .map_err(|e| Error::Storage(e.to_string()))?;
        Ok(())
    }
}
