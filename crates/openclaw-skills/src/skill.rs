use async_trait::async_trait;
use openclaw_core::types::ToolSchema;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;

use crate::skill_md::SkillMd;

/// A skill is a composable unit: a system prompt plus a set of tools
/// that together give an agent a specific capability.
#[async_trait]
pub trait Skill: Send + Sync {
    /// Unique identifier for this skill.
    fn id(&self) -> &str;

    /// Human-readable name.
    fn name(&self) -> &str;

    /// System prompt fragment injected when this skill is active.
    fn system_prompt(&self) -> &str;

    /// Tool schemas this skill contributes.
    fn tools(&self) -> Vec<ToolSchema>;
}

/// Metadata describing a skill (for listing / UI).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillManifest {
    pub id: String,
    pub name: String,
    pub description: String,
}

/// Registry that holds all available skills.
pub struct SkillRegistry {
    skills: HashMap<String, Arc<dyn Skill>>,
    md_skills_map: HashMap<String, Arc<SkillMd>>,
}

impl SkillRegistry {
    pub fn new() -> Self {
        Self {
            skills: HashMap::new(),
            md_skills_map: HashMap::new(),
        }
    }

    pub fn register(&mut self, skill: Arc<dyn Skill>) {
        self.skills.insert(skill.id().to_string(), skill);
    }

    /// Register a SKILL.md-based skill (stored in both maps).
    pub fn register_md(&mut self, skill: Arc<SkillMd>) {
        self.skills
            .insert(skill.frontmatter.name.clone(), skill.clone());
        self.md_skills_map
            .insert(skill.frontmatter.name.clone(), skill);
    }

    pub fn get(&self, id: &str) -> Option<&Arc<dyn Skill>> {
        self.skills.get(id)
    }

    /// Access rich SKILL.md metadata by id.
    pub fn get_md(&self, id: &str) -> Option<&Arc<SkillMd>> {
        self.md_skills_map.get(id)
    }

    /// List all SKILL.md skills (for XML catalog injection).
    pub fn md_skills(&self) -> Vec<&Arc<SkillMd>> {
        self.md_skills_map.values().collect()
    }

    pub fn list(&self) -> Vec<SkillManifest> {
        self.skills
            .values()
            .map(|s| SkillManifest {
                id: s.id().to_string(),
                name: s.name().to_string(),
                description: s.system_prompt().lines().next().unwrap_or("").to_string(),
            })
            .collect()
    }
}

impl Default for SkillRegistry {
    fn default() -> Self {
        Self::new()
    }
}
