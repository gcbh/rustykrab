use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use rustykrab_core::types::ToolSchema;
use rustykrab_core::{Error, Result, SandboxRequirements, Tool};
use rustykrab_skills::SkillRegistry;
use serde_json::{json, Value};

/// A tool that deletes a SKILL.md skill from disk and the live registry.
///
/// Mirrors `skill_create`: the removal takes effect immediately (next agent
/// turn), no restart required.
pub struct SkillDeleteTool {
    skills_dir: PathBuf,
    registry: Option<Arc<SkillRegistry>>,
}

impl SkillDeleteTool {
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
/// Same rule as `skill_create` — blocks path traversal.
fn is_valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

#[async_trait]
impl Tool for SkillDeleteTool {
    fn name(&self) -> &str {
        "skill_delete"
    }

    fn description(&self) -> &str {
        "Delete a SKILL.md skill from disk and unregister it from the live registry. \
         The removal takes effect immediately (next agent turn). \
         Use the skill's name (the same name given to skill_create)."
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
                        "description": "Skill name to delete."
                    }
                },
                "required": ["name"]
            }),
        }
    }

    async fn execute(&self, args: Value) -> Result<Value> {
        let name = args["name"]
            .as_str()
            .ok_or_else(|| Error::ToolExecution("missing 'name'".into()))?;

        if !is_valid_name(name) {
            return Err(Error::ToolExecution(
                "invalid skill name: must be 1-64 chars, lowercase a-z, 0-9, hyphens, underscores only".into(),
            ));
        }

        let skill_dir = self.skills_dir.join(name);

        // Unregister from the live registry first so the agent stops seeing
        // the skill even if the on-disk remove races or partially fails.
        let unregistered = self
            .registry
            .as_ref()
            .map(|r| r.unregister(name))
            .unwrap_or(false);

        let existed_on_disk = skill_dir.is_dir();
        if existed_on_disk {
            tokio::fs::remove_dir_all(&skill_dir).await.map_err(|e| {
                Error::ToolExecution(format!("failed to remove skill directory: {e}").into())
            })?;
        }

        if !existed_on_disk && !unregistered {
            return Err(Error::ToolExecution(
                format!("skill '{name}' not found on disk or in registry").into(),
            ));
        }

        Ok(json!({
            "deleted": true,
            "name": name,
            "removed_from_disk": existed_on_disk,
            "unregistered": unregistered,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn rejects_invalid_names() {
        let tmp = TempDir::new().unwrap();
        let tool = SkillDeleteTool::new(tmp.path().to_path_buf());
        for bad in &["../escape", "UPPER", "has space"] {
            let r = tool.execute(json!({ "name": bad })).await;
            assert!(r.is_err(), "should reject: {bad}");
        }
    }

    #[tokio::test]
    async fn errors_when_skill_missing() {
        let tmp = TempDir::new().unwrap();
        let tool = SkillDeleteTool::new(tmp.path().to_path_buf());
        let r = tool.execute(json!({ "name": "does-not-exist" })).await;
        assert!(r.is_err());
    }

    #[tokio::test]
    async fn removes_from_disk_and_registry() {
        let tmp = TempDir::new().unwrap();
        let registry = Arc::new(SkillRegistry::new());

        // First create a skill via the create tool so it's both on disk and in the registry.
        let create =
            crate::SkillCreateTool::with_registry(tmp.path().to_path_buf(), registry.clone());
        create
            .execute(json!({
                "name": "to-remove",
                "description": "temp",
                "instructions": "body"
            }))
            .await
            .unwrap();
        assert!(registry.get_md("to-remove").is_some());
        assert!(tmp.path().join("to-remove/SKILL.md").exists());

        let delete = SkillDeleteTool::with_registry(tmp.path().to_path_buf(), registry.clone());
        let result = delete
            .execute(json!({ "name": "to-remove" }))
            .await
            .unwrap();

        assert_eq!(result["deleted"], true);
        assert_eq!(result["removed_from_disk"], true);
        assert_eq!(result["unregistered"], true);
        assert!(registry.get_md("to-remove").is_none());
        assert!(!tmp.path().join("to-remove").exists());
    }
}
