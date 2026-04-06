use serde::{Deserialize, Serialize};
use std::collections::HashSet;

/// A capability that can be granted to a conversation session.
///
/// Capabilities follow the principle of least privilege — each
/// conversation only gets access to what it explicitly needs.
/// This prevents the session isolation failures from the original
/// OpenClaw where data leaked across user sessions.
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
    /// Can access a specific messaging channel by name.
    Channel(String),
    /// Can use a specific tool by name.
    Tool(String),
    /// Can read/write secrets.
    SecretAccess,
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
    pub fn can_use_tool(&self, tool_name: &str) -> bool {
        self.capabilities.contains(&Capability::Tool(tool_name.to_string()))
    }

    /// Create a capability set that grants access to a specific set of tools
    /// plus standard resource capabilities (file, shell, http).
    pub fn for_tools(tool_names: &[&str]) -> Self {
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
