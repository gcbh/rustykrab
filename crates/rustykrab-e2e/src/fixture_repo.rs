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

    /// Run git in the fixture and return its trimmed stdout.
    pub fn git(&self, args: &[&str]) -> Result<String> {
        git_stdout(self.path(), args)
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

/// A recorded delivery `StackManifest`, the stand-in for the delivery
/// compiler until the control plan's Phase 7 (scenario 26). Layers run
/// bottom to top; each layer's `acceptance` becomes its parent item's
/// `done_when`, and `delivery_dependencies` become `blocks` edges between
/// the `code` items of a layer. `cyclic` adds the one dependency that closes
/// a cycle (`wi-1` on `wi-2`), which the import must reject whole.
pub fn stack_manifest(slice_title: &str, cyclic: bool) -> serde_json::Value {
    let first_dependencies = if cyclic { vec!["wi-2"] } else { vec![] };
    serde_json::json!({
        "slice": {
            "id": "slice-e2e-control-001",
            "title": slice_title,
            "objective": "Persist work items and list them over REST",
        },
        "layers": [
            {
                "id": "layer-1",
                "title": "Persist work items",
                "acceptance": "work items survive a daemon restart",
                "parent_layer": null,
                "work_items": [
                    {
                        "id": "wi-1",
                        "title": "Add the work_items table",
                        "objective": "Create the table and its migration",
                        "done_when": "the migration runs twice without error",
                        "delivery_dependencies": first_dependencies,
                    },
                    {
                        "id": "wi-2",
                        "title": "Add the store API over the table",
                        "objective": "Insert, read and list work items",
                        "done_when": "the store round-trips an item",
                        "delivery_dependencies": ["wi-1"],
                    },
                ],
            },
            {
                "id": "layer-2",
                "title": "List work items over REST",
                "acceptance": "GET /api/work lists open items",
                "parent_layer": "layer-1",
                "work_items": [
                    {
                        "id": "wi-3",
                        "title": "Add the list route",
                        "objective": "Serve open items as JSON",
                        "done_when": "the route returns the open items",
                        "delivery_dependencies": [],
                    },
                ],
            },
        ],
    })
}

/// Markers a prompt can carry to steer the [`ClaudeCodeStandIn`].
pub const CLAIM_WRONGLY: &str = "e2e-control-claim-wrongly";
pub const KEEPS_ITS_OWN_TRACKER: &str = "e2e-control-keeps-its-own-tracker";
/// What the stand-in reports after building the `tide_table` capability;
/// the resumed item's brief carries it as an input line.
pub const TIDE_TABLE_BUILT: &str =
    "e2e-control capability: the tide_table tool is built and verified";

/// A stand-in for `claude -p <prompt> --output-format json` (the control
/// plan's `claude_code` worker, Phases 3 and later). It commits one line in
/// its working directory and prints Claude Code's result envelope whose
/// `result` is the section 5 result contract. The prompt steers it:
/// [`CLAIM_WRONGLY`] claims a path the commit did not touch;
/// [`KEEPS_ITS_OWN_TRACKER`] writes a Beads file and a task list into the
/// worktree and returns one `discovered` draft; `tide_table` writes a
/// `SKILL.md` into the daemon's data dir and reports [`TIDE_TABLE_BUILT`].
pub struct ClaudeCodeStandIn {
    dir: tempfile::TempDir,
}

impl ClaudeCodeStandIn {
    pub fn create() -> Result<Self> {
        let dir = tempfile::Builder::new()
            .prefix("rustykrab-claude-code-stand-in-")
            .tempdir()?;
        let script = CLAUDE_CODE_STAND_IN
            .replace("{CLAIM_WRONGLY}", CLAIM_WRONGLY)
            .replace("{KEEPS_ITS_OWN_TRACKER}", KEEPS_ITS_OWN_TRACKER)
            .replace("{TIDE_TABLE_BUILT}", TIDE_TABLE_BUILT);
        let path = dir.path().join("claude");
        std::fs::write(&path, script)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
        }
        Ok(Self { dir })
    }

    /// The executable to register as the worker's command.
    pub fn path(&self) -> std::path::PathBuf {
        self.dir.path().join("claude")
    }
}

