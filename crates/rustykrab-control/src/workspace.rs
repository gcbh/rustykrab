//! Isolated git worktrees for `code` runs, and the check of a `code` result
//! against them (plan section 5; the delivery plan's workspace kernel, the
//! part Phase 3 needs).
//!
//! A `code` item names its repository as a `repo:<path>` writable
//! resource, which is also what the single-writer rule serialises on. At
//! lease time the controller pins the repository's `HEAD` as the run's
//! parent commit and plans a [`Workspace`]: a new branch
//! (`rustykrab/work/<item>-<run>`) checked out in a worktree under the
//! daemon's data directory, never inside the user's own checkout. The
//! worker adapter creates it before the run and removes the worktree
//! directory after it; the branch stays, so the commit the run made stays
//! reachable and the controller can verify it once the directory is gone.
//!
//! [`verify`] is the controller's check (section 5: "the commit exists on
//! the expected parent, the changed paths match the diff"): the commit the
//! result names, or the branch tip when it names none, must exist, descend
//! from the pinned parent and lie on the run's branch, and the paths it
//! claims must be exactly the paths `git diff --name-only --no-renames`
//! reports between the parent and that commit (a rename counts as its two
//! paths). Anything else is a claim beyond the evidence.
//!
//! Every git call runs with hooks disabled (`core.hooksPath=/dev/null`) and
//! no terminal prompt, so creating a worktree in a user's repository runs
//! none of that repository's code. All of it is blocking; async callers
//! wrap it in `spawn_blocking`.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

/// The prefix of a repository writable resource: `repo:<path>`.
pub const REPO_PREFIX: &str = "repo:";

/// The evidence kind that records a run's workspace at lease time: the
/// [`Workspace`] as JSON. Unverified, so never handed on as an input.
pub const WORKSPACE_EVIDENCE: &str = "workspace";

/// Where a namespaced run branch starts.
const BRANCH_PREFIX: &str = "rustykrab/work/";

/// One run's isolated checkout of a repository.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Workspace {
    /// The item's configured repository.
    pub repo: PathBuf,
    /// The parent commit the run starts from, pinned at lease time.
    pub base: String,
    /// The run's branch in `repo`.
    pub branch: String,
    /// The worktree, under the daemon's data directory.
    pub path: PathBuf,
}

/// What a `code` result claims about its change.
#[derive(Debug, Clone, Copy)]
pub struct CodeClaim<'a> {
    pub commit: Option<&'a str>,
    pub changed_paths: &'a [String],
}

/// The verdict on a `code` claim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodeVerdict {
    /// The claim matches the repository: the commit and the paths as git
    /// reports them.
    Verified {
        commit: String,
        changed_paths: Vec<String>,
    },
    /// The claim says more, or other, than the evidence shows.
    Mismatch(String),
    /// Nothing was committed and nothing was claimed.
    Incomplete(String),
}

impl Workspace {
    /// The repository an item's writable resources name, if any: the first
    /// `repo:<path>` entry.
    pub fn repo_of(writable_resources: &[String]) -> Option<PathBuf> {
        writable_resources
            .iter()
            .find_map(|r| r.strip_prefix(REPO_PREFIX))
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .map(PathBuf::from)
    }

    /// The workspace a run of `item` gets: names only, nothing is created.
    pub fn plan(root: &Path, repo: &Path, base: &str, item: &str, run: &str) -> Workspace {
        let short = |s: &str| -> String {
            s.chars()
                .filter(|c| c.is_ascii_alphanumeric())
                .take(8)
                .collect::<String>()
                .to_ascii_lowercase()
        };
        let slug = format!("{}-{}", short(item), short(run));
        Workspace {
            repo: repo.to_path_buf(),
            base: base.to_string(),
            branch: format!("{BRANCH_PREFIX}{slug}"),
            path: root.join(slug),
        }
    }

