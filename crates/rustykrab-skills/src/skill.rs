use async_trait::async_trait;
use rustykrab_core::types::ToolSchema;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, RwLock};

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

#[derive(Default)]
struct Inner {
    skills: HashMap<String, Arc<dyn Skill>>,
    md_skills_map: HashMap<String, Arc<SkillMd>>,
}

/// Registry that holds all available skills.
///
/// Uses interior mutability so tools can hot-register and hot-remove skills
/// against a shared `Arc<SkillRegistry>` without a restart.
pub struct SkillRegistry {
    inner: RwLock<Inner>,
}

impl SkillRegistry {
    pub fn new() -> Self {
        Self {
            inner: RwLock::new(Inner::default()),
        }
    }

    pub fn register(&self, skill: Arc<dyn Skill>) {
        let mut g = self.inner.write().expect("SkillRegistry poisoned");
        g.skills.insert(skill.id().to_string(), skill);
    }

    /// Register a SKILL.md-based skill (stored in both maps).
    pub fn register_md(&self, skill: Arc<SkillMd>) {
        let mut g = self.inner.write().expect("SkillRegistry poisoned");
        g.skills
            .insert(skill.frontmatter.name.clone(), skill.clone());
        g.md_skills_map
            .insert(skill.frontmatter.name.clone(), skill);
    }

    /// Remove a skill (by name) from both maps. Returns true if it existed.
    pub fn unregister(&self, id: &str) -> bool {
        let mut g = self.inner.write().expect("SkillRegistry poisoned");
        let had_md = g.md_skills_map.remove(id).is_some();
        let had_any = g.skills.remove(id).is_some();
        had_md || had_any
    }

    pub fn get(&self, id: &str) -> Option<Arc<dyn Skill>> {
        let g = self.inner.read().expect("SkillRegistry poisoned");
        g.skills.get(id).cloned()
    }

    /// Access rich SKILL.md metadata by id.
    pub fn get_md(&self, id: &str) -> Option<Arc<SkillMd>> {
        let g = self.inner.read().expect("SkillRegistry poisoned");
        g.md_skills_map.get(id).cloned()
    }

    /// List all SKILL.md skills (for XML catalog injection).
    pub fn md_skills(&self) -> Vec<Arc<SkillMd>> {
        let g = self.inner.read().expect("SkillRegistry poisoned");
        g.md_skills_map.values().cloned().collect()
    }

    pub fn list(&self) -> Vec<SkillManifest> {
        let g = self.inner.read().expect("SkillRegistry poisoned");
        g.skills
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
