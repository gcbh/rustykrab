use std::collections::HashMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::tool::mcp_server_of;

/// A named agent definition: system prompt, harness profile, the tools
/// visible from its first turn, and the ceiling of tools it may use.
///
/// Definitions are files (`<data dir>/agents/<name>.md`, loaded by
/// `rustykrab-skills`; the built-ins are embedded defaults of the same
/// format), used by sub-agents and by the control layer's local workers.
/// Construct one in code with `..Default::default()` so a field added here
/// does not break every literal.
///
/// Two tool lists, deliberately distinct (plan
/// `docs/plans/control-layer-and-worker-fleet.md`, section 12):
///
/// - `tools` and `mcp_servers` are the *visible set*: declared in the tools
///   array from turn 0, because the host already knows the work needs them.
///   The array then stays fixed for the run; anything found later arrives by
///   append. Keep it small on slow-prefill models: every thousand tokens of
///   schemas costs about five seconds of uncached prefill on qwen3.8.
/// - `allowed_tools` is the *ceiling*: what the agent may ever call.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
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
    /// Tools visible from turn 0 (declared before the first model call).
    pub tools: Vec<String>,
    /// MCP servers whose tools are visible from turn 0.
    pub mcp_servers: Vec<String>,
    /// Preferred model, when the definition has one. Advisory: routing by
    /// model is the worker registry's (Phase 3), not the runner's.
    pub model: Option<String>,
    /// Resources the agent may write, for the controller's single-writer
    /// rule. Empty means the definition does not narrow it.
    pub writable_resources: Vec<String>,
}

impl Default for AgentDefinition {
    fn default() -> Self {
        Self {
            id: String::new(),
            description: String::new(),
            system_prompt: String::new(),
            profile: "default".into(),
            allowed_tools: None,
            tools: Vec::new(),
            mcp_servers: Vec::new(),
            model: None,
            writable_resources: Vec::new(),
        }
    }
}

impl AgentDefinition {
    /// The names to declare from turn 0, out of `registered` (the host's
    /// tool names): every named tool that is registered, and every
    /// registered tool of a named MCP server (`mcp__<server>__*`, server
    /// matched without regard to case), within `allowed_tools` when it is
    /// set. Sorted and deduplicated. A named tool the host does not have is
    /// left out rather than failing the run: the definition describes what
    /// the agent starts with, not what the item requires.
    pub fn visible_set<'a>(&self, registered: impl IntoIterator<Item = &'a str>) -> Vec<String> {
        let within = |name: &str| {
            self.allowed_tools
                .as_ref()
                .is_none_or(|allowed| allowed.iter().any(|a| a == name))
        };
        let mut names: Vec<String> = registered
            .into_iter()
            .filter(|name| {
                self.tools.iter().any(|t| t == name)
                    || mcp_server_of(name).is_some_and(|server| {
                        self.mcp_servers
                            .iter()
                            .any(|s| s.eq_ignore_ascii_case(server))
                    })
            })
            .filter(|name| within(name))
            .map(str::to_string)
            .collect();
        names.sort();
        names.dedup();
        names
    }
}

/// Read-only catalog of [`AgentDefinition`]s. Filled from definition files
/// by `rustykrab_skills::agents`: the embedded built-ins first, then the
/// data dir's files, which replace a built-in of the same id.
#[derive(Debug, Default, Clone)]
pub struct AgentRegistry {
    by_id: HashMap<String, Arc<AgentDefinition>>,
}

impl AgentRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a definition, replacing any with the same id.
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
}

#[cfg(test)]
mod tests {
    use super::*;

    fn def(id: &str) -> AgentDefinition {
        AgentDefinition {
            id: id.into(),
            ..Default::default()
        }
    }

    #[test]
    fn registry_lists_by_id_and_later_inserts_replace() {
        let mut reg = AgentRegistry::new();
        reg.insert(def("researcher"));
        reg.insert(def("coder"));
        let mut coder = def("coder");
        coder.profile = "coding".into();
        reg.insert(coder);
        let ids: Vec<String> = reg.list().iter().map(|d| d.id.clone()).collect();
        assert_eq!(ids, ["coder", "researcher"]);
        assert_eq!(reg.get("coder").unwrap().profile, "coding");
        assert!(reg.get("nonexistent").is_none());
    }

    #[test]
    fn a_literal_with_defaults_stays_constructible() {
        let planner = AgentDefinition {
            id: "planner".into(),
            tools: vec!["work_plan".into()],
            ..Default::default()
        };
        assert_eq!(planner.profile, "default");
        assert!(planner.allowed_tools.is_none() && planner.mcp_servers.is_empty());
        // Old serialized definitions, without the new fields, still load.
        let old: AgentDefinition = serde_json::from_str(
            r#"{"id":"x","description":"d","system_prompt":"p","profile":"coding","allowed_tools":null}"#,
        )
        .unwrap();
        assert!(old.tools.is_empty() && old.model.is_none());
    }

    #[test]
    fn the_visible_set_is_named_tools_and_server_tools_within_the_ceiling() {
        let registered = [
            "read",
            "write",
            "exec",
            "mcp__linear__create_issue",
            "mcp__linear__search",
            "mcp__jira__search",
            "browser",
        ];
        let mut coder = AgentDefinition {
            tools: vec!["read".into(), "write".into(), "exec".into(), "gone".into()],
            mcp_servers: vec!["Linear".into()],
            ..Default::default()
        };
        assert_eq!(
            coder.visible_set(registered),
            [
                "exec",
                "mcp__linear__create_issue",
                "mcp__linear__search",
                "read",
                "write"
            ]
        );
        coder.allowed_tools = Some(vec!["read".into(), "mcp__linear__search".into()]);
        assert_eq!(
            coder.visible_set(registered),
            ["mcp__linear__search", "read"]
        );
        assert!(AgentDefinition::default()
            .visible_set(registered)
            .is_empty());
    }
}
