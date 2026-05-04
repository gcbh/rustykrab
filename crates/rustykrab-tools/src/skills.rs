use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use rustykrab_core::types::ToolSchema;
use rustykrab_core::{Error, Result, SandboxRequirements, Tool};
use rustykrab_skills::SkillRegistry;
use serde_json::{json, Value};

/// A tool that creates, deletes, and loads SKILL.md skills.
///
/// Skills are written to `$DATA_DIR/skills/<name>/SKILL.md`. When a live
/// `SkillRegistry` handle is supplied, the change is hot-loaded — available
/// on the next agent turn with no restart required.
///
/// `load` is the activation path: the model calls it with a skill name from
/// the `<available_skills>` catalog and receives the full SKILL.md body in
/// the tool result, which it then follows for the remainder of the turn.
pub struct SkillsTool {
    skills_dir: PathBuf,
    registry: Option<Arc<SkillRegistry>>,
}

impl SkillsTool {
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

    async fn action_create(&self, args: &Value) -> Result<Value> {
        let name = args["name"]
            .as_str()
            .ok_or_else(|| Error::ToolExecution("missing 'name'".into()))?;
        let description = args["description"]
            .as_str()
            .ok_or_else(|| Error::ToolExecution("missing 'description'".into()))?;
        let instructions = args["instructions"]
            .as_str()
            .ok_or_else(|| Error::ToolExecution("missing 'instructions'".into()))?;

        if !is_valid_name(name) {
            return Err(Error::ToolExecution(
                "invalid skill name: must be 1-64 chars, lowercase a-z, 0-9, hyphens, underscores only".into(),
            ));
        }

        let skill_dir = self.skills_dir.join(name);
        if skill_dir.exists() {
            return Err(Error::ToolExecution(
                format!("skill '{name}' already exists at {}", skill_dir.display()).into(),
            ));
        }

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

        tokio::fs::create_dir_all(&skill_dir).await.map_err(|e| {
            Error::ToolExecution(format!("failed to create skill directory: {e}").into())
        })?;

        let path = skill_dir.join("SKILL.md");
        tokio::fs::write(&path, &content)
            .await
            .map_err(|e| Error::ToolExecution(format!("failed to write SKILL.md: {e}").into()))?;

        // Hot-register in the live registry if one was supplied. Parse the
        // file we just wrote so requirement validation matches the startup
        // loader path exactly.
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
            "action": "create",
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

    async fn action_load(&self, args: &Value) -> Result<Value> {
        let name = args["name"]
            .as_str()
            .ok_or_else(|| Error::ToolExecution("missing 'name'".into()))?;

        if !is_valid_name(name) {
            return Err(Error::ToolExecution(
                "invalid skill name: must be 1-64 chars, lowercase a-z, 0-9, hyphens, underscores only".into(),
            ));
        }

        // Prefer the live registry — it reflects hot-loaded skills and skips
        // a disk read. Fall back to disk so `load` works even when no registry
        // handle was wired (e.g. tests, ad-hoc invocations).
        if let Some(ref registry) = self.registry {
            if let Some(skill) = registry.get_md(name) {
                return Ok(load_response(name, &skill));
            }
        }

        let skill_dir = self.skills_dir.join(name);
        let skill_md_path = skill_dir.join("SKILL.md");
        if !skill_md_path.is_file() {
            return Err(Error::ToolExecution(
                format!("skill '{name}' not found").into(),
            ));
        }

        let skill_dir_owned = skill_dir.clone();
        let skill_md_path_owned = skill_md_path.clone();
        let loaded = tokio::task::spawn_blocking(move || {
            rustykrab_skills::load_single_skill(&skill_dir_owned, &skill_md_path_owned)
        })
        .await
        .map_err(|e| Error::ToolExecution(format!("load task join failed: {e}").into()))?
        .map_err(|e| Error::ToolExecution(format!("failed to load skill: {e}").into()))?;

        Ok(load_response(name, &loaded))
    }

    async fn action_delete(&self, args: &Value) -> Result<Value> {
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
            "action": "delete",
            "deleted": true,
            "name": name,
            "removed_from_disk": existed_on_disk,
            "unregistered": unregistered,
        }))
    }
}

