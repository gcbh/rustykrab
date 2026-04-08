use std::path::{Path, PathBuf};

use crate::skill_md::{
    parse_skill_md, RequirementValidation, SkillMd, SkillMdFrontmatter,
};

/// Scan `skills_dir` for subdirectories containing a `SKILL.md` file.
///
/// Each valid skill directory is expected to have:
/// ```text
/// skills_dir/
///   my-skill/
///     SKILL.md
/// ```
pub fn load_skills_from_dir(skills_dir: &Path) -> anyhow::Result<Vec<SkillMd>> {
    if !skills_dir.is_dir() {
        tracing::debug!(path = %skills_dir.display(), "skills directory does not exist");
        return Ok(Vec::new());
    }

    let mut skills = Vec::new();

    let entries = std::fs::read_dir(skills_dir)?;
    for entry in entries {
        let entry = entry?;
        let skill_dir = entry.path();
        if !skill_dir.is_dir() {
            continue;
        }

        let skill_md_path = skill_dir.join("SKILL.md");
        if !skill_md_path.is_file() {
            tracing::debug!(path = %skill_dir.display(), "skipping directory (no SKILL.md)");
            continue;
        }

        match load_single_skill(&skill_dir, &skill_md_path) {
            Ok(skill) => {
                tracing::info!(
                    name = %skill.frontmatter.name,
                    satisfied = skill.validation.is_satisfied(),
                    "loaded skill"
                );
                skills.push(skill);
            }
            Err(e) => {
                tracing::warn!(
                    path = %skill_md_path.display(),
                    error = %e,
                    "failed to load skill"
                );
            }
        }
    }

    Ok(skills)
}

fn load_single_skill(skill_dir: &Path, skill_md_path: &Path) -> anyhow::Result<SkillMd> {
    let content = std::fs::read_to_string(skill_md_path)?;
    let (frontmatter, raw_body) =
        parse_skill_md(&content).map_err(|e| anyhow::anyhow!("{e}"))?;
    let validation = validate_requirements(&frontmatter);

    Ok(SkillMd {
        path: skill_dir.to_path_buf(),
        frontmatter,
        raw_body,
        validation,
    })
}

/// Check which of a skill's required env vars and binaries are present.
pub fn validate_requirements(fm: &SkillMdFrontmatter) -> RequirementValidation {
    let missing_env = fm
        .requires
        .env
        .iter()
        .filter(|var| std::env::var(var).is_err())
        .cloned()
        .collect();

    let missing_bins = fm
        .requires
        .bins
        .iter()
        .filter(|bin| which_bin(bin).is_none())
        .cloned()
        .collect();

    RequirementValidation {
        missing_env,
        missing_bins,
    }
}

/// Locate a binary on `$PATH`, similar to the `which` command.
pub fn which_bin(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths).find_map(|dir| {
            let candidate = dir.join(name);
            if candidate.is_file() {
                Some(candidate)
            } else {
                None
            }
        })
    })
}
