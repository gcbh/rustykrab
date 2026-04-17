use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use rustykrab_core::types::ToolSchema;
use rustykrab_core::{Error, Result, SandboxRequirements, Tool};
use rustykrab_skills::SkillRegistry;
use serde_json::{json, Value};

/// A tool that creates new SKILL.md skills on disk.
///
/// The agent can use this to author reusable skills during a conversation.
/// Skills are written to `$DATA_DIR/skills/<name>/SKILL.md` and — when a
/// registry handle is supplied — are also registered live, becoming
/// available on the next agent turn with no restart.
pub struct SkillCreateTool {
    skills_dir: PathBuf,
    registry: Option<Arc<SkillRegistry>>,
}

impl SkillCreateTool {
    pub fn new(skills_dir: PathBuf) -> Self {
        Self {
            skills_dir,
            registry: None,
        }
    }

    pub fn with_registry(skills_dir: PathBuf, registry: Arc<SkillRegistry>) -> Self {
        Self {
            skills_dir,
            registry: Some(registry),
        }
    }
}

/// Validate that a skill name contains only `[a-z0-9_-]` and is 1–64 chars.
fn is_valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

#[async_trait]
impl Tool for SkillCreateTool {
    fn name(&self) -> &str {
        "skill_create"
    }

    fn description(&self) -> &str {
        "Create a new SKILL.md skill on disk. The skill is hot-loaded into the \
         registry immediately (available on the next agent turn, no restart required). \
         Provide a name (lowercase a-z, 0-9, hyphens, underscores), a short description, \
         and the markdown instructions body."
    }