    /// Create the branch at the parent commit and check it out in the
    /// worktree. An existing worktree at the path (a retried run) is
    /// removed first.
    pub fn create(&self) -> Result<(), String> {
        if self.path.exists() {
            self.remove()?;
        }
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
        }
        let path = self.path.to_string_lossy().to_string();
        let out = git(
            &self.repo,
            &["worktree", "add", "-B", &self.branch, &path, &self.base],
        )?;
        require(out, "git worktree add")
    }

    /// Remove the worktree directory and git's record of it. The branch
    /// stays, with whatever the run committed on it.
    pub fn remove(&self) -> Result<(), String> {
        let path = self.path.to_string_lossy().to_string();
        let removed = git(&self.repo, &["worktree", "remove", "--force", &path])
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !removed && self.path.exists() {
            std::fs::remove_dir_all(&self.path)
                .map_err(|e| format!("cannot remove {}: {e}", self.path.display()))?;
        }
        let _ = git(&self.repo, &["worktree", "prune"]);
        Ok(())
    }

    /// The branch's tip, or `None` when the branch does not exist.
    pub fn tip(&self) -> Result<Option<String>, String> {
        let spec = format!("refs/heads/{}^{{commit}}", self.branch);
        let out = git(&self.repo, &["rev-parse", "--verify", "--quiet", &spec])?;
        if !out.status.success() {
            return Ok(None);
        }
        Ok(Some(stdout(&out)))
    }

    /// Whether the run committed anything on its branch.
    pub fn has_commits(&self) -> bool {
        matches!(self.tip(), Ok(Some(tip)) if tip != self.base)
    }

    /// A claimed path as a path relative to the repository root: trimmed,
    /// without a leading `./`, and without the worktree's own prefix when
    /// the worker named it absolutely.
    fn relative(&self, claimed: &str) -> String {
        let claimed = claimed.trim();
        let claimed = Path::new(claimed)
            .strip_prefix(&self.path)
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| claimed.to_string());
        claimed.trim_start_matches("./").to_string()
    }
}

/// The commit `repo`'s `HEAD` points at.
pub fn head(repo: &Path) -> Result<String, String> {
    let out = git(repo, &["rev-parse", "--verify", "HEAD^{commit}"])?;
    if !out.status.success() {
        return Err(format!(
            "{} has no HEAD commit: {}",
            repo.display(),
            stderr(&out)
        ));
    }
    Ok(stdout(&out))
}

/// Check a `code` claim against the workspace's repository.
///
/// `Err` is a git failure (the repository is gone, git is missing), which
/// leaves the claim unverified.
pub fn verify(ws: &Workspace, claim: CodeClaim<'_>) -> Result<CodeVerdict, String> {
    let tip = ws.tip()?;
    let claimed: BTreeSet<String> = claim
        .changed_paths
        .iter()
        .map(|p| ws.relative(p))
        .filter(|p| !p.is_empty())
        .collect();
    let commit = match claim.commit.map(str::trim).filter(|c| !c.is_empty()) {
        Some(c) => c.to_string(),
        None => match tip.as_ref().filter(|t| **t != ws.base) {
            Some(t) => t.clone(),
            None if claimed.is_empty() => {
                return Ok(CodeVerdict::Incomplete(format!(
                    "nothing was committed on {} and nothing was claimed",
                    ws.branch
                )))
            }
            None => {
                return Ok(CodeVerdict::Mismatch(format!(
                    "the result claims {} changed path(s) but no commit exists on {} beyond \
                     the parent {}",
                    claimed.len(),
                    ws.branch,
                    short(&ws.base)
                )))
            }
        },
    };
    let kind = git(&ws.repo, &["cat-file", "-t", &commit])?;
    if !kind.status.success() || stdout(&kind) != "commit" {
        return Ok(CodeVerdict::Mismatch(format!(
            "commit {} does not exist in {}",
            short(&commit),
            ws.repo.display()
        )));
    }
    let full = stdout(&git(
        &ws.repo,
        &["rev-parse", "--verify", &format!("{commit}^{{commit}}")],
    )?);
    if full == ws.base {
        return Ok(CodeVerdict::Mismatch(format!(
            "commit {} is the parent itself, not a change on it",
            short(&full)
        )));
    }
    if !is_ancestor(&ws.repo, &ws.base, &full)? {
        return Ok(CodeVerdict::Mismatch(format!(
            "commit {} is not on the expected parent {}",
            short(&full),
            short(&ws.base)
        )));
    }
    if let Some(tip) = &tip {
        if !is_ancestor(&ws.repo, &full, tip)? {
            return Ok(CodeVerdict::Mismatch(format!(
                "commit {} is not on the run's branch {}",
                short(&full),
                ws.branch
            )));
        }
    }
    let diff = git(
        &ws.repo,
        &["diff", "--name-only", "--no-renames", &ws.base, &full],
    )?;
    if !diff.status.success() {
        return Err(format!("git diff failed: {}", stderr(&diff)));
    }
    let changed: BTreeSet<String> = String::from_utf8_lossy(&diff.stdout)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect();
    let unclaimed: Vec<&String> = changed.difference(&claimed).collect();
    let unchanged: Vec<&String> = claimed.difference(&changed).collect();
    if !unclaimed.is_empty() || !unchanged.is_empty() {
        let mut parts = Vec::new();
        if !unchanged.is_empty() {
            parts.push(format!("claimed but not in the diff: {unchanged:?}"));
        }
        if !unclaimed.is_empty() {
            parts.push(format!("in the diff but not claimed: {unclaimed:?}"));
        }
        return Ok(CodeVerdict::Mismatch(format!(
            "the changed paths do not match the diff of {}..{}: {}",
            short(&ws.base),
            short(&full),
            parts.join("; ")
        )));
    }
    Ok(CodeVerdict::Verified {
        commit: full,
        changed_paths: changed.into_iter().collect(),
    })
}

