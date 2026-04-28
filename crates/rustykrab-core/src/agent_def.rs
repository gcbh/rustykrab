use std::collections::HashMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

/// A named sub-agent definition: system prompt, harness profile, and the
/// tool subset the sub-agent is allowed to use.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentDefinition {
    /// Stable identifier referenced by the `subagents` tool.
    pub id: String,
    /// Short description shown in `agents_list`.
    pub description: String,
    /// System prompt prepended to the sub-agent's conversation.
    pub system_prompt: String,
    /// Harness profile name: `coding`, `research`, `creative`, or `default`.
    pub profile: String,
    /// Tools the sub-agent may call. `None` means inherit the parent's
    /// active capability set unchanged.
    pub allowed_tools: Option<Vec<String>>,
}

/// Read-only catalog of [`AgentDefinition`]s.
#[derive(Debug, Default, Clone)]
pub struct AgentRegistry {
    by_id: HashMap<String, Arc<AgentDefinition>>,
}

impl AgentRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, def: AgentDefinition) {
        self.by_id.insert(def.id.clone(), Arc::new(def));
    }

    pub fn get(&self, id: &str) -> Option<Arc<AgentDefinition>> {
        self.by_id.get(id).cloned()
    }

    pub fn list(&self) -> Vec<Arc<AgentDefinition>> {
        let mut defs: Vec<_> = self.by_id.values().cloned().collect();
        defs.sort_by(|a, b| a.id.cmp(&b.id));
        defs
    }

    /// Built-in catalog: researcher, coder, planner.
    pub fn with_defaults() -> Self {
        let mut reg = Self::new();
        reg.insert(AgentDefinition {
            id: "researcher".into(),
            description: "Investigates a question using web/search/memory tools and returns a synthesized answer.".into(),
            system_prompt: "You are a focused research sub-agent. Answer the user's question by gathering evidence with the available tools, then return a concise synthesis with citations or sources where applicable. Do not ask follow-up questions; produce a final answer in one turn loop.".into(),
            profile: "research".into(),
            allowed_tools: None,
        });
        reg.insert(AgentDefinition {
            id: "coder".into(),
            description: "Reads, edits, and runs code to implement a change or diagnose a bug.".into(),
            system_prompt: "You are a focused coding sub-agent. Implement the requested change end-to-end: read relevant files, apply edits, and verify with tests or a build. Return a short summary of what you changed.".into(),
            profile: "coding".into(),
            allowed_tools: None,
        });
        reg.insert(AgentDefinition {
            id: "planner".into(),
            description: "Drafts a step-by-step implementation plan without making changes.".into(),
            system_prompt: "You are a planning sub-agent. Read enough of the codebase to ground your reasoning, then produce a concrete, ordered plan with file paths and named functions. Do not modify any files.".into(),
            profile: "default".into(),
            allowed_tools: None,
        });
        reg
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_contain_three_agents() {
        let reg = AgentRegistry::with_defaults();
        let ids: Vec<String> = reg.list().iter().map(|d| d.id.clone()).collect();
        assert_eq!(ids, vec!["coder", "planner", "researcher"]);
    }

    #[test]
    fn get_returns_definition_by_id() {
        let reg = AgentRegistry::with_defaults();
        let def = reg.get("coder").expect("coder agent exists");
        assert_eq!(def.profile, "coding");
    }

    #[test]
    fn unknown_id_returns_none() {
        let reg = AgentRegistry::with_defaults();
        assert!(reg.get("nonexistent").is_none());
    }
}
