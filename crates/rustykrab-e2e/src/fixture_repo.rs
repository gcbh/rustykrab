//! Deterministic repositories and planning conversations for project E2E tests.
//!
//! Planning scenarios should begin where a user begins: with an ordinary,
//! incomplete idea and a real repository to inspect.  This module creates that
//! repository without network access and gives scenarios a stable transcript
//! they can replay after a daemon restart or context compaction.

use std::path::Path;
use std::process::{Command, Output};

use anyhow::{bail, Context, Result};

/// Stable identity for the canonical planning conversation in this fixture.
pub const CONVERSATION_ID: &str = "conversation-planning-delivery-001";

/// One source message in a planning conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConversationTurn {
    pub message_id: &'static str,
    pub role: &'static str,
    pub content: &'static str,
}

/// A replayable conversation beginning with an intentionally vague idea.
#[derive(Debug, Clone, Copy)]
pub struct ConversationFixture {
    pub id: &'static str,
    pub conversation_id: &'static str,
    pub turns: &'static [ConversationTurn],
}

impl ConversationFixture {
    /// Replay the complete conversation in source-message order.
    pub fn replay(&self) -> impl Iterator<Item = ConversationTurn> + '_ {
        self.turns.iter().copied()
    }

    /// Resume immediately after a previously persisted source message.
    ///
    /// A compacted context uses this together with durable project state.  The
    /// project store links to these IDs; it must not copy this transcript.
    pub fn replay_after(
        &self,
        message_id: &str,
    ) -> Result<impl Iterator<Item = ConversationTurn> + '_> {
        let index = self
            .turns
            .iter()
            .position(|turn| turn.message_id == message_id)
            .with_context(|| format!("message {message_id} is not in fixture {}", self.id))?;
        Ok(self.turns[index + 1..].iter().copied())
    }

    pub fn opening(&self) -> ConversationTurn {
        self.turns[0]
    }
}

const DELIVERY_PLANNING_TURNS: &[ConversationTurn] = &[
    ConversationTurn {
        message_id: "message-001-vague-idea",
        role: "user",
        content: "I want this repository to carry out long-running software plans for me.",
    },
    ConversationTurn {
        message_id: "message-002-material-question",
        role: "assistant",
        content: "Should the project have one durable planning conversation, or can planning be split across independent task threads?",
    },
    ConversationTurn {
        message_id: "message-003-decision",
        role: "user",
        content: "Use one durable project conversation. Pause planning until every specialist reports back.",
    },
    ConversationTurn {
        message_id: "message-004-correction",
        role: "user",
        content: "Correction: keep one durable conversation, but let specialist work continue concurrently and link each result when it arrives.",
    },
    ConversationTurn {
        message_id: "message-005-authorization",
        role: "user",
        content: "Build the durable planning model first. Deployment details can remain open for a later slice.",
    },
];

pub const DELIVERY_PLANNING: ConversationFixture = ConversationFixture {
    id: "delivery-planning-v1",
    conversation_id: CONVERSATION_ID,
    turns: DELIVERY_PLANNING_TURNS,
};

/// A throwaway Git repository with a reproducible initial commit.
pub struct FixtureRepo {
    root: tempfile::TempDir,
    head_sha: String,
}

impl FixtureRepo {
    pub fn create() -> Result<Self> {
        let root = tempfile::Builder::new()
            .prefix("rustykrab-planning-fixture-")
            .tempdir()?;
        seed_files(root.path())?;

        git(root.path(), &["init", "--initial-branch=main"])?;
        git(root.path(), &["config", "user.name", "RustyKrab E2E"])?;
        git(
            root.path(),
            &["config", "user.email", "e2e@rustykrab.invalid"],
        )?;
        git(root.path(), &["config", "commit.gpgsign", "false"])?;
        git(root.path(), &["config", "core.autocrlf", "false"])?;
        git(root.path(), &["config", "core.filemode", "false"])?;
        git(
            root.path(),
            &[
                "add",
                "--",
                ".gitignore",
                "Cargo.toml",
                "README.md",
                "src/lib.rs",
            ],
        )?;

        let output = git_command(root.path())
            .args(["commit", "--no-gpg-sign", "-m", "fixture: initial project"])
            .env("GIT_AUTHOR_DATE", "2026-01-01T00:00:00Z")
            .env("GIT_COMMITTER_DATE", "2026-01-01T00:00:00Z")
            .output()
            .context("run deterministic fixture commit")?;
        require_success("git commit", output)?;

        let head_sha = git_stdout(root.path(), &["rev-parse", "HEAD"])?;
        Ok(Self { root, head_sha })
    }