/// Build the JSON tool result returned by `action_load`. The body is the
/// SKILL.md instructions the model must follow next.
fn load_response(name: &str, skill: &rustykrab_skills::SkillMd) -> Value {
    let satisfied = skill.validation.is_satisfied();
    json!({
        "action": "load",
        "name": name,
        "description": skill.frontmatter.description,
        "version": skill.frontmatter.version,
        "body": skill.raw_body,
        "requirements_satisfied": satisfied,
        "missing_env": skill.validation.missing_env,
        "missing_bins": skill.validation.missing_bins,
        "instruction": "Follow the instructions in `body` for the rest of this turn.",
    })
}

/// Adapter that exposes a single SKILL.md skill as a first-class `Tool`.
///
/// The point of this adapter is discoverability. A meta-tool like `SkillsTool`
/// requires the model to (1) read the `<available_skills>` catalog in the
/// system prompt, (2) realize a skill matches the task, (3) call `skills`
/// with `action="load"` and the right `name`. Small local models are bad at
/// step 3 — they're trained much harder on "call a tool" than on "consult a
/// catalog and call a meta-tool with the right enum + arg." Promoting each
/// skill to its own native tool replaces three inferences with one and rides
/// the strong tool-use prior the model already has.
///
/// Execution semantics: calling the tool returns the SKILL.md body verbatim
/// as the tool result (a JSON string). The model reads it as instructions and
/// executes the recipe over subsequent tool calls in the same turn — same as
/// today's `SkillsTool::action_load`, just delivered through a tool the model
/// is far more likely to invoke unprompted.
pub struct SkillTool {
    skill: Arc<rustykrab_skills::SkillMd>,
}

impl SkillTool {
    pub fn new(skill: Arc<rustykrab_skills::SkillMd>) -> Self {
        Self { skill }
    }

    /// The skill name, used as the tool name. Constrained to `[a-z0-9_-]` by
    /// the SKILL.md loader, so it's already a safe tool identifier.
    fn skill_name(&self) -> &str {
        &self.skill.frontmatter.name
    }
}

#[async_trait]
impl Tool for SkillTool {
    fn name(&self) -> &str {
        self.skill_name()
    }

    fn description(&self) -> &str {
        // Use the SKILL.md description verbatim. We deliberately don't prepend
        // a "[Skill]" tag or boilerplate about "calling this returns a recipe":
        // the tool-result-is-instructions pattern is something models already
        // handle natively (web_search, web_fetch, etc. all return text the
        // model then acts on), and extra framing would just dilute the
        // signal that picks this tool over neighbours in the schema.
        &self.skill.frontmatter.description
    }

    fn schema(&self) -> ToolSchema {
        // No parameters: the tool name *is* the routing. Activating the skill
        // takes no args; the body is the same regardless of caller intent.
        ToolSchema {
            name: self.name().to_string(),
            description: self.description().to_string(),
            parameters: json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        }
    }

    fn available(&self) -> bool {
        // Hide skills whose declared requirements (env vars, binaries) aren't
        // satisfied — calling one would just produce a recipe the agent
        // can't execute anyway.
        self.skill.validation.is_satisfied()
    }

    async fn execute(&self, _args: Value) -> Result<Value> {
        // Return the body as a bare JSON string, not a structured object.
        // Models read tool results as the next round of "input" and act on
        // text directly; wrapping it in `{ "body": ... }` would force the
        // model to do an extra parse step that small models routinely skip.
        Ok(Value::String(self.skill.raw_body.clone()))
    }
}

/// Validate that a skill name contains only `[a-z0-9_-]` and is 1–64 chars.
/// Strict allowlist blocks path traversal.
fn is_valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

#[async_trait]
impl Tool for SkillsTool {
    fn name(&self) -> &str {
        "skills"
    }

