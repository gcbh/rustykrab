//! File-based agent definitions: `<data dir>/agents/<name>.md`.
//!
//! The same layout as a `SKILL.md`: a `---`-fenced TOML front-matter block,
//! then a markdown body, which is the agent's system prompt. The file stem
//! is the definition's id.
//!
//! ```text
//! ---
//! description = "Reads, edits, and runs code to implement a change."
//! profile = "coding"                     # default | coding | research | creative
//! tools = ["read", "write", "exec"]      # visible from turn 0
//! mcp_servers = ["linear"]               # their tools visible from turn 0
//! allowed_tools = ["read", "write"]      # optional ceiling; absent = inherit
//! model = "qwen3.8:27b"                  # optional preference
//! writable_resources = ["repo"]          # optional
//! ---
//! You are a focused coding sub-agent. ...
//! ```
//!
//! The built-in definitions (`worker`, `coder`, `researcher`, `planner`)
//! are files of this format embedded in the binary; a data-dir file with the
//! same stem replaces one. A file that does not parse is logged and skipped,
//! and the built-in of its name, if any, stays: a typo in a user file must
//! not take away the worker the controller runs on. Unknown front-matter
//! keys are errors rather than silently ignored, for the same reason.

use std::path::Path;

use rustykrab_core::{AgentDefinition, AgentRegistry};
use serde::Deserialize;

use crate::skill_md::split_frontmatter;

/// The directory under the data dir that holds definition files.
pub const AGENTS_DIR: &str = "agents";

/// The embedded defaults, by id.
const BUILTIN: &[(&str, &str)] = &[
    ("coder", include_str!("../agents/coder.md")),
    ("planner", include_str!("../agents/planner.md")),
    ("researcher", include_str!("../agents/researcher.md")),
    ("work-planner", include_str!("../agents/work-planner.md")),
    ("worker", include_str!("../agents/worker.md")),
];

/// Harness profiles a definition may name (see `rustykrab-agent`'s
/// `HarnessProfile` presets).
const PROFILES: &[&str] = &["default", "coding", "research", "creative"];

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Frontmatter {
    /// Optional; must equal the file stem when present.
    #[serde(default)]
    name: Option<String>,
    description: String,
    #[serde(default = "default_profile")]
    profile: String,
    #[serde(default)]
    planning_only: bool,
    #[serde(default)]
    tools: Vec<String>,
    #[serde(default)]
    mcp_servers: Vec<String>,
    #[serde(default)]
    allowed_tools: Option<Vec<String>>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    writable_resources: Vec<String>,
}

fn default_profile() -> String {
    "default".into()
}

/// Whether `id` can name a definition: lowercase ASCII letters, digits,
/// `-` and `_`.
fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

/// Parse one definition file's contents into the definition named `id`.
pub fn parse_agent_md(id: &str, content: &str) -> Result<AgentDefinition, String> {
    if !valid_id(id) {
        return Err(format!(
            "agent name `{id}` must be lowercase letters, digits, `-` or `_`"
        ));
    }
    let (toml_str, body) = split_frontmatter(content, "an agent definition")?;
    let front: Frontmatter =
        toml::from_str(toml_str).map_err(|e| format!("invalid agent front-matter: {e}"))?;
    if let Some(name) = front.name.as_deref().filter(|n| *n != id) {
        return Err(format!(
            "front-matter name `{name}` does not match the file name `{id}`"
        ));
    }
    if !PROFILES.contains(&front.profile.as_str()) {
        return Err(format!(
            "unknown profile `{}`; expected one of {}",
            front.profile,
            PROFILES.join(", ")
        ));
    }
    let system_prompt = body.trim().to_string();
    if system_prompt.is_empty() {
        return Err("the body (the system prompt) is empty".into());
    }
    Ok(AgentDefinition {
        id: id.to_string(),
        description: front.description,
        system_prompt,
        profile: front.profile,
        planning_only: front.planning_only,
        allowed_tools: front.allowed_tools,
        tools: front.tools,
        mcp_servers: front.mcp_servers,
        model: front.model,
        writable_resources: front.writable_resources,
    })
}

/// The embedded default definitions, sorted by id.
pub fn builtin_definitions() -> Vec<AgentDefinition> {
    BUILTIN
        .iter()
        .map(|(id, content)| {
            // The embedded files are part of the build; a unit test parses
            // every one, so this cannot fail in a binary that passed it.
            parse_agent_md(id, content)
                .unwrap_or_else(|e| panic!("built-in agent definition `{id}` is invalid: {e}"))
        })
        .collect()
}

/// One embedded default definition.
pub fn builtin(id: &str) -> Option<AgentDefinition> {
    builtin_definitions().into_iter().find(|d| d.id == id)
}

/// Every `*.md` definition directly in `dir`, sorted by id. A missing
/// directory is no definitions; a file that does not parse is logged and
/// skipped.
pub fn load_agent_dir(dir: &Path) -> Vec<AgentDefinition> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        tracing::debug!(path = %dir.display(), "no agent definitions directory");
        return Vec::new();
    };
    let mut defs = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") || !path.is_file() {
            continue;
        }
        let Some(id) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let parsed = std::fs::read_to_string(&path)
            .map_err(|e| e.to_string())
            .and_then(|content| parse_agent_md(id, &content));
        match parsed {
            Ok(def) => defs.push(def),
            Err(e) => tracing::warn!(
                path = %path.display(),
                error = %e,
                "agent definition skipped"
            ),
        }
    }
    defs.sort_by(|a, b| a.id.cmp(&b.id));
    defs
}

