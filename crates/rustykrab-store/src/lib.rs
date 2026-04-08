mod conversation;
pub mod keychain;
mod knowledge_graph;
pub mod memory;
mod secret;

use std::path::Path;

use rustykrab_core::Error;
use zeroize::Zeroizing;

pub use conversation::ConversationStore;
pub use knowledge_graph::{KnowledgeGraph, SubGraph};
pub use memory::{MemoryEntry, MemoryStore};
pub use secret::SecretStore;

/// Top-level database handle wrapping a sled instance.
///
/// The master key is wrapped in `Zeroizing` so it is securely erased
/// from memory when the Store is dropped.
#[derive(Clone)]
pub struct Store {
    db: sled::Db,
    master_key: Zeroizing<Vec<u8>>,
    // Pre-opened trees to avoid panics on every accessor call
    conversations_tree: sled::Tree,
    secrets_tree: sled::Tree,
    memories_tree: sled::Tree,
    kg_entities_tree: sled::Tree,
    kg_relations_tree: sled::Tree,
    kg_entity_names_tree: sled::Tree,
}

impl Store {
    /// Open (or create) a store at the given directory path.
    ///
    /// `master_key` is used to encrypt secrets at rest. It should be
    /// sourced from the OS keychain or an environment variable — never
    /// stored alongside the database.
    pub fn open(path: impl AsRef<Path>, master_key: Vec<u8>) -> Result<Self, Error> {
        let db = sled::open(path).map_err(|e| Error::Storage(e.to_string()))?;
        let conversations_tree = db.open_tree("conversations")
            .map_err(|e| Error::Storage(e.to_string()))?;
        let secrets_tree = db.open_tree("secrets")
            .map_err(|e| Error::Storage(e.to_string()))?;
        let memories_tree = db.open_tree("memories")
            .map_err(|e| Error::Storage(e.to_string()))?;
        let kg_entities_tree = db.open_tree("kg_entities")
            .map_err(|e| Error::Storage(e.to_string()))?;
        let kg_relations_tree = db.open_tree("kg_relations")
            .map_err(|e| Error::Storage(e.to_string()))?;
        let kg_entity_names_tree = db.open_tree("kg_entity_names")
            .map_err(|e| Error::Storage(e.to_string()))?;
        Ok(Self {
            db,
            master_key: Zeroizing::new(master_key),
            conversations_tree,
            secrets_tree,
            memories_tree,
            kg_entities_tree,
            kg_relations_tree,
            kg_entity_names_tree,
        })
    }

    /// Return a handle for conversation operations.
    pub fn conversations(&self) -> ConversationStore {
        ConversationStore::new(self.conversations_tree.clone())
    }

    /// Return a handle for encrypted secret operations.
    pub fn secrets(&self) -> SecretStore {
        SecretStore::new(self.secrets_tree.clone(), (*self.master_key).clone())
    }

    /// Return a handle for conversation memory/RAG operations.
    pub fn memories(&self) -> MemoryStore {
        MemoryStore::new(self.memories_tree.clone())
    }

    /// Return a handle for the persistent knowledge graph.
    pub fn knowledge_graph(&self) -> KnowledgeGraph {
        KnowledgeGraph::new(
            self.kg_entities_tree.clone(),
            self.kg_relations_tree.clone(),
            self.kg_entity_names_tree.clone(),
        )
    }

    /// Flush all pending writes to disk.
    pub fn flush(&self) -> Result<(), Error> {
        self.db
            .flush()
            .map_err(|e| Error::Storage(e.to_string()))?;
        Ok(())
    }
}