    pub fn path(&self) -> &Path {
        self.root.path()
    }

    pub fn head_sha(&self) -> &str {
        &self.head_sha
    }

    /// Verify the facts on which planning scenarios rely.
    pub fn verify(&self) -> Result<()> {
        let branch = git_stdout(self.path(), &["branch", "--show-current"])?;
        if branch != "main" {
            bail!("fixture branch is {branch}, want main");
        }
        let head = git_stdout(self.path(), &["rev-parse", "HEAD"])?;
        if head != self.head_sha {
            bail!("fixture HEAD changed from {} to {head}", self.head_sha);
        }
        let status = git_stdout(self.path(), &["status", "--short"])?;
        if !status.is_empty() {
            bail!("fixture repository is dirty: {status}");
        }
        let readme = std::fs::read_to_string(self.path().join("README.md"))?;
        if !readme.contains("manual release checklist") {
            bail!("fixture no longer contains the repository fact scenarios inspect");
        }
        Ok(())
    }
}

fn seed_files(root: &Path) -> Result<()> {
    std::fs::create_dir_all(root.join("src"))?;
    std::fs::write(root.join(".gitignore"), "/target\n")?;
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"fixture-service\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )?;
    std::fs::write(
        root.join("README.md"),
        "# Fixture Service\n\nThis service currently uses a manual release checklist.\n",
    )?;
    std::fs::write(
        root.join("src/lib.rs"),
        "/// Return the fixture service's status.\npub fn status() -> &'static str { \"ready\" }\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn status_is_ready() {\n        assert_eq!(super::status(), \"ready\");\n    }\n}\n",
    )?;
    Ok(())
}

fn git(root: &Path, args: &[&str]) -> Result<()> {
    let output = git_command(root)
        .args(args)
        .output()
        .with_context(|| format!("run git {}", args.join(" ")))?;
    require_success(&format!("git {}", args.join(" ")), output)
}

fn git_stdout(root: &Path, args: &[&str]) -> Result<String> {
    let output = git_command(root)
        .args(args)
        .output()
        .with_context(|| format!("run git {}", args.join(" ")))?;
    if !output.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_string())
}

fn git_command(root: &Path) -> Command {
    let mut command = Command::new("git");
    command
        .current_dir(root)
        // A developer's global config can install hooks, signing, or file
        // transforms. None of those belong in a deterministic fixture.
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1");
    command
}

fn require_success(operation: &str, output: Output) -> Result<()> {
    if output.status.success() {
        return Ok(());
    }
    bail!(
        "{operation} failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixture_repository_is_clean_and_reproducible() {
        let first = FixtureRepo::create().unwrap();
        let second = FixtureRepo::create().unwrap();
        first.verify().unwrap();
        second.verify().unwrap();
        assert_eq!(first.head_sha(), second.head_sha());
    }

    #[test]
    fn conversation_can_resume_from_a_source_message() {
        let all: Vec<_> = DELIVERY_PLANNING.replay().collect();
        assert!(all[0].content.starts_with("I want"));
        assert_eq!(all[0].role, "user");

        let resumed: Vec<_> = DELIVERY_PLANNING
            .replay_after("message-003-decision")
            .unwrap()
            .collect();
        assert_eq!(resumed[0].message_id, "message-004-correction");
        assert_eq!(resumed.len(), 2);
    }

    #[test]
    fn conversation_rejects_an_unknown_checkpoint() {
        assert!(DELIVERY_PLANNING.replay_after("missing").is_err());
    }
}