/// The registry a daemon runs with: the built-ins, then every definition
/// in `dir` (normally `<data dir>/agents`), which replaces a built-in of
/// the same id.
pub fn agent_registry(dir: Option<&Path>) -> AgentRegistry {
    let mut registry = AgentRegistry::new();
    for def in builtin_definitions() {
        registry.insert(def);
    }
    if let Some(dir) = dir {
        for def in load_agent_dir(dir) {
            tracing::info!(
                agent = %def.id,
                path = %dir.display(),
                visible = ?def.tools,
                "agent definition loaded from file"
            );
            registry.insert(def);
        }
    }
    registry
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_builtin_parses_and_the_coder_starts_with_its_workbench() {
        let ids: Vec<String> = builtin_definitions().into_iter().map(|d| d.id).collect();
        assert_eq!(
            ids,
            ["coder", "planner", "researcher", "work-planner", "worker"]
        );

        let coder = builtin("coder").unwrap();
        assert_eq!(coder.profile, "coding");
        // Phase 2's exit: filesystem and runtime tools visible from turn 0.
        for tool in [
            "read",
            "write",
            "edit",
            "apply_patch",
            "exec",
            "process",
            "code_execution",
        ] {
            assert!(coder.tools.iter().any(|t| t == tool), "{tool}");
        }
        assert!(coder.allowed_tools.is_none(), "the built-ins inherit");

        let worker = builtin("worker").unwrap();
        assert!(worker
            .system_prompt
            .starts_with("You are {name}, a RustyKrab worker."));
        assert!(worker.tools.is_empty());
        assert!(builtin("nobody").is_none());
    }

    #[test]
    fn a_file_names_every_field() {
        let def = parse_agent_md(
            "triage",
            "---\n\
             description = \"Sorts the inbox.\"\n\
             profile = \"research\"\n\
             tools = [\"gmail\"]\n\
             mcp_servers = [\"linear\"]\n\
             allowed_tools = [\"gmail\", \"memory_search\"]\n\
             model = \"gemma4:26b\"\n\
             writable_resources = [\"inbox\"]\n\
             ---\n\
             \n  You sort mail.\n\n",
        )
        .unwrap();
        assert_eq!(def.id, "triage");
        assert_eq!(def.description, "Sorts the inbox.");
        assert_eq!(def.system_prompt, "You sort mail.");
        assert_eq!(def.profile, "research");
        assert_eq!(def.tools, ["gmail"]);
        assert_eq!(def.mcp_servers, ["linear"]);
        assert_eq!(
            def.allowed_tools.as_deref(),
            Some(&["gmail".to_string(), "memory_search".to_string()][..])
        );
        assert_eq!(def.model.as_deref(), Some("gemma4:26b"));
        assert_eq!(def.writable_resources, ["inbox"]);

        let minimal = parse_agent_md("m", "---\ndescription = \"d\"\n---\nPrompt.").unwrap();
        assert_eq!(minimal.profile, "default");
        assert!(minimal.tools.is_empty() && minimal.allowed_tools.is_none());
    }

    #[test]
    fn a_bad_file_says_what_is_wrong() {
        let cases = [
            ("Bad", "---\ndescription = \"d\"\n---\nP", "lowercase"),
            ("x", "description = \"d\"\nP", "must begin with `---`"),
            (
                "x",
                "---\ndescription = \"d\"\ntool = []\n---\nP",
                "unknown field",
            ),
            (
                "x",
                "---\ndescription = \"d\"\nprofile = \"fast\"\n---\nP",
                "unknown profile",
            ),
            (
                "x",
                "---\nname = \"y\"\ndescription = \"d\"\n---\nP",
                "does not match",
            ),
            ("x", "---\ndescription = \"d\"\n---\n  \n", "empty"),
            ("x", "---\nprofile = \"coding\"\n---\nP", "description"),
        ];
        for (id, content, needle) in cases {
            let err = parse_agent_md(id, content).unwrap_err();
            assert!(err.contains(needle), "{id}: {err}");
        }
    }

    #[test]
    fn a_data_dir_file_overrides_a_builtin_and_a_broken_one_does_not() {
        let dir = std::env::temp_dir().join(format!("rk-agents-{}", uuid_like()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("coder.md"),
            "---\ndescription = \"Mine.\"\nprofile = \"coding\"\ntools = [\"read\"]\n---\nMy coder.",
        )
        .unwrap();
        std::fs::write(dir.join("worker.md"), "---\nprofile = 1\n---\nBroken.").unwrap();
        std::fs::write(
            dir.join("scout.md"),
            "---\ndescription = \"New.\"\n---\nScout.",
        )
        .unwrap();
        std::fs::write(dir.join("notes.txt"), "not a definition").unwrap();

        let loaded: Vec<String> = load_agent_dir(&dir).into_iter().map(|d| d.id).collect();
        assert_eq!(loaded, ["coder", "scout"]);

        let registry = agent_registry(Some(&dir));
        assert_eq!(registry.get("coder").unwrap().system_prompt, "My coder.");
        assert_eq!(registry.get("coder").unwrap().tools, ["read"]);
        assert!(registry
            .get("worker")
            .unwrap()
            .system_prompt
            .starts_with("You are {name}"));
        assert!(registry.get("scout").is_some());
        assert_eq!(registry.list().len(), 6);

        assert_eq!(agent_registry(None).list().len(), 5);
        assert!(load_agent_dir(&dir.join("missing")).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A unique enough directory suffix without pulling in `uuid`.
    fn uuid_like() -> String {
        format!(
            "{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        )
    }
}
