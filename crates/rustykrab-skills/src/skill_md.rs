use async_trait::async_trait;
use rustykrab_core::outcome::SignalClass;
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
    /// Declared definition of done. Absent means the skill is *frozen*:
    /// it runs normally, but the self-improvement outer loop will never
    /// propose edits to it. See `DREAMING.md`.
    #[serde(default)]
    pub outcome: Option<SkillOutcome>,
    /// Forward-compatible catch-all for unknown fields.
    #[serde(flatten)]
    pub extra: HashMap<String, toml::Value>,
}

/// What success means for a skill, and what evidence should establish it.
///
/// A skill exists only to cause an outcome in the world, so "better
/// instructions" is undefined except relative to that outcome. Declaring it
/// is what makes a skill eligible for automated improvement at all — an
/// undeclared skill can only be mutated blindly, which is drift rather than
/// improvement.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SkillOutcome {
    /// Natural-language definition of done.
    #[serde(default)]
    pub success: String,
    /// Optional machine-checkable post-conditions, when the skill's effect
    /// is verifiable (e.g. `"calendar.event_created"`).
    #[serde(default)]
    pub checks: Vec<String>,
    /// Which class of evidence to trust for this skill: `verifiable`,
    /// `explicit`, `implicit`, or `judge`.
    #[serde(default)]
    pub signal: Option<String>,
}

impl SkillOutcome {
    /// The declared signal class, defaulting to the weakest interpretation.
    ///
    /// An unset or unparseable `signal` yields [`SignalClass::Implicit`]
    /// rather than something stronger: mistakenly treating a proxy as
    /// ground truth is the failure that lets a loop optimize its own
    /// measurement, so an unclear declaration must not buy authority.
    pub fn signal_class(&self) -> SignalClass {
        self.signal
            .as_deref()
            .and_then(SignalClass::parse)
            .unwrap_or(SignalClass::Implicit)
    }

    /// Whether this declaration is complete enough to optimize against.
    ///
    /// A `[outcome]` block with an empty `success` states nothing, and is
    /// treated as if it were absent.
    pub fn is_declared(&self) -> bool {
        !self.success.trim().is_empty()
    }

    /// Whether outcomes for this skill can be established mechanically
    /// rather than inferred.
    pub fn is_verifiable(&self) -> bool {
        self.signal_class() == SignalClass::Verifiable && !self.checks.is_empty()
    }
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

impl SkillMd {
    /// Whether the self-improvement outer loop may propose changes to this
    /// skill.
    ///
    /// The rule is deliberately conservative: no declared outcome means
    /// frozen. A skill that cannot be measured can still be used — it just
    /// cannot be optimized, because there would be no way to tell an
    /// improvement from a regression.
    pub fn is_optimizable(&self) -> bool {
        self.frontmatter
            .outcome
            .as_ref()
            .is_some_and(|o| o.is_declared())
    }

    /// The evidence class this skill's outcomes should be judged by, or
    /// `None` if it has declared no outcome at all.
    pub fn signal_class(&self) -> Option<SignalClass> {
        self.frontmatter
            .outcome
            .as_ref()
            .filter(|o| o.is_declared())
            .map(|o| o.signal_class())
    }
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

    fn skill_from(content: &str) -> SkillMd {
        let (frontmatter, raw_body) = parse_skill_md(content).unwrap();
        SkillMd {
            path: PathBuf::from("/tmp/test-skill"),
            frontmatter,
            raw_body,
            validation: RequirementValidation {
                missing_env: Vec::new(),
                missing_bins: Vec::new(),
            },
        }
    }

    #[test]
    fn parses_outcome_block() {
        let skill = skill_from(
            r#"---
name = "calendar"

[outcome]
success = "The requested event exists and the user confirmed the details."
checks = ["calendar.event_created", "user.confirmed"]
signal = "verifiable"
---
Body
"#,
        );
        let outcome = skill.frontmatter.outcome.as_ref().unwrap();
        assert_eq!(
            outcome.success,
            "The requested event exists and the user confirmed the details."
        );
        assert_eq!(outcome.checks.len(), 2);
        assert_eq!(outcome.signal_class(), SignalClass::Verifiable);
        assert!(outcome.is_verifiable());
        assert!(skill.is_optimizable());
        assert_eq!(skill.signal_class(), Some(SignalClass::Verifiable));
    }

    #[test]
    fn skill_without_outcome_is_frozen() {
        let skill = skill_from("---\nname = \"legacy\"\n---\nBody\n");
        assert!(skill.frontmatter.outcome.is_none());
        assert!(!skill.is_optimizable());
        assert_eq!(skill.signal_class(), None);
    }

    #[test]
    fn empty_success_is_treated_as_undeclared() {
        // An [outcome] block that states nothing must not buy eligibility.
        let skill = skill_from(
            r#"---
name = "vague"

[outcome]
success = "   "
signal = "verifiable"
---
Body
"#,
        );
        assert!(!skill.is_optimizable());
        assert_eq!(skill.signal_class(), None);
    }

    #[test]
    fn unparseable_signal_falls_back_to_weakest_class() {
        // An unclear declaration must not be read as ground truth.
        let skill = skill_from(
            r#"---
name = "odd"

[outcome]
success = "Something happened."
signal = "wishful-thinking"
---
Body
"#,
        );
        assert_eq!(skill.signal_class(), Some(SignalClass::Implicit));
        assert!(!skill
            .frontmatter
            .outcome
            .as_ref()
            .unwrap()
            .signal_class()
            .is_ground_truth());
    }

    #[test]
    fn verifiable_requires_checks() {
        // Claiming a verifiable signal without post-conditions to check
        // leaves nothing to verify against.
        let skill = skill_from(
            r#"---
name = "claims-too-much"

[outcome]
success = "It worked."
signal = "verifiable"
---
Body
"#,
        );
        let outcome = skill.frontmatter.outcome.as_ref().unwrap();
        assert_eq!(outcome.signal_class(), SignalClass::Verifiable);
        assert!(!outcome.is_verifiable());
    }

    #[test]
    fn outcome_block_does_not_disturb_existing_fields() {
        let skill = skill_from(
            r#"---
name = "full"
description = "Has everything"
version = "2.0"
user_invocable = true

[requires]
env = ["KEY"]

[outcome]
success = "Done."
---
Body
"#,
        );
        assert_eq!(skill.frontmatter.name, "full");
        assert_eq!(skill.frontmatter.description, "Has everything");
        assert_eq!(skill.frontmatter.version, "2.0");
        assert!(skill.frontmatter.user_invocable);
        assert_eq!(skill.frontmatter.requires.env, vec!["KEY"]);
        assert!(skill.is_optimizable());
    }
}