    fn sandbox_requirements(&self) -> SandboxRequirements {
        SandboxRequirements {
            needs_fs_write: true,
            ..SandboxRequirements::default()
        }
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: self.name().to_string(),
            description: self.description().to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "Skill name: lowercase alphanumeric, hyphens, underscores. Max 64 chars."
                    },
                    "description": {
                        "type": "string",
                        "description": "Short human-readable description of what the skill does."
                    },
                    "instructions": {
                        "type": "string",
                        "description": "Markdown body — the system prompt instructions for the skill."
                    },
                    "version": {
                        "type": "string",
                        "description": "Skill version string (default: \"1.0\")."
                    },
                    "user_invocable": {
                        "type": "boolean",
                        "description": "Whether users can invoke this skill directly (default: true)."
                    },
                    "emoji": {
                        "type": "string",
                        "description": "Optional emoji for display."
                    },
                    "requires_env": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Environment variables the skill requires."
                    },
                    "requires_bins": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Binaries the skill requires on PATH."
                    }
                },
                "required": ["name", "description", "instructions"]
            }),
        }
    }

    async fn execute(&self, args: Value) -> Result<Value> {
        let name = args["name"]
            .as_str()
            .ok_or_else(|| Error::ToolExecution("missing 'name'".into()))?;
        let description = args["description"]
            .as_str()
            .ok_or_else(|| Error::ToolExecution("missing 'description'".into()))?;
        let instructions = args["instructions"]
            .as_str()
            .ok_or_else(|| Error::ToolExecution("missing 'instructions'".into()))?;

        // Validate name (strict allowlist blocks path traversal)
        if !is_valid_name(name) {
            return Err(Error::ToolExecution(
                "invalid skill name: must be 1-64 chars, lowercase a-z, 0-9, hyphens, underscores only".into(),
            ));
        }

        // Reject if skill already exists
        let skill_dir = self.skills_dir.join(name);
        if skill_dir.exists() {
            return Err(Error::ToolExecution(
                format!("skill '{name}' already exists at {}", skill_dir.display()).into(),
            ));
        }

        // Optional fields
        let version = args["version"].as_str().unwrap_or("1.0");
        let user_invocable = args["user_invocable"].as_bool().unwrap_or(true);
        let emoji = args["emoji"].as_str();
        let requires_env: Vec<&str> = args["requires_env"]
            .as_array()
            .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
            .unwrap_or_default();
        let requires_bins: Vec<&str> = args["requires_bins"]
            .as_array()
            .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
            .unwrap_or_default();

        // Build TOML frontmatter
        let mut fm = format!(
            "name = \"{}\"\ndescription = \"{}\"\nversion = \"{version}\"\nuser_invocable = {user_invocable}",
            name.replace('"', "\\\""),
            description.replace('"', "\\\""),
        );
        if let Some(e) = emoji {
            fm.push_str(&format!("\nemoji = \"{}\"", e.replace('"', "\\\"")));
        }
        if !requires_env.is_empty() || !requires_bins.is_empty() {
            fm.push_str("\n\n[requires]");
            if !requires_env.is_empty() {
                let env_items: Vec<String> =
                    requires_env.iter().map(|v| format!("\"{v}\"")).collect();
                fm.push_str(&format!("\nenv = [{}]", env_items.join(", ")));
            }
            if !requires_bins.is_empty() {
                let bin_items: Vec<String> =
                    requires_bins.iter().map(|b| format!("\"{b}\"")).collect();
                fm.push_str(&format!("\nbins = [{}]", bin_items.join(", ")));
            }
        }

        let content = format!("---\n{fm}\n---\n{instructions}");

        // Write to disk using tokio::fs to avoid blocking the async
        // runtime (fixes ASYNC-M3).
        tokio::fs::create_dir_all(&skill_dir).await.map_err(|e| {
            Error::ToolExecution(format!("failed to create skill directory: {e}").into())
        })?;

        let path = skill_dir.join("SKILL.md");
        tokio::fs::write(&path, &content)
            .await
            .map_err(|e| Error::ToolExecution(format!("failed to write SKILL.md: {e}").into()))?;

        // Hot-register in the live registry if one was supplied.
        // Parse the file we just wrote so requirement validation (env/bins)
        // is consistent with the startup loader path.
        let mut hot_reloaded = false;
        if let Some(ref registry) = self.registry {
            let skill_dir_owned = skill_dir.clone();
            let path_owned = path.clone();
            let loaded = tokio::task::spawn_blocking(move || {
                rustykrab_skills::load_single_skill(&skill_dir_owned, &path_owned)
            })
            .await
            .map_err(|e| Error::ToolExecution(format!("load task join failed: {e}").into()))?;

            match loaded {
                Ok(skill_md) => {
                    registry.register_md(Arc::new(skill_md));
                    hot_reloaded = true;
                }
                Err(e) => {
                    tracing::warn!(
                        name = name,
                        error = %e,
                        "skill written to disk but hot-reload parse failed"
                    );
                }
            }
        }

        Ok(json!({
            "created": true,
            "name": name,
            "path": path.display().to_string(),
            "hot_reloaded": hot_reloaded,
            "note": if hot_reloaded {
                "Skill is live — available on the next agent turn."
            } else {
                "Skill will be available on next server restart."
            },
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_tool() -> (SkillCreateTool, TempDir) {
        let tmp = TempDir::new().unwrap();
        let tool = SkillCreateTool::new(tmp.path().to_path_buf());
        (tool, tmp)
    }

    #[tokio::test]
    async fn creates_skill_on_disk() {
        let (tool, tmp) = make_tool();
        let result = tool
            .execute(json!({
                "name": "my-skill",
                "description": "A test skill",
                "instructions": "Do the thing.\n\nWith **markdown**."
            }))
            .await
            .unwrap();

        assert_eq!(result["created"], true);
        assert_eq!(result["name"], "my-skill");

        let path = tmp.path().join("my-skill/SKILL.md");
        assert!(path.exists());

        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.starts_with("---\n"));
        assert!(content.contains("name = \"my-skill\""));
        assert!(content.contains("Do the thing."));
    }

    #[tokio::test]
    async fn rejects_path_traversal_names() {
        let (tool, _tmp) = make_tool();
        let result = tool
            .execute(json!({
                "name": "../escape",
                "description": "bad",
                "instructions": "nope"
            }))
            .await;

        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("invalid skill name"));
    }

    #[tokio::test]
    async fn rejects_existing_skill() {
        let (tool, tmp) = make_tool();
        // Pre-create the skill directory
        std::fs::create_dir_all(tmp.path().join("existing")).unwrap();

        let result = tool
            .execute(json!({
                "name": "existing",
                "description": "dup",
                "instructions": "dup"
            }))
            .await;

        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("already exists"));
    }

    #[tokio::test]
    async fn rejects_invalid_characters() {
        let (tool, _tmp) = make_tool();
        for bad_name in &["Has.Dot", "slash/bad", "UPPER", "has space"] {
            let result = tool
                .execute(json!({
                    "name": bad_name,
                    "description": "bad",
                    "instructions": "nope"
                }))
                .await;
            assert!(result.is_err(), "should reject name: {bad_name}");
        }
    }

    #[tokio::test]
    async fn creates_minimal_skill_with_defaults() {
        let (tool, tmp) = make_tool();
        let result = tool
            .execute(json!({
                "name": "minimal",
                "description": "Bare minimum",
                "instructions": "Just do it."
            }))
            .await
            .unwrap();

        assert_eq!(result["created"], true);

        let content = std::fs::read_to_string(tmp.path().join("minimal/SKILL.md")).unwrap();
        assert!(content.contains("version = \"1.0\""));
        assert!(content.contains("user_invocable = true"));
        // Should not contain requires section
        assert!(!content.contains("[requires]"));
    }

    #[tokio::test]
    async fn hot_reload_registers_skill_in_registry() {
        let tmp = TempDir::new().unwrap();
        let registry = Arc::new(SkillRegistry::new());
        let tool = SkillCreateTool::with_registry(tmp.path().to_path_buf(), registry.clone());

        let result = tool
            .execute(json!({
                "name": "live-skill",
                "description": "Hot reloaded",
                "instructions": "Instructions body."
            }))
            .await
            .unwrap();

        assert_eq!(result["hot_reloaded"], true);
        assert!(registry.get_md("live-skill").is_some());
    }
}