    fn description(&self) -> &str {
        "Create, delete, or load SKILL.md skills. To USE a skill listed in \
         <available_skills>, call this tool with action='load' and the skill \
         name — the tool result contains the skill body, which you must then \
         follow. Other actions: 'create' (name, description, instructions), \
         'delete' (name). Created/deleted skills are hot-loaded into the \
         registry on the next agent turn."
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
                    "action": {
                        "type": "string",
                        "enum": ["load", "create", "delete"],
                        "description": "Action to perform. Use 'load' to activate a skill from <available_skills> and receive its body."
                    },
                    "name": {
                        "type": "string",
                        "description": "Skill name: lowercase alphanumeric, hyphens, underscores. Max 64 chars."
                    },
                    "description": {
                        "type": "string",
                        "description": "Short human-readable description of what the skill does (create only)."
                    },
                    "instructions": {
                        "type": "string",
                        "description": "Markdown body — the system prompt instructions for the skill (create only)."
                    },
                    "version": {
                        "type": "string",
                        "description": "Skill version string (create only, default: \"1.0\")."
                    },
                    "user_invocable": {
                        "type": "boolean",
                        "description": "Whether users can invoke this skill directly (create only, default: true)."
                    },
                    "emoji": {
                        "type": "string",
                        "description": "Optional emoji for display (create only)."
                    },
                    "requires_env": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Environment variables the skill requires (create only)."
                    },
                    "requires_bins": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Binaries the skill requires on PATH (create only)."
                    }
                },
                "required": ["action", "name"]
            }),
        }
    }

    async fn execute(&self, args: Value) -> Result<Value> {
        let action = args["action"]
            .as_str()
            .ok_or_else(|| Error::ToolExecution("missing 'action'".into()))?;

        match action {
            "load" => self.action_load(&args).await,
            "create" => self.action_create(&args).await,
            "delete" => self.action_delete(&args).await,
            other => Err(Error::ToolExecution(
                format!("unknown action '{other}', expected one of: load, create, delete").into(),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_tool() -> (SkillsTool, TempDir) {
        let tmp = TempDir::new().unwrap();
        let tool = SkillsTool::new(tmp.path().to_path_buf());
        (tool, tmp)
    }

    #[tokio::test]
    async fn create_writes_skill_on_disk() {
        let (tool, tmp) = make_tool();
        let result = tool
            .execute(json!({
                "action": "create",
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
    async fn create_rejects_path_traversal_names() {
        let (tool, _tmp) = make_tool();
        let result = tool
            .execute(json!({
                "action": "create",
                "name": "../escape",
                "description": "bad",
                "instructions": "nope"
            }))
            .await;

        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("invalid skill name"));
    }

    #[tokio::test]
    async fn create_rejects_existing_skill() {
        let (tool, tmp) = make_tool();
        std::fs::create_dir_all(tmp.path().join("existing")).unwrap();

        let result = tool
            .execute(json!({
                "action": "create",
                "name": "existing",
                "description": "dup",
                "instructions": "dup"
            }))
            .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("already exists"));
    }

    #[tokio::test]
    async fn create_rejects_invalid_characters() {
        let (tool, _tmp) = make_tool();
        for bad_name in &["Has.Dot", "slash/bad", "UPPER", "has space"] {
            let result = tool
                .execute(json!({
                    "action": "create",
                    "name": bad_name,
                    "description": "bad",
                    "instructions": "nope"
                }))
                .await;
            assert!(result.is_err(), "should reject name: {bad_name}");
        }
    }

    #[tokio::test]
    async fn create_minimal_skill_with_defaults() {
        let (tool, tmp) = make_tool();
        tool.execute(json!({
            "action": "create",
            "name": "minimal",
            "description": "Bare minimum",
            "instructions": "Just do it."
        }))
        .await
        .unwrap();

        let content = std::fs::read_to_string(tmp.path().join("minimal/SKILL.md")).unwrap();
        assert!(content.contains("version = \"1.0\""));
        assert!(content.contains("user_invocable = true"));
        assert!(!content.contains("[requires]"));
    }

    #[tokio::test]
    async fn create_hot_reloads_into_registry() {
        let tmp = TempDir::new().unwrap();
        let registry = Arc::new(SkillRegistry::new());
        let tool = SkillsTool::with_registry(tmp.path().to_path_buf(), registry.clone());

        let result = tool
            .execute(json!({
                "action": "create",
                "name": "live-skill",
                "description": "Hot reloaded",
                "instructions": "Instructions body."
            }))
            .await
            .unwrap();

        assert_eq!(result["hot_reloaded"], true);
        assert!(registry.get_md("live-skill").is_some());
    }

    #[tokio::test]
    async fn delete_rejects_invalid_names() {
        let (tool, _tmp) = make_tool();
        for bad in &["../escape", "UPPER", "has space"] {
            let r = tool
                .execute(json!({ "action": "delete", "name": bad }))
                .await;
            assert!(r.is_err(), "should reject: {bad}");
        }
    }

    #[tokio::test]
    async fn delete_errors_when_skill_missing() {
        let (tool, _tmp) = make_tool();
        let r = tool
            .execute(json!({ "action": "delete", "name": "does-not-exist" }))
            .await;
        assert!(r.is_err());
    }

    #[tokio::test]
    async fn delete_removes_from_disk_and_registry() {
        let tmp = TempDir::new().unwrap();
        let registry = Arc::new(SkillRegistry::new());
        let tool = SkillsTool::with_registry(tmp.path().to_path_buf(), registry.clone());

        tool.execute(json!({
            "action": "create",
            "name": "to-remove",
            "description": "temp",
            "instructions": "body"
        }))
        .await
        .unwrap();
        assert!(registry.get_md("to-remove").is_some());
        assert!(tmp.path().join("to-remove/SKILL.md").exists());

        let result = tool
            .execute(json!({ "action": "delete", "name": "to-remove" }))
            .await
            .unwrap();

        assert_eq!(result["deleted"], true);
        assert_eq!(result["removed_from_disk"], true);
        assert_eq!(result["unregistered"], true);
        assert!(registry.get_md("to-remove").is_none());
        assert!(!tmp.path().join("to-remove").exists());
    }

    #[tokio::test]
    async fn load_returns_body_from_disk() {
        let (tool, tmp) = make_tool();
        std::fs::create_dir_all(tmp.path().join("on-disk")).unwrap();
        std::fs::write(
            tmp.path().join("on-disk/SKILL.md"),
            "---\nname = \"on-disk\"\ndescription = \"Disk skill\"\n---\nDo the disk thing.\n",
        )
        .unwrap();

        let result = tool
            .execute(json!({ "action": "load", "name": "on-disk" }))
            .await
            .unwrap();

        assert_eq!(result["action"], "load");
        assert_eq!(result["name"], "on-disk");
        assert_eq!(result["description"], "Disk skill");
        assert!(result["body"]
            .as_str()
            .unwrap()
            .contains("Do the disk thing."));
        assert_eq!(result["requirements_satisfied"], true);
    }

    #[tokio::test]
    async fn load_prefers_registry_over_disk() {
        let tmp = TempDir::new().unwrap();
        let registry = Arc::new(SkillRegistry::new());
        let tool = SkillsTool::with_registry(tmp.path().to_path_buf(), registry.clone());

        // Create via the tool so the registry is populated.
        tool.execute(json!({
            "action": "create",
            "name": "live-load",
            "description": "Live skill",
            "instructions": "Live body content."
        }))
        .await
        .unwrap();

        // Mutate the on-disk file so we can prove the registry is what's read.
        std::fs::write(
            tmp.path().join("live-load/SKILL.md"),
            "---\nname = \"live-load\"\ndescription = \"Stale\"\n---\nStale disk body.\n",
        )
        .unwrap();

        let result = tool
            .execute(json!({ "action": "load", "name": "live-load" }))
            .await
            .unwrap();

        assert_eq!(result["description"], "Live skill");
        assert!(result["body"]
            .as_str()
            .unwrap()
            .contains("Live body content."));
    }

    #[tokio::test]
    async fn load_errors_when_skill_missing() {
        let (tool, _tmp) = make_tool();
        let r = tool
            .execute(json!({ "action": "load", "name": "ghost" }))
            .await;
        assert!(r.is_err());
        assert!(r.unwrap_err().to_string().contains("not found"));
    }

    #[tokio::test]
    async fn load_rejects_invalid_name() {
        let (tool, _tmp) = make_tool();
        let r = tool
            .execute(json!({ "action": "load", "name": "../etc/passwd" }))
            .await;
        assert!(r.is_err());
        assert!(r.unwrap_err().to_string().contains("invalid skill name"));
    }

    #[tokio::test]
    async fn rejects_unknown_action() {
        let (tool, _tmp) = make_tool();
        let r = tool
            .execute(json!({ "action": "frobnicate", "name": "x" }))
            .await;
        assert!(r.is_err());
        assert!(r.unwrap_err().to_string().contains("unknown action"));
    }

    fn make_skill_md(name: &str, description: &str, body: &str) -> Arc<rustykrab_skills::SkillMd> {
        use rustykrab_skills::skill_md::{
            RequirementValidation, SkillMd, SkillMdFrontmatter, SkillRequirements,
        };
        use std::collections::HashMap;
        use std::path::PathBuf;

        Arc::new(SkillMd {
            path: PathBuf::from(format!("/skills/{name}")),
            frontmatter: SkillMdFrontmatter {
                name: name.to_string(),
                description: description.to_string(),
                version: "1.0".to_string(),
                requires: SkillRequirements::default(),
                user_invocable: true,
                emoji: None,
                extra: HashMap::new(),
            },
            raw_body: body.to_string(),
            validation: RequirementValidation {
                missing_env: Vec::new(),
                missing_bins: Vec::new(),
            },
        })
    }

    #[tokio::test]
    async fn skill_tool_uses_skill_name_and_description() {
        let tool = SkillTool::new(make_skill_md(
            "daily_briefing",
            "Generate the daily briefing.",
            "Step 1: do the thing.",
        ));

        // The tool's identity is the skill's identity — that's the whole
        // point. If these ever drift, the model's schema view of the skill
        // and the registry's view diverge.
        assert_eq!(tool.name(), "daily_briefing");
        assert_eq!(tool.description(), "Generate the daily briefing.");
    }

    #[tokio::test]
    async fn skill_tool_execute_returns_body_as_bare_string() {
        let body = "# Daily briefing\n\nStep 1: gmail.\nStep 2: weather.";
        let tool = SkillTool::new(make_skill_md("daily_briefing", "desc", body));

        let result = tool.execute(json!({})).await.unwrap();

        // Bare string, not a JSON object: models receive tool results as
        // text and act on them directly. Wrapping in { "body": ... } would
        // force an extra parse step.
        match result {
            Value::String(s) => assert_eq!(s, body),
            other => panic!("expected JSON string, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn skill_tool_execute_ignores_args() {
        // The tool name is the routing — args don't change behavior.
        // Defending against the model passing junk args.
        let tool = SkillTool::new(make_skill_md("s", "d", "BODY"));
        let r1 = tool.execute(json!({})).await.unwrap();
        let r2 = tool.execute(json!({ "junk": "ignored" })).await.unwrap();
        let r3 = tool.execute(Value::Null).await.unwrap();

        assert_eq!(r1, r2);
        assert_eq!(r2, r3);
        assert_eq!(r1, Value::String("BODY".to_string()));
    }

    #[tokio::test]
    async fn skill_tool_schema_has_empty_params() {
        // Empty object schema is what we want: the model sees a tool that
        // takes no args, and the schema rejects accidental extras.
        let tool = SkillTool::new(make_skill_md("s", "d", "b"));
        let schema = tool.schema();

        assert_eq!(schema.name, "s");
        assert_eq!(schema.description, "d");
        assert_eq!(schema.parameters["type"], "object");
        assert_eq!(schema.parameters["additionalProperties"], false);
        assert!(
            schema.parameters["properties"]
                .as_object()
                .unwrap()
                .is_empty(),
            "skill-tool schema must take no parameters"
        );
    }

    #[tokio::test]
    async fn skill_tool_unavailable_when_requirements_unsatisfied() {
        use rustykrab_skills::skill_md::{
            RequirementValidation, SkillMd, SkillMdFrontmatter, SkillRequirements,
        };
        use std::collections::HashMap;
        use std::path::PathBuf;

        // A skill whose required env var is missing should hide from the
        // tool list — exposing it would just produce a recipe the agent
        // can't actually execute.
        let unsatisfied = Arc::new(SkillMd {
            path: PathBuf::from("/skills/needs_env"),
            frontmatter: SkillMdFrontmatter {
                name: "needs_env".to_string(),
                description: "Needs SOMETHING".to_string(),
                version: "1.0".to_string(),
                requires: SkillRequirements::default(),
                user_invocable: true,
                emoji: None,
                extra: HashMap::new(),
            },
            raw_body: "body".to_string(),
            validation: RequirementValidation {
                missing_env: vec!["SOMETHING".to_string()],
                missing_bins: Vec::new(),
            },
        });

        let tool = SkillTool::new(unsatisfied);
        assert!(!tool.available());
    }
}
