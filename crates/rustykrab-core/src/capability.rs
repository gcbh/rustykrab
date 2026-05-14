use serde::{Deserialize, Serialize};
use std::collections::HashSet;

/// Names of the sub-agent / session-management tools that, in addition to
/// a matching [`Capability::Tool`] grant, require [`Capability::Subagent`]
/// to use. Centralising this list keeps [`CapabilitySet::can_use_tool`]
/// and any consumer that filters tool schemas in sync.
///
/// These tools can spawn nested agent loops or coordinate other
/// sessions, so they get an extra gate beyond the per-tool capability
/// — they are off-by-default in `for_tools_permissive` and only granted
/// when the runtime explicitly opts in.
pub const SUBAGENT_TOOL_NAMES: &[&str] = &[
    "subagents",
    "agents_list",
    "sessions_list",
    "sessions_history",
    "sessions_send",
    "sessions_spawn",
    "sessions_yield",
    "session_status",
];

/// Returns whether `tool_name` is one of the sub-agent / session-management
/// tools that require [`Capability::Subagent`].
pub fn is_subagent_tool(tool_name: &str) -> bool {
    SUBAGENT_TOOL_NAMES.contains(&tool_name)
}

/// A capability that can be granted to a conversation session.
///
/// Capabilities follow the principle of least privilege — each
/// conversation only gets access to what it explicitly needs.
/// This prevents the session isolation failures from the original
/// RustyKrab where data leaked across user sessions.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    /// Can read files from the filesystem.
    FileRead,
    /// Can write files to the filesystem.
    FileWrite,
    /// Can execute shell commands.
    ShellExec,
    /// Can make outbound HTTP requests.
    HttpRequest,
    /// Can perform raw-packet network discovery on the local LAN
    /// (e.g. ARP sweeps, mDNS browsing, broadcast probes). Distinct
    /// from `HttpRequest` so a session can be allowed to fetch URLs
    /// without also getting raw-socket discovery, and vice versa.
    NetDiscovery,
    /// Can access a specific messaging channel by name.
    Channel(String),
    /// Can use a specific tool by name.
    Tool(String),
    /// Can read/write secrets.
    SecretAccess,
    /// Can use the sub-agent / session-management tool family
    /// (`subagents`, `agents_list`, `sessions_*`). Required in addition
    /// to [`Capability::Tool`] for those tools so spawning nested agent
    /// loops is opt-in even when the tools are registered.
    Subagent,
    /// Administrative — can manage other sessions.
    Admin,
}

/// A set of capabilities scoped to a single conversation session.
///
/// Created when a conversation starts; checked before every tool
/// execution and resource access.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapabilitySet {
    capabilities: HashSet<Capability>,
}

impl CapabilitySet {
    /// Create an empty capability set (deny all).
    pub fn none() -> Self {
        Self {
            capabilities: HashSet::new(),
        }
    }

    /// Create a default set with safe capabilities only.
    pub fn default_safe() -> Self {
        let mut caps = HashSet::new();
        caps.insert(Capability::HttpRequest);
        Self { capabilities: caps }
    }

    /// Grant a capability.
    pub fn grant(&mut self, cap: Capability) {
        self.capabilities.insert(cap);
    }

    /// Revoke a capability.
    pub fn revoke(&mut self, cap: &Capability) {
        self.capabilities.remove(cap);
    }

    /// Check whether a capability is granted.
    pub fn has(&self, cap: &Capability) -> bool {
        self.capabilities.contains(cap)
    }

    /// Check whether the set has permission to use a specific tool.
    ///
    /// Normalises the tool name before lookup: trims whitespace and, if
    /// the name contains a colon separator (e.g. `"some_tool:subaction"`
    /// emitted by some models), checks the base name as well.
    ///
    /// For tools in [`SUBAGENT_TOOL_NAMES`] this additionally requires
    /// [`Capability::Subagent`] — see that constant for rationale.
    pub fn can_use_tool(&self, tool_name: &str) -> bool {
        let trimmed = tool_name.trim();
        let base = trimmed.split(':').next().unwrap_or(trimmed);

        // Two-layer gate for the sub-agent family: even if `Tool(name)`
        // is granted, the session also needs `Subagent`.
        if is_subagent_tool(base) && !self.capabilities.contains(&Capability::Subagent) {
            return false;
        }

        if self
            .capabilities
            .contains(&Capability::Tool(trimmed.to_string()))
        {
            return true;
        }
        // Fall back to the base name before any colon separator.
        if base != trimmed {
            return self
                .capabilities
                .contains(&Capability::Tool(base.to_string()));
        }
        false
    }

