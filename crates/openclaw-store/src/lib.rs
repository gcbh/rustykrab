mod conversation;
mod memory;
mod secret;

use std::path::Path;

use openclaw_core::Error;

pub use conversation::ConversationStore;
pub use memory::{MemoryEntry, MemoryStore};
pub use secret::SecretStore;

/// Top-level database handle wrapping a sled instance.
#[derive(Clone)]
pub struct Store {
    db: sled::Db,
    master_key: Vec<u8>,
}

impl Store {
    /// Open (or create) a store at the given directory path.
    ///
    /// `master_key` is used to encrypt secrets at rest. It should be
    /// sourced from the OS keychain or an environment variable — never
    /// stored alongside the database.
    pub fn open(path: impl AsRef<Path>, master_key: Vec<u8>) -> Result<Self, Error> {
        let db = sled::open(path).map_err(|e| Error::Storage(e.to_string()))?;
        Ok(Self { db, master_key })
    }

    /// Return a handle for conversation operations.
    pub fn conversations(&self) -> ConversationStore {
        let tree = self
            .db
            .open_tree("conversations")
            .expect("failed to open conversations tree");
        ConversationStore::new(tree)
    }

    /// Return a handle for encrypted secret operations.
    pub fn secrets(&self) -> SecretStore {
        let tree = self
            .db
            .open_tree("secrets")
            .expect("failed to open secrets tree");
        SecretStore::new(tree, self.master_key.clone())
    }

    /// Return a handle for conversation memory/RAG operations.
    pub fn memories(&self) -> MemoryStore {
        let tree = self
            .db
            .open_tree("memories")
            .expect("failed to open memories tree");
        MemoryStore::new(tree)
    }

    /// Flush all pending writes to disk.
    pub fn flush(&self) -> Result<(), Error> {
        self.db
            .flush()
            .map_err(|e| Error::Storage(e.to_string()))?;
        Ok(())
    }
}
