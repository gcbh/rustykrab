//! Persistent knowledge graph for structured memory.
//!
//! Instead of stuffing everything into the context window, the agent
//! maintains a persistent knowledge graph. On each request, only the
//! relevant subgraph is retrieved and injected as context.
//!
//! Nodes: entities (people, projects, events, preferences)
//! Edges: relationships (works-with, depends-on, prefers, scheduled-for)

use rustykrab_core::orchestration::{EntityType, KnowledgeEntity, KnowledgeRelation, RelationType};
use rustykrab_core::Error;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Persistent knowledge graph backed by sled trees.
///
/// Uses two trees:
/// - `entities`: UUID → KnowledgeEntity
/// - `relations`: composite key → KnowledgeRelation
/// - `entity_names`: name (lowercased) → UUID (for name lookups)
#[derive(Clone)]
pub struct KnowledgeGraph {
    entities: sled::Tree,
    relations: sled::Tree,
    entity_names: sled::Tree,
}

impl KnowledgeGraph {
    pub(crate) fn new(
        entities: sled::Tree,
        relations: sled::Tree,
        entity_names: sled::Tree,
    ) -> Self {
        Self {
            entities,
            relations,
            entity_names,
        }
    }

    // --- Entity operations ---

    /// Add or update an entity in the graph.
    pub fn upsert_entity(&self, entity: &KnowledgeEntity) -> Result<(), Error> {
        let key = entity.id.as_bytes().to_vec();

        // If the entity already exists, check whether the name changed
        // and remove the stale name-index entry (fixes #162).
        if let Some(old_bytes) = self
            .entities
            .get(&key)
            .map_err(|e| Error::Storage(e.to_string()))?
        {
            let old_entity: KnowledgeEntity = serde_json::from_slice(&old_bytes)?;
            let old_name_key = old_entity.name.to_lowercase();
            let new_name_key = entity.name.to_lowercase();
            if old_name_key != new_name_key {
                self.entity_names
                    .remove(old_name_key.as_bytes())
                    .map_err(|e| Error::Storage(e.to_string()))?;
            }
        }

        let bytes = serde_json::to_vec(entity)?;
        self.entities
            .insert(key, bytes)
            .map_err(|e| Error::Storage(e.to_string()))?;

        // Index by lowercase name.
        let name_key = entity.name.to_lowercase();
        self.entity_names
            .insert(name_key.as_bytes(), entity.id.as_bytes().as_slice())
            .map_err(|e| Error::Storage(e.to_string()))?;

        Ok(())
    }

