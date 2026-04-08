use chrono::Utc;
use rustykrab_core::types::Conversation;
use rustykrab_core::Error;
use uuid::Uuid;

/// CRUD operations on conversations backed by a sled tree.
#[derive(Clone)]
pub struct ConversationStore {
    tree: sled::Tree,
}

impl ConversationStore {
    pub(crate) fn new(tree: sled::Tree) -> Self {
        Self { tree }
    }

    /// Create a new empty conversation and return it.
    pub fn create(&self) -> Result<Conversation, Error> {
        let conv = Conversation {
            id: Uuid::new_v4(),
            messages: Vec::new(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
            summary: None,
            detected_profile: None,
        };
        self.save(&conv)?;
        Ok(conv)
    }

    /// Persist a conversation (insert or update).
    pub fn save(&self, conv: &Conversation) -> Result<(), Error> {
        let bytes = serde_json::to_vec(conv)?;
        self.tree
            .insert(conv.id.as_bytes(), bytes)
            .map_err(|e| Error::Storage(e.to_string()))?;
        self.tree
            .flush()
            .map_err(|e| Error::Storage(e.to_string()))?;
        Ok(())
    }

    /// Retrieve a conversation by ID.
    pub fn get(&self, id: Uuid) -> Result<Conversation, Error> {
        let bytes = self
            .tree
            .get(id.as_bytes())
            .map_err(|e| Error::Storage(e.to_string()))?
            .ok_or_else(|| Error::NotFound(format!("conversation {id}")))?;
        let conv: Conversation = serde_json::from_slice(&bytes)?;
        Ok(conv)
    }

    /// List all conversation IDs (lightweight, doesn't deserialize messages).
    pub fn list_ids(&self) -> Result<Vec<Uuid>, Error> {
        let mut ids = Vec::new();
        for entry in self.tree.iter() {
            let (key, _) = entry.map_err(|e| Error::Storage(e.to_string()))?;
            let id =
                Uuid::from_slice(&key).map_err(|e| Error::Storage(e.to_string()))?;
            ids.push(id);
        }
        Ok(ids)
    }

    /// Delete a conversation by ID.
    pub fn delete(&self, id: Uuid) -> Result<(), Error> {
        self.tree
            .remove(id.as_bytes())
            .map_err(|e| Error::Storage(e.to_string()))?;
        Ok(())
    }
}
