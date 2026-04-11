use async_trait::async_trait;
use rustykrab_core::types::ToolSchema;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

use crate::Skill;

/// TOML frontmatter from a SKILL.md file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillMdFrontmatter {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub requires: SkillRequirements,
    #[serde(default)]
    pub user_invocable: bool,
    #[serde(default)]
    pub emoji: Option<String>,
    /// Forward-compatible catch-all for unknown fields.
    #[serde(flatten)]
    pub extra: HashMap<String, toml::Value>,
}

/// Environment and binary requirements for a skill.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SkillRequirements {
    #[serde(default)]
    pub env: Vec<String>,
    #[serde(default)]
    pub bins: Vec<String>,
}

/// Result of checking whether a skill's requirements are met.
#[derive(Debug, Clone)]
pub struct RequirementValidation {
    pub missing_env: Vec<String>,
    pub missing_bins: Vec<String>,
}

impl RequirementValidation {
    pub fn is_satisfied(&self) -> bool {
        self.missing_env.is_empty() && self.missing_bins.is_empty()
    }
}

/// A skill loaded from a SKILL.md file on disk.
#[derive(Debug, Clone)]
pub struct SkillMd {
    /// Directory containing the SKILL.md file.
    pub path: PathBuf,
    pub frontmatter: SkillMdFrontmatter,
    /// Raw markdown body (everything after the second `---`).
    pub raw_body: String,
    pub validation: RequirementValidation,
}

/// Parse a SKILL.md string into its frontmatter and body.
///
/// Expected format:
/// ```text
/// ---
/// name = "my-skill"
/// description = "Does something useful"
/// ---
/// Markdown instructions here...
/// ```
pub fn parse_skill_md(content: &str) -> Result<(SkillMdFrontmatter, String), String> {
    let trimmed = content.trim_start();
    if !trimmed.starts_with("---") {
        return Err("SKILL.md must begin with `---` frontmatter delimiter".into());
    }

    // Skip the opening `---` line.
    let after_open = &trimmed[3..];
    let after_open = after_open.strip_prefix('\n').unwrap_or(after_open);

    let close_pos = after_open
        .find("\n---")
        .ok_or("missing closing `---` frontmatter delimiter")?;

    let toml_str = &after_open[..close_pos];
    let body_start = close_pos + 4; // skip "\n---"
    let body = if body_start < after_open.len() {
        after_open[body_start..]
            .strip_prefix('\n')
            .unwrap_or(&after_open[body_start..])
    } else {
        ""
    };

    let frontmatter: SkillMdFrontmatter =
        toml::from_str(toml_str).map_err(|e| format!("invalid SKILL.md frontmatter: {e}"))?;

    Ok((frontmatter, body.to_string()))
}

#[async_trait]
impl Skill for SkillMd {
    fn id(&self) -> &str {
        &self.frontmatter.name
    }

    fn name(&self) -> &str {
        &self.frontmatter.name
    }

    fn system_prompt(&self) -> &str {
        &self.raw_body
    }

    fn tools(&self) -> Vec<ToolSchema> {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_valid_skill_md() {
        let content = r#"---
name = "test-skill"
description = "A test skill"
version = "1.0"
user_invocable = true
emoji = "\U0001F680"

[requires]
env = ["MY_API_KEY"]
bins = ["jq"]
---
You are a helpful test skill.

Use these instructions carefully.
"#;
        let (fm, body) = parse_skill_md(content).unwrap();
        assert_eq!(fm.name, "test-skill");
        assert_eq!(fm.description, "A test skill");
        assert_eq!(fm.version, "1.0");
        assert!(fm.user_invocable);
        assert_eq!(fm.requires.env, vec!["MY_API_KEY"]);
        assert_eq!(fm.requires.bins, vec!["jq"]);
        assert!(body.starts_with("You are a helpful test skill."));
    }

    #[test]
    fn parse_minimal_skill_md() {
        let content = "---\nname = \"minimal\"\n---\nBody text\n";
        let (fm, body) = parse_skill_md(content).unwrap();
        assert_eq!(fm.name, "minimal");
        assert_eq!(fm.description, "");
        assert!(!fm.user_invocable);
        assert!(body.contains("Body text"));
    }

    #[test]
    fn parse_missing_delimiter() {
        let content = "name = \"no-delimiters\"\nBody text\n";
        assert!(parse_skill_md(content).is_err());
    }
}