    /// Create a capability set that grants access to a specific set of tools
    /// with only safe defaults (`HttpRequest` + `Tool(name)` entries).
    ///
    /// Use `for_tools_permissive()` if you need file, shell, and other
    /// resource capabilities as well (and understand the security tradeoff).
    pub fn for_tools(tool_names: &[&str]) -> Self {
        let mut caps = HashSet::new();
        caps.insert(Capability::HttpRequest);
        for name in tool_names {
            caps.insert(Capability::Tool(name.to_string()));
        }
        Self { capabilities: caps }
    }

    /// Create a capability set that grants access to a specific set of tools
    /// plus all standard resource capabilities (file read/write, shell, http).
    ///
    /// # Security
    /// This grants broad permissions. Prefer `for_tools()` and explicitly
    /// granting only the capabilities each tool actually needs.
    pub fn for_tools_permissive(tool_names: &[&str]) -> Self {
        let mut caps = HashSet::new();
        caps.insert(Capability::FileRead);
        caps.insert(Capability::FileWrite);
        caps.insert(Capability::ShellExec);
        caps.insert(Capability::HttpRequest);
        for name in tool_names {
            caps.insert(Capability::Tool(name.to_string()));
        }
        Self { capabilities: caps }
    }

    /// Return all granted capabilities.
    pub fn list(&self) -> impl Iterator<Item = &Capability> {
        self.capabilities.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn for_tools_permissive_grants_resource_capabilities() {
        let tools = &["http_request", "read", "exec", "example_tool"];
        let caps = CapabilitySet::for_tools_permissive(tools);

        assert!(caps.can_use_tool("example_tool"));
        assert!(caps.can_use_tool("http_request"));
        assert!(caps.has(&Capability::FileRead));
        assert!(caps.has(&Capability::ShellExec));
        assert!(caps.has(&Capability::HttpRequest));
    }

    #[test]
    fn can_use_tool_trims_whitespace() {
        let caps = CapabilitySet::for_tools(&["example_tool"]);
        assert!(caps.can_use_tool("example_tool"));
        assert!(caps.can_use_tool("example_tool "));
        assert!(caps.can_use_tool(" example_tool"));
        assert!(caps.can_use_tool(" example_tool "));
    }

    #[test]
    fn can_use_tool_handles_colon_separator() {
        let caps = CapabilitySet::for_tools(&["example_tool"]);
        assert!(caps.can_use_tool("example_tool:subaction"));
        assert!(caps.can_use_tool("example_tool:another"));
    }

    #[test]
    fn can_use_tool_rejects_unknown() {
        let caps = CapabilitySet::for_tools(&["example_tool"]);
        assert!(!caps.can_use_tool("other_tool"));
        assert!(!caps.can_use_tool("unknown_tool"));
    }

    #[test]
    fn default_safe_denies_tools() {
        let caps = CapabilitySet::default_safe();
        assert!(!caps.can_use_tool("example_tool"));
        assert!(caps.has(&Capability::HttpRequest));
    }

    #[test]
    fn subagent_tools_blocked_without_subagent_capability() {
        // Even with `Tool("subagents")` granted, the session still needs
        // `Subagent` to actually use it.
        let caps = CapabilitySet::for_tools_permissive(&["subagents", "agents_list", "read"]);
        assert!(!caps.can_use_tool("subagents"));
        assert!(!caps.can_use_tool("agents_list"));
        assert!(caps.can_use_tool("read"));
    }

    #[test]
    fn subagent_tools_allowed_when_subagent_capability_granted() {
        let mut caps = CapabilitySet::for_tools_permissive(&["subagents", "agents_list"]);
        caps.grant(Capability::Subagent);
        assert!(caps.can_use_tool("subagents"));
        assert!(caps.can_use_tool("agents_list"));
    }

    #[test]
    fn subagent_capability_alone_does_not_grant_other_tools() {
        let mut caps = CapabilitySet::none();
        caps.grant(Capability::Subagent);
        // No `Tool(...)` grant, so still denied.
        assert!(!caps.can_use_tool("subagents"));
        assert!(!caps.can_use_tool("read"));
    }

    #[test]
    fn all_session_tools_gated_by_subagent_capability() {
        let names: Vec<&str> = SUBAGENT_TOOL_NAMES.to_vec();
        let caps = CapabilitySet::for_tools_permissive(&names);
        // Without Capability::Subagent, every tool in the family is denied.
        for name in &names {
            assert!(!caps.can_use_tool(name), "{name} should be denied");
        }
        // After granting, all are allowed.
        let mut caps = caps;
        caps.grant(Capability::Subagent);
        for name in &names {
            assert!(caps.can_use_tool(name), "{name} should be allowed");
        }
    }

    #[test]
    fn for_tools_permissive_does_not_grant_subagent() {
        let caps = CapabilitySet::for_tools_permissive(&["subagents"]);
        assert!(!caps.has(&Capability::Subagent));
    }
}