/// Remove worktrees under `root` older than `older_than` (by modification
/// time): the retention policy's sweep for runs whose worktree was kept.
/// Returns the paths removed.
pub fn prune_older_than(root: &Path, older_than: Duration) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let now = SystemTime::now();
    let mut removed = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let old = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| now.duration_since(t).ok())
            .is_some_and(|age| age > older_than);
        if !old || !path.is_dir() {
            continue;
        }
        // The worktree knows its repository: ask it, then remove it there.
        let common = git(
            &path,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )
        .ok()
        .filter(|o| o.status.success())
        .map(|o| PathBuf::from(stdout(&o)));
        let target = path.to_string_lossy().to_string();
        let done = common.is_some_and(|dir| {
            Command::new("git")
                .arg("--git-dir")
                .arg(&dir)
                .args(["worktree", "remove", "--force", &target])
                .stdin(Stdio::null())
                .output()
                .is_ok_and(|o| o.status.success())
        });
        if done || std::fs::remove_dir_all(&path).is_ok() {
            removed.push(path);
        }
    }
    removed
}

/// Choose the newest commit only when all candidates form one ancestry chain.
/// Incomparable verified branches need integration; silently choosing loses work.
pub fn continuation_base(repo: &Path, head: &str, commits: &[String]) -> Result<String, String> {
    let mut base = head.to_owned();
    for commit in commits {
        let resolved = git(
            repo,
            &["rev-parse", "--verify", &format!("{commit}^{{commit}}")],
        )?;
        if !resolved.status.success() {
            return Err(format!(
                "verified project commit {commit} is no longer available"
            ));
        }
        let commit = stdout(&resolved);
        if is_ancestor(repo, &base, &commit)? {
            base = commit;
        } else if !is_ancestor(repo, &commit, &base)? {
            return Err(format!("project history diverges at {base} and {commit}; integrate these branches before continuing"));
        }
    }
    Ok(base)
}

pub fn is_ancestor(repo: &Path, ancestor: &str, descendant: &str) -> Result<bool, String> {
    let out = git(repo, &["merge-base", "--is-ancestor", ancestor, descendant])?;
    match out.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(format!("git merge-base failed: {}", stderr(&out))),
    }
}

fn git(dir: &Path, args: &[&str]) -> Result<Output, String> {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "core.hooksPath=/dev/null"])
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("cannot run git: {e}"))
}

fn require(out: Output, what: &str) -> Result<(), String> {
    if out.status.success() {
        Ok(())
    } else {
        Err(format!("{what} failed: {}", stderr(&out)))
    }
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).trim().to_string()
}