    /// Get an entity by ID.
    pub fn get_entity(&self, id: Uuid) -> Result<Option<KnowledgeEntity>, Error> {
        match self
            .entities
            .get(id.as_bytes())
            .map_err(|e| Error::Storage(e.to_string()))?
        {
            Some(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
            None => Ok(None),
        }
    }

    /// Find an entity by name (case-insensitive).
    pub fn find_by_name(&self, name: &str) -> Result<Option<KnowledgeEntity>, Error> {
        let name_key = name.to_lowercase();
        match self
            .entity_names
            .get(name_key.as_bytes())
            .map_err(|e| Error::Storage(e.to_string()))?
        {
            Some(id_bytes) => {
                let id = Uuid::from_slice(&id_bytes).map_err(|e| Error::Storage(e.to_string()))?;
                self.get_entity(id)
            }
            None => Ok(None),
        }
    }

    /// Search entities by type.
    pub fn find_by_type(&self, entity_type: &EntityType) -> Result<Vec<KnowledgeEntity>, Error> {
        let mut results = Vec::new();
        for entry in self.entities.iter() {
            let (_, value) = entry.map_err(|e| Error::Storage(e.to_string()))?;
            let entity: KnowledgeEntity = serde_json::from_slice(&value)?;
            if &entity.entity_type == entity_type {
                results.push(entity);
            }
        }
        Ok(results)
    }

    /// Search entities by keyword in name or attributes.
    pub fn search_entities(&self, query: &str) -> Result<Vec<KnowledgeEntity>, Error> {
        let query_lower = query.to_lowercase();
        let mut results = Vec::new();

        for entry in self.entities.iter() {
            let (_, value) = entry.map_err(|e| Error::Storage(e.to_string()))?;
            let entity: KnowledgeEntity = serde_json::from_slice(&value)?;

            let name_match = entity.name.to_lowercase().contains(&query_lower);
            let attr_match = entity
                .attributes
                .to_string()
                .to_lowercase()
                .contains(&query_lower);

            if name_match || attr_match {
                results.push(entity);
            }
        }

        Ok(results)
    }

    /// Delete an entity and all its relations.
    ///
    /// Relation removals are applied as an atomic `Batch` so a crash
    /// mid-delete cannot leave orphaned relations (fixes #168).
    pub fn delete_entity(&self, id: Uuid) -> Result<(), Error> {
        // Collect relation keys to remove first, then apply as an
        // atomic batch before removing the entity itself. This ordering
        // ensures a crash never leaves orphaned relations.
        let mut batch = sled::Batch::default();
        for entry in self.relations.iter() {
            let (key, value) = entry.map_err(|e| Error::Storage(e.to_string()))?;
            if let Ok(rel) = serde_json::from_slice::<KnowledgeRelation>(&value) {
                if rel.from_id == id || rel.to_id == id {
                    batch.remove(key);
                }
            }
        }
        self.relations
            .apply_batch(batch)
            .map_err(|e| Error::Storage(e.to_string()))?;

        // Remove the entity and its name index.
        if let Some(bytes) = self
            .entities
            .remove(id.as_bytes())
            .map_err(|e| Error::Storage(e.to_string()))?
        {
            let entity: KnowledgeEntity = serde_json::from_slice(&bytes)?;
            let name_key = entity.name.to_lowercase();
            self.entity_names
                .remove(name_key.as_bytes())
                .map_err(|e| Error::Storage(e.to_string()))?;
        }

        Ok(())
    }

    // --- Relation operations ---

    /// Add a relation between two entities.
    pub fn add_relation(&self, relation: &KnowledgeRelation) -> Result<(), Error> {
        let key = relation_key(&relation.from_id, &relation.to_id, &relation.relation_type);
        let bytes = serde_json::to_vec(relation)?;
        self.relations
            .insert(key, bytes)
            .map_err(|e| Error::Storage(e.to_string()))?;
        Ok(())
    }

    /// Get all relations from a given entity.
    pub fn relations_from(&self, entity_id: Uuid) -> Result<Vec<KnowledgeRelation>, Error> {
        let prefix = entity_id.as_bytes().to_vec();
        let mut results = Vec::new();

        for entry in self.relations.scan_prefix(&prefix) {
            let (_, value) = entry.map_err(|e| Error::Storage(e.to_string()))?;
            let rel: KnowledgeRelation = serde_json::from_slice(&value)?;
            // Verify the prefix scan returned a relation that actually
            // belongs to this entity (guards against key-collision after
            // deserialization, fixes #189).
            debug_assert_eq!(
                rel.from_id, entity_id,
                "prefix scan returned relation with unexpected from_id"
            );
            results.push(rel);
        }
        Ok(results)
    }

    /// Get all relations involving a given entity (incoming and outgoing).
    pub fn relations_for(&self, entity_id: Uuid) -> Result<Vec<KnowledgeRelation>, Error> {
        let mut results = Vec::new();
        for entry in self.relations.iter() {
            let (_, value) = entry.map_err(|e| Error::Storage(e.to_string()))?;
            let rel: KnowledgeRelation = serde_json::from_slice(&value)?;
            if rel.from_id == entity_id || rel.to_id == entity_id {
                results.push(rel);
            }
        }
        Ok(results)
    }

    /// Remove a specific relation.
    pub fn remove_relation(
        &self,
        from_id: Uuid,
        to_id: Uuid,
        relation_type: &RelationType,
    ) -> Result<(), Error> {
        let key = relation_key(&from_id, &to_id, relation_type);
        self.relations
            .remove(key)
            .map_err(|e| Error::Storage(e.to_string()))?;
        Ok(())
    }

    // --- Graph traversal ---

    /// Retrieve the relevant subgraph for a given set of entity IDs.
    ///
    /// Performs a breadth-first traversal up to `max_hops` away from
    /// the seed entities, collecting all entities and relations found.
    pub fn retrieve_subgraph(&self, seed_ids: &[Uuid], max_hops: usize) -> Result<SubGraph, Error> {
        use std::collections::{HashSet, VecDeque};

        let mut visited: HashSet<Uuid> = HashSet::new();
        let mut queue: VecDeque<(Uuid, usize)> = VecDeque::new();
        let mut entities = Vec::new();
        let mut relations = Vec::new();

        // Seed the BFS.
        for &id in seed_ids {
            if visited.insert(id) {
                queue.push_back((id, 0));
            }
        }

        while let Some((entity_id, depth)) = queue.pop_front() {
            // Fetch the entity.
            if let Some(entity) = self.get_entity(entity_id)? {
                entities.push(entity);
            }

            if depth >= max_hops {
                continue;
            }

            // Fetch all relations for this entity.
            let rels = self.relations_for(entity_id)?;
            for rel in &rels {
                // Queue the other end of the relation.
                let other = if rel.from_id == entity_id {
                    rel.to_id
                } else {
                    rel.from_id
                };
                if visited.insert(other) {
                    queue.push_back((other, depth + 1));
                }
            }
            relations.extend(rels);
        }

        Ok(SubGraph {
            entities,
            relations,
        })
    }

    /// Format a subgraph as context text suitable for injection into a prompt.
    pub fn subgraph_to_context(subgraph: &SubGraph) -> String {
        let mut lines = Vec::new();
        lines.push("Known entities:".to_string());

        for entity in &subgraph.entities {
            let attrs = if entity.attributes.is_null() || entity.attributes == serde_json::json!({})
            {
                String::new()
            } else {
                format!(" — {}", entity.attributes)
            };
            lines.push(format!(
                "- {} ({:?}){attrs}",
                entity.name, entity.entity_type
            ));
        }

        if !subgraph.relations.is_empty() {
            lines.push(String::new());
            lines.push("Relationships:".to_string());
            // Build a name lookup.
            let names: std::collections::HashMap<Uuid, &str> = subgraph
                .entities
                .iter()
                .map(|e| (e.id, e.name.as_str()))
                .collect();

            for rel in &subgraph.relations {
                let from = names.get(&rel.from_id).unwrap_or(&"?");
                let to = names.get(&rel.to_id).unwrap_or(&"?");
                lines.push(format!("- {from} --[{:?}]--> {to}", rel.relation_type));
            }
        }

        lines.join("\n")
    }

    /// List all entities.
    pub fn list_entities(&self) -> Result<Vec<KnowledgeEntity>, Error> {
        let mut results = Vec::new();
        for entry in self.entities.iter() {
            let (_, value) = entry.map_err(|e| Error::Storage(e.to_string()))?;
            let entity: KnowledgeEntity = serde_json::from_slice(&value)?;
            results.push(entity);
        }
        results.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        Ok(results)
    }
}

/// A subgraph extracted from the knowledge graph.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubGraph {
    pub entities: Vec<KnowledgeEntity>,
    pub relations: Vec<KnowledgeRelation>,
}

/// Build a composite key for a relation: from_id + to_id + relation_type hash.
fn relation_key(from_id: &Uuid, to_id: &Uuid, relation_type: &RelationType) -> Vec<u8> {
    let mut key = Vec::with_capacity(48);
    key.extend_from_slice(from_id.as_bytes());
    key.extend_from_slice(to_id.as_bytes());
    // Use a simple hash of the relation type for the key suffix.
    let type_str = serde_json::to_string(relation_type).unwrap_or_default();
    key.extend_from_slice(type_str.as_bytes());
    key
}