const CLAUDE_CODE_STAND_IN: &str = r##"#!/bin/sh
# Stand-in for `claude -p <prompt> --output-format json`, written by the
# RustyKrab e2e harness. No model, no network.
set -e
prompt="$*"
summary="Touched src/lib.rs"
claimed="src/lib.rs"
discovered="[]"
case "$prompt" in
  *tide_table*)
    mkdir -p "$RUSTYKRAB_DATA_DIR/skills/tide_table"
    printf '%s\n' '---' 'name: tide_table' 'description: Tide times for a port (e2e stand-in)' '---' \
      'Return high and low water for the named port.' > "$RUSTYKRAB_DATA_DIR/skills/tide_table/SKILL.md"
    summary="{TIDE_TABLE_BUILT}"
    ;;
esac
case "$prompt" in *{CLAIM_WRONGLY}*) claimed="src/elsewhere.rs" ;; esac
case "$prompt" in
  *{KEEPS_ITS_OWN_TRACKER}*)
    mkdir -p .beads .claude
    echo '{"id":"bd-1","title":"e2e-control beads task"}' > .beads/issues.jsonl
    echo '[{"content":"e2e-control claude task"}]' > .claude/tasks.json
    discovered='[{"kind":"code","title":"Add a changelog entry [e2e-control s31]","objective":"Record the stand-in change in CHANGELOG.md","done_when":"CHANGELOG.md names the change"}]'
    ;;
esac
commit="null"
if git rev-parse --git-dir >/dev/null 2>&1; then
  echo "// touched by the e2e claude_code stand-in" >> src/lib.rs
  git add src/lib.rs
  git -c user.name=e2e -c user.email=e2e@rustykrab.invalid commit -q --no-gpg-sign -m "e2e: stand-in change"
  commit="\"$(git rev-parse HEAD)\""
fi
contract=$(printf '{"summary":"%s","artifacts":[],"changed_paths":["%s"],"commit":%s,"checks_run":[],"known_limits":[],"blocked":null,"error":null,"questions":[],"discovered":%s}' "$summary" "$claimed" "$commit" "$discovered")
escaped=$(printf '%s' "$contract" | sed 's/\\/\\\\/g; s/"/\\"/g')
printf '{"type":"result","subtype":"success","is_error":false,"result":"%s"}\n' "$escaped"
"##;

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

    /// Kahn's algorithm over one manifest's work items: `Some(order)` for a
    /// DAG, `None` when a cycle remains.
    fn delivery_order(manifest: &serde_json::Value) -> Option<Vec<String>> {
        let items: Vec<serde_json::Value> = manifest["layers"]
            .as_array()?
            .iter()
            .flat_map(|layer| layer["work_items"].as_array().cloned().unwrap_or_default())
            .collect();
        let mut done: Vec<String> = Vec::new();
        while done.len() < items.len() {
            let next = items.iter().find(|item| {
                let id = item["id"].as_str().unwrap_or_default();
                !done.iter().any(|d| d == id)
                    && item["delivery_dependencies"]
                        .as_array()
                        .is_some_and(|deps| deps.iter().all(|d| done.iter().any(|x| d == x)))
            })?;
            done.push(next["id"].as_str()?.to_string());
        }
        Some(done)
    }

    #[test]
    fn stack_manifest_is_a_dag_and_its_cyclic_twin_is_not() {
        let manifest = stack_manifest("slice", false);
        assert_eq!(
            delivery_order(&manifest).unwrap(),
            vec!["wi-1", "wi-2", "wi-3"]
        );
        assert_eq!(manifest["layers"][1]["parent_layer"], "layer-1");
        assert!(delivery_order(&stack_manifest("slice", true)).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn claude_code_stand_in_commits_and_reports_the_result_contract() {
        let repo = FixtureRepo::create().unwrap();
        let stand_in = ClaudeCodeStandIn::create().unwrap();
        let output = Command::new(stand_in.path())
            .args(["-p", KEEPS_ITS_OWN_TRACKER, "--output-format", "json"])
            .current_dir(repo.path())
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let envelope: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        let contract: serde_json::Value =
            serde_json::from_str(envelope["result"].as_str().unwrap()).unwrap();
        let head = git_stdout(repo.path(), &["rev-parse", "HEAD"]).unwrap();
        assert_eq!(contract["commit"], head.as_str());
        assert_ne!(head, repo.head_sha());
        assert_eq!(contract["changed_paths"][0], "src/lib.rs");
        assert_eq!(contract["discovered"].as_array().unwrap().len(), 1);
        assert!(repo.path().join(".beads/issues.jsonl").exists());
    }

    #[test]
    fn conversation_rejects_an_unknown_checkpoint() {
        assert!(DELIVERY_PLANNING.replay_after("missing").is_err());
    }
}