fn short(sha: &str) -> String {
    sha.chars().take(12).collect()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A throwaway repository with one commit, and the git identity
    /// commits in it use.
    pub(crate) struct Repo {
        pub dir: tempfile::TempDir,
    }

    impl Repo {
        pub(crate) fn new() -> Repo {
            let dir = tempfile::tempdir().unwrap();
            let run = |args: &[&str]| run_git(dir.path(), args);
            run(&["init", "--initial-branch=main"]);
            std::fs::create_dir_all(dir.path().join("src")).unwrap();
            std::fs::write(dir.path().join("src/lib.rs"), "pub fn a() {}\n").unwrap();
            std::fs::write(dir.path().join("README.md"), "# fixture\n").unwrap();
            run(&["add", "."]);
            commit_all(dir.path(), "initial");
            Repo { dir }
        }

        pub(crate) fn path(&self) -> &Path {
            self.dir.path()
        }
    }

    pub(crate) fn run_git(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    pub(crate) fn commit_all(dir: &Path, message: &str) -> String {
        run_git(dir, &["add", "-A"]);
        run_git(
            dir,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@example.invalid",
                "commit",
                "-q",
                "--no-gpg-sign",
                "-m",
                message,
            ],
        );
        run_git(dir, &["rev-parse", "HEAD"])
    }

    fn workspace(repo: &Repo, root: &Path) -> Workspace {
        let base = head(repo.path()).unwrap();
        Workspace::plan(root, repo.path(), &base, "item-0001-abcd", "run-9f8e7d6c")
    }

    #[test]
    fn a_workspace_is_a_branch_in_a_worktree_outside_the_checkout() {
        let repo = Repo::new();
        let root = tempfile::tempdir().unwrap();
        let ws = workspace(&repo, root.path());
        assert_eq!(ws.branch, "rustykrab/work/item0001-run9f8e7");
        assert!(ws.path.starts_with(root.path()));
        ws.create().unwrap();
        assert!(ws.path.join("src/lib.rs").exists());
        assert!(!ws.has_commits());

        std::fs::write(ws.path.join("src/lib.rs"), "pub fn a() {}\npub fn b() {}\n").unwrap();
        let commit = commit_all(&ws.path, "change");
        assert!(ws.has_commits());
        ws.remove().unwrap();
        assert!(!ws.path.exists(), "the worktree directory goes");
        assert_eq!(ws.tip().unwrap().as_deref(), Some(commit.as_str()));
        // The user's checkout never moved.
        assert_eq!(head(repo.path()).unwrap(), ws.base);
        assert_eq!(run_git(repo.path(), &["status", "--short"]), "");
    }

    #[test]
    fn a_claim_is_checked_against_the_commit_and_its_diff() {
        let repo = Repo::new();
        let root = tempfile::tempdir().unwrap();
        let ws = workspace(&repo, root.path());
        ws.create().unwrap();
        std::fs::write(ws.path.join("src/lib.rs"), "pub fn changed() {}\n").unwrap();
        let commit = commit_all(&ws.path, "change");
        let lib = vec!["src/lib.rs".to_string()];

        // The claimed commit and paths.
        let got = verify(
            &ws,
            CodeClaim {
                commit: Some(&commit),
                changed_paths: &lib,
            },
        )
        .unwrap();
        assert_eq!(
            got,
            CodeVerdict::Verified {
                commit: commit.clone(),
                changed_paths: lib.clone(),
            }
        );
        // No commit named: the branch tip stands in; an absolute path
        // inside the worktree reads as relative.
        let absolute = vec![ws.path.join("src/lib.rs").to_string_lossy().to_string()];
        assert!(matches!(
            verify(
                &ws,
                CodeClaim {
                    commit: None,
                    changed_paths: &absolute
                }
            )
            .unwrap(),
            CodeVerdict::Verified { .. }
        ));

        // Other paths than the diff.
        let wrong = vec!["src/elsewhere.rs".to_string()];
        let CodeVerdict::Mismatch(why) = verify(
            &ws,
            CodeClaim {
                commit: Some(&commit),
                changed_paths: &wrong,
            },
        )
        .unwrap() else {
            panic!("a wrong path verified")
        };
        assert!(
            why.contains("src/elsewhere.rs") && why.contains("src/lib.rs"),
            "{why}"
        );

        // A commit that does not exist.
        let CodeVerdict::Mismatch(why) = verify(
            &ws,
            CodeClaim {
                commit: Some("0000000000000000000000000000000000000000"),
                changed_paths: &lib,
            },
        )
        .unwrap() else {
            panic!("a missing commit verified")
        };
        assert!(why.contains("does not exist"), "{why}");

        // The parent itself.
        let CodeVerdict::Mismatch(why) = verify(
            &ws,
            CodeClaim {
                commit: Some(&ws.base),
                changed_paths: &lib,
            },
        )
        .unwrap() else {
            panic!("the parent verified as a change")
        };
        assert!(why.contains("parent itself"), "{why}");
    }

    #[test]
    fn a_commit_off_the_parent_or_off_the_branch_is_refused() {
        let repo = Repo::new();
        let root = tempfile::tempdir().unwrap();
        let ws = workspace(&repo, root.path());
        ws.create().unwrap();
        std::fs::write(ws.path.join("README.md"), "# on the branch\n").unwrap();
        commit_all(&ws.path, "on the branch");

        // A commit in the user's own checkout, on top of the parent but not
        // on the run's branch.
        std::fs::write(repo.path().join("README.md"), "# elsewhere\n").unwrap();
        let elsewhere = commit_all(repo.path(), "elsewhere");
        let readme = vec!["README.md".to_string()];
        let CodeVerdict::Mismatch(why) = verify(
            &ws,
            CodeClaim {
                commit: Some(&elsewhere),
                changed_paths: &readme,
            },
        )
        .unwrap() else {
            panic!("a commit off the branch verified")
        };
        assert!(why.contains("not on the run's branch"), "{why}");

        // A workspace whose parent is not an ancestor of the claim.
        let mut other = ws.clone();
        other.base = elsewhere.clone();
        let tip = ws.tip().unwrap().unwrap();
        let CodeVerdict::Mismatch(why) = verify(
            &other,
            CodeClaim {
                commit: Some(&tip),
                changed_paths: &readme,
            },
        )
        .unwrap() else {
            panic!("a commit off the parent verified")
        };
        assert!(why.contains("not on the expected parent"), "{why}");
    }

    #[test]
    fn nothing_committed_is_incomplete_or_a_mismatch() {
        let repo = Repo::new();
        let root = tempfile::tempdir().unwrap();
        let ws = workspace(&repo, root.path());
        ws.create().unwrap();
        assert!(matches!(
            verify(
                &ws,
                CodeClaim {
                    commit: None,
                    changed_paths: &[]
                }
            )
            .unwrap(),
            CodeVerdict::Incomplete(_)
        ));
        let lib = vec!["src/lib.rs".to_string()];
        assert!(matches!(
            verify(
                &ws,
                CodeClaim {
                    commit: None,
                    changed_paths: &lib
                }
            )
            .unwrap(),
            CodeVerdict::Mismatch(_)
        ));
        assert_eq!(
            Workspace::repo_of(&["calendar".into(), "repo:/src/app".into()]),
            Some(PathBuf::from("/src/app"))
        );
        assert_eq!(Workspace::repo_of(&["calendar".into()]), None);
    }

    #[test]
    fn a_continuation_preserves_all_verified_commits_or_refuses_divergence() {
        let repo = Repo::new();
        let root = tempfile::tempdir().unwrap();
        let ws = workspace(&repo, root.path());
        ws.create().unwrap();
        std::fs::write(ws.path.join("first.txt"), "first").unwrap();
        let first = commit_all(&ws.path, "first");
        std::fs::write(ws.path.join("second.txt"), "second").unwrap();
        let second = commit_all(&ws.path, "second");
        assert_eq!(
            continuation_base(repo.path(), &ws.base, &[second.clone(), first.clone()]).unwrap(),
            second
        );
        std::fs::write(repo.path().join("other.txt"), "other").unwrap();
        let divergent = commit_all(repo.path(), "other branch");
        assert!(
            continuation_base(repo.path(), &ws.base, &[first, divergent])
                .unwrap_err()
                .contains("diverges")
        );
        assert!(continuation_base(repo.path(), &ws.base, &["f".repeat(40)])
            .unwrap_err()
            .contains("no longer available"));
        ws.remove().unwrap();
    }

    #[test]
    fn stale_worktrees_are_pruned_and_fresh_ones_kept() {
        let repo = Repo::new();
        let root = tempfile::tempdir().unwrap();
        let ws = workspace(&repo, root.path());
        ws.create().unwrap();
        assert!(prune_older_than(root.path(), Duration::from_secs(3600)).is_empty());
        std::thread::sleep(Duration::from_millis(20));
        let removed = prune_older_than(root.path(), Duration::ZERO);
        assert_eq!(removed, vec![ws.path.clone()]);
        assert!(!ws.path.exists());
    }
}
