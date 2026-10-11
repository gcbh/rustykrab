//! `ExternalWorker`: the `claude_code` and `codex` worker kinds of the
//! control layer (`docs/plans/control-layer-and-worker-fleet.md`, section
//! 5).
//!
//! One run is one headless invocation of the agent's own CLI:
//!
//! - **claude_code**: `claude -p <brief> --output-format stream-json
//!   --verbose --max-turns <n> --permission-mode <mode> --allowedTools <list>
//!   --disallowedTools <list>` (the built-in deny list, then the spec's
//!   `denied_tools`), plus `--model` and, for a capability build,
//!   `--add-dir <skills dir>`.
//! - **codex**: `codex exec --json --skip-git-repo-check --sandbox
//!   workspace-write --cd <dir> --output-last-message <file> <brief>`, plus
//!   `--model` and `--add-dir <skills dir>`.
//!
//! **Where it runs.** A `code` brief carries a
//! [`rustykrab_control::workspace::Workspace`]: the adapter refuses a
//! repository it was not added for (a `policy/scope` stop), creates the
//! worktree under the daemon's data dir, and runs the agent there, never in
//! the user's own checkout. Any other brief runs in a scratch directory
//! under `<data dir>/workers/<name>/runs`. After the run the directory is
//! removed when the run produced a result; a run that ended without one
//! keeps it for diagnosis, and kept directories older than the retention
//! window are pruned at the next run ([`Retention`]). The branch outlives
//! the directory, so the controller verifies the commit after it is gone.
//!
//! **What it is told.** The brief is rendered as the delivery plan's
//! executor brief ([`render_executor_brief`]): the item's typed fields, the
//! repository, worktree, branch and parent commit, the inputs and the prior
//! failure, and the section 5 result contract the final message must be. A
//! capability build of a tool is told to write it as
//! `<skills dir>/<tool>/SKILL.md`, which the daemon loads at run time.
//!
//! **What it returns.** The contract JSON is taken from the agent's final
//! message ([`parse_contract`]). The adapter adds what it saw itself: every
//! shell command the agent ran, as `command_run` artifacts (a model-written
//! `command_run` is dropped), which is the evidence the controller checks
//! `checks_run` against. A run that ends without a contract is a typed
//! [`RunFailure`]: the turn cap is `budget/iterations`, the timeout
//! `budget/wall`, a failed process `process/<code>`, an unreadable final
//! message `model/format`.
//!
//! **Its turn budget.** The brief tells a Claude Code agent how many turns
//! it has and asks it to commit and return the contract with a few to
//! spare. One that still stops at the cap (`error_max_turns`) and printed a
//! session id is resumed once (`--resume <session>`, the same allowlist,
//! permission mode and directory, at most [`RESUME_TURNS`] turns) and asked
//! only for the contract for the work it already committed. That report is
//! read and attested as usual, with a `known_limits` entry saying it was
//! recovered; if the resume fails too, the run stays `budget/iterations`.
//!
//! **Its environment** is cleared: only the variables an agent CLI needs
//! (`PATH`, `HOME`, the locale, its own config and key variables) and the
//! names the spec lists pass through, plus `RUSTYKRAB_DATA_DIR` and
//! `RUSTYKRAB_SKILLS_DIR`. The daemon's own secrets never do.
//!
//! **Its process group.** On unix the agent starts in a process group of
//! its own, and every live run's group is recorded in a [`RunGroups`].
//! **Cancel** drops the run's future, which kills the whole group, not just
//! the agent (`kill_on_drop` alone would miss what the agent started); its
//! worktree is kept and pruned by retention. Daemon shutdown calls
//! [`RunGroups::terminate_all`], which sends every live group SIGTERM and
//! then SIGKILL after a short grace, so no agent outlives the daemon and
//! keeps working while the next daemon re-runs its item. It marks the
//! groups it ends first, and a run whose group was marked returns
//! [`RunFailure::Interrupted`] instead of a failed process, which the
//! controller returns to `ready` without climbing the ladder.

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use rustykrab_control::errors::{BudgetKind, PolicyStop, ProviderProblem};
use rustykrab_control::registry::WorkerSpec;
use rustykrab_control::routing::built_tool;
use rustykrab_control::worker::{
    Brief, RunFailure, RunUsage, Worker, WorkerCapabilities, COMMAND_RUN,
};
use rustykrab_control::workspace::{self, REPO_PREFIX};
use rustykrab_core::work::{ArtifactRef, ResultReport, WorkerKind};
use rustykrab_core::{Error, Result};
use serde_json::Value;

/// Characters of an agent's stderr kept in a failure.
const STDERR_TAIL: usize = 600;
/// Commands recorded per run; the rest are counted, not kept.
const COMMANDS_MAX: usize = 64;
/// Turns a resume after the turn cap gets to return the contract.
pub const RESUME_TURNS: u32 = 2;

/// The section 5 result contract's shape, as the brief shows it.
const CONTRACT_SHAPE: &str = "{\"summary\": \"...\", \"artifacts\": [{\"kind\": \"path\", \
     \"value\": \"...\"}], \"changed_paths\": [\"...\"], \"commit\": \"<sha>\" or null, \
     \"checks_run\": [\"...\"], \"known_limits\": [], \"blocked\": null, \"error\": null, \
     \"questions\": [], \"discovered\": [{\"kind\": \"code\", \"title\": \"...\", \
     \"objective\": \"...\", \"done_when\": \"...\"}]}";

/// Claude Code's tools a worker gets when the spec names none: reading and
/// editing files, and the local git and build commands a change needs.
pub const CLAUDE_DEFAULT_TOOLS: [&str; 14] = [
    "Read",
    "Glob",
    "Grep",
    "Edit",
    "MultiEdit",
    "Write",
    "TodoWrite",
    "Bash(git status:*)",
    "Bash(git diff:*)",
    "Bash(git add:*)",
    "Bash(git commit:*)",
    "Bash(git log:*)",
    "Bash(ls:*)",
    "Bash(cargo:*)",
];

/// Never allowed, whatever the spec says: publishing and the network are
/// not a worker's to use (delivery plan, section 7.5).
pub const CLAUDE_DENIED_TOOLS: [&str; 3] = ["Bash(git push:*)", "WebFetch", "WebSearch"];

/// Claude Code's `--disallowedTools`: [`CLAUDE_DENIED_TOOLS`], then the
/// worker's own additions. A spec can deny more, never less.
fn denied_list(extra: &[String]) -> String {
    CLAUDE_DENIED_TOOLS
        .iter()
        .copied()
        .chain(extra.iter().map(String::as_str))
        .collect::<Vec<_>>()
        .join(",")
}

/// Environment variables every agent process gets, when the daemon has
/// them.
const BASE_ENV: [&str; 9] = [
    "PATH", "HOME", "USER", "LOGNAME", "LANG", "LC_ALL", "TERM", "TMPDIR", "SHELL",
];
const CLAUDE_ENV: [&str; 4] = [
    "CLAUDE_CONFIG_DIR",
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_BASE_URL",
    "ANTHROPIC_MODEL",
];
const CODEX_ENV: [&str; 3] = ["CODEX_HOME", "OPENAI_API_KEY", "OPENAI_BASE_URL"];

/// When a run's directory goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Retention {
    /// Remove it after every run.
    Always,
    /// Remove it after a run that produced a result; keep it after one that
    /// did not, and prune kept ones older than the window.
    KeepFailed(Duration),
}

impl Default for Retention {
    fn default() -> Self {
        Retention::KeepFailed(Duration::from_secs(7 * 24 * 3600))
    }
}

const SIGKILL: i32 = 9;
const SIGTERM: i32 = 15;

/// How long a run its wall limit ended gets for its pipes to drain once its
/// group is killed.
const DRAIN_GRACE: Duration = Duration::from_secs(2);

/// Copy `pipe` into a buffer as it arrives, so what a run printed survives
/// the run being ended. The task finishes at end of file.
fn capture<R>(pipe: Option<R>) -> (Arc<Mutex<Vec<u8>>>, tokio::task::JoinHandle<()>)
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    use tokio::io::AsyncReadExt as _;
    let buf = Arc::new(Mutex::new(Vec::new()));
    let task = tokio::spawn({
        let buf = buf.clone();
        async move {
            let Some(mut pipe) = pipe else { return };
            let mut chunk = [0u8; 8192];
            while let Ok(n @ 1..) = pipe.read(&mut chunk).await {
                buf.lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .extend_from_slice(&chunk[..n]);
            }
        }
    });
    (buf, task)
}

/// Send `signal` to every process in group `group`; 0 only probes. Whether
/// the call succeeded, which for a probe means the group still exists.
#[cfg(unix)]
fn signal_group(group: i32, signal: i32) -> bool {
    extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    // A group id of 1 or less would name init or every process, never an
    // agent's own group.
    if group <= 1 {
        return false;
    }
    // SAFETY: kill(2) takes plain integers and touches no memory of ours; a
    // negative pid addresses the process group.
    unsafe { kill(-group, signal) == 0 }
}

#[cfg(not(unix))]
fn signal_group(_group: i32, _signal: i32) -> bool {
    false
}

/// The process groups of the external runs alive right now, so daemon
/// shutdown can end them. Every [`ExternalWorker`] records into
/// [`RunGroups::global`] unless given its own
/// ([`ExternalWorker::with_groups`]).
#[derive(Debug, Default)]
pub struct RunGroups {
    live: Mutex<HashSet<i32>>,
    /// Groups [`RunGroups::terminate_all`] ended: their runs report an
    /// interruption, not a failure.
    interrupted: Mutex<HashSet<i32>>,
}

/// One run's membership in a [`RunGroups`]: dropping it forgets the group
/// and kills whatever is left in it.
struct GroupGuard {
    groups: Arc<RunGroups>,
    group: i32,
}

impl Drop for GroupGuard {
    fn drop(&mut self) {
        self.groups
            .live
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.group);
        self.groups
            .interrupted
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.group);
        signal_group(self.group, SIGKILL);
    }
}

impl GroupGuard {
    /// Whether shutdown ended this run's group.
    fn interrupted(&self) -> bool {
        self.groups
            .interrupted
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(&self.group)
    }
}

impl RunGroups {
    /// The daemon's one set, which every worker records into by default.
    pub fn global() -> Arc<RunGroups> {
        static GLOBAL: OnceLock<Arc<RunGroups>> = OnceLock::new();
        GLOBAL.get_or_init(Arc::default).clone()
    }

    /// The group ids of the live runs.
    pub fn live(&self) -> Vec<i32> {
        let mut live: Vec<i32> = self
            .live
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .copied()
            .collect();
        live.sort_unstable();
        live
    }

    fn enter(self: &Arc<Self>, group: i32) -> GroupGuard {
        self.live
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(group);
        GroupGuard {
            groups: self.clone(),
            group,
        }
    }

    /// End every live run: SIGTERM to each group, then SIGKILL to any
    /// still there after `grace`. Returns how many groups were signalled.
    /// Each group is marked first, so its run reports
    /// [`RunFailure::Interrupted`] rather than a failed process, and keeps
    /// its worktree for retention.
    pub async fn terminate_all(&self, grace: Duration) -> usize {
        let groups = self.live();
        if groups.is_empty() {
            return 0;
        }
        tracing::info!(groups = ?groups, "terminating external worker runs");
        self.interrupted
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .extend(groups.iter().copied());
        for &group in &groups {
            signal_group(group, SIGTERM);
        }
        let deadline = tokio::time::Instant::now() + grace;
        loop {
            // Gone once its run has let it go, or nothing answers in it.
            let live = self.live();
            let left: Vec<i32> = groups
                .iter()
                .copied()
                .filter(|g| live.contains(g) && signal_group(*g, 0))
                .collect();
            if left.is_empty() {
                break;
            }
            if tokio::time::Instant::now() >= deadline {
                tracing::warn!(groups = ?left, "external runs outlived SIGTERM; killing");
                for group in left {
                    signal_group(group, SIGKILL);
                }
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        groups.len()
    }
}

/// How an [`ExternalWorker`] runs its agent.
#[derive(Debug, Clone, PartialEq)]
pub struct ExternalConfig {
    /// `ClaudeCode` or `Codex`.
    pub kind: WorkerKind,
    pub command: PathBuf,
    /// Repositories it may work in, as given (`repo:<path>` resources).
    pub repos: Vec<String>,
    pub model: Option<String>,
    pub claude_config_dir: Option<PathBuf>,
    pub require_max: bool,
    pub codex_home: Option<PathBuf>,
    pub require_chatgpt: bool,
    /// Claude Code's `--allowedTools`; empty takes [`CLAUDE_DEFAULT_TOOLS`].
    pub allowed_tools: Vec<String>,
    /// Denied on top of [`CLAUDE_DENIED_TOOLS`] in Claude Code's
    /// `--disallowedTools`; empty for codex, which has no such flag.
    pub denied_tools: Vec<String>,
    pub max_turns: u32,
    pub permission_mode: String,
    pub timeout: Duration,
    pub concurrency: usize,
    /// Extra environment variables passed through, by name.
    pub env: Vec<String>,
    /// The daemon's data directory, and where scratch runs go under it.
    pub data_dir: PathBuf,
    /// The daemon's skills directory, where a capability build writes a
    /// tool as `SKILL.md`.
    pub skills_dir: PathBuf,
    pub retention: Retention,
}

impl ExternalConfig {
    /// The config a registry spec describes, on this daemon.
    pub fn from_spec(
        spec: &WorkerSpec,
        data_dir: &Path,
    ) -> std::result::Result<ExternalConfig, String> {
        let default_command = match spec.kind {
            WorkerKind::ClaudeCode => "claude",
            WorkerKind::Codex => "codex",
            other => {
                return Err(format!(
                    "no external adapter for `{}` workers",
                    other.as_str()
                ))
            }
        };
        if let Some(bad) = spec.repos.iter().find(|r| r.trim().is_empty()) {
            return Err(format!("empty repository path {bad:?}"));
        }
        if (spec.require_max || spec.claude_config_dir.is_some())
            && spec.kind != WorkerKind::ClaudeCode
        {
            return Err("Claude login options apply only to claude_code workers".into());
        }
        if let Some(dir) = &spec.claude_config_dir {
            if !Path::new(dir).is_absolute() || !Path::new(dir).is_dir() {
                return Err("claude_config_dir must be an existing absolute directory".into());
            }
        }
        if spec.require_max
            && spec
                .env
                .iter()
                .any(|name| name.starts_with("ANTHROPIC_") || name.starts_with("CLAUDE_"))
        {
            return Err("Max runtimes refuse Claude/Anthropic environment overrides; select claude_config_dir instead".into());
        }
        if (spec.require_chatgpt || spec.codex_home.is_some()) && spec.kind != WorkerKind::Codex {
            return Err("Codex login options apply only to codex workers".into());
        }
        if let Some(dir) = &spec.codex_home {
            if !Path::new(dir).is_absolute() || !Path::new(dir).is_dir() {
                return Err("codex_home must be an existing absolute directory".into());
            }
        }
        if spec.require_chatgpt
            && spec.env.iter().any(|name| {
                name.starts_with("OPENAI_")
                    || name.starts_with("CODEX_")
                    || name.starts_with("AZURE_")
            })
        {
            return Err("ChatGPT runtimes refuse OpenAI/Codex/Azure environment overrides; select codex_home instead".into());
        }
        let denied_tools: Vec<String> = spec
            .denied_tools
            .iter()
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty())
            .collect();
        let denied_tools = if spec.kind == WorkerKind::Codex && !denied_tools.is_empty() {
            tracing::warn!(
                denied_tools = ?denied_tools,
                "codex has no deny list flag; ignoring the spec's denied_tools"
            );
            Vec::new()
        } else {
            denied_tools
        };
        Ok(ExternalConfig {
            kind: spec.kind,
            command: PathBuf::from(spec.command.as_deref().unwrap_or(default_command)),
            repos: spec.repos.iter().map(|r| r.trim().to_string()).collect(),
            model: spec.model.clone(),
            claude_config_dir: spec.claude_config_dir.as_ref().map(PathBuf::from),
            require_max: spec.require_max,
            codex_home: spec.codex_home.as_ref().map(PathBuf::from),
            require_chatgpt: spec.require_chatgpt,
            allowed_tools: spec.allowed_tools.clone(),
            denied_tools,
            max_turns: spec.max_turns.unwrap_or(30),
            permission_mode: spec
                .permission_mode
                .clone()
                .unwrap_or_else(|| "acceptEdits".to_string()),
            timeout: Duration::from_secs(spec.timeout_seconds.unwrap_or(1_800)),
            concurrency: spec.concurrency.unwrap_or(1).max(1),
            env: spec.env.clone(),
            data_dir: data_dir.to_path_buf(),
            skills_dir: data_dir.join("skills"),
            retention: Retention::default(),
        })
    }
}

/// The `claude_code` and `codex` worker kinds: an agent CLI run headless
/// per brief.
pub struct ExternalWorker {
    name: String,
    config: ExternalConfig,
    max_runtime: Option<rustykrab_providers::ClaudeMaxRuntime>,
    codex_runtime: Option<rustykrab_providers::CodexChatGptRuntime>,
    /// What each run spent, by the brief's run id ([`Worker::usage`]).
    usage: Mutex<HashMap<String, RunUsage>>,
    /// The commands the last run of each item was seen to run, handed from
    /// reading the output to attesting the report.
    commands: Mutex<HashMap<String, Vec<String>>>,
    /// Where its live runs' process groups are recorded.
    groups: Arc<RunGroups>,
}

/// What one invocation printed, read.
#[derive(Debug, Default, PartialEq)]
struct Transcript {
    /// The agent's final message, if it got that far.
    final_message: Option<String>,
    /// Shell commands it ran, in order.
    commands: Vec<String>,
    usage: RunUsage,
    /// Set when the agent's own result says it stopped on an error.
    failure: Option<RunFailure>,
    /// Claude Code's session id, when it printed one.
    session: Option<String>,
}

impl ExternalWorker {
    pub fn new(name: impl Into<String>, config: ExternalConfig) -> ExternalWorker {
        ExternalWorker {
            name: name.into(),
            max_runtime: config.require_max.then(|| {
                rustykrab_providers::ClaudeMaxRuntime::new(
                    config.command.clone(),
                    config.claude_config_dir.clone(),
                )
            }),
            codex_runtime: config.require_chatgpt.then(|| {
                rustykrab_providers::CodexChatGptRuntime::new(
                    config.command.clone(),
                    config.codex_home.clone(),
                )
            }),
            config,
            usage: Mutex::new(HashMap::new()),
            commands: Mutex::new(HashMap::new()),
            groups: RunGroups::global(),
        }
    }

    /// Record live runs' process groups in `groups` instead of the global
    /// set.
    pub fn with_groups(mut self, groups: Arc<RunGroups>) -> ExternalWorker {
        self.groups = groups;
        self
    }

    pub fn config(&self) -> &ExternalConfig {
        &self.config
    }

    fn runs_root(&self) -> PathBuf {
        self.config
            .data_dir
            .join("workers")
            .join(&self.name)
            .join("runs")
    }

    /// Whether the command resolves to a file, on `PATH` when it is a bare
    /// name.
    fn command_found(&self) -> bool {
        let cmd = &self.config.command;
        if cmd.components().count() > 1 {
            return cmd.is_file();
        }
        std::env::var_os("PATH")
            .is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join(cmd).is_file()))
    }

    /// Whether the brief's workspace is in one of this worker's
    /// repositories.
    fn owns(&self, repo: &Path) -> bool {
        let canon = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
        let want = canon(repo);
        self.config
            .repos
            .iter()
            .any(|r| Path::new(r) == repo || canon(Path::new(r)) == want)
    }

    /// Read what the agent printed into the result contract, or the typed
    /// reason there is none; record what it spent and the commands it ran,
    /// adding to what an earlier invocation of the same run recorded when
    /// `resumed`. Also returns the session to resume when a Claude Code run
    /// stopped at its turn cap and printed one.
    fn read_output(
        &self,
        brief: &Brief,
        run: &str,
        output: &std::process::Output,
        last: &Path,
        resumed: bool,
    ) -> (Result<ResultReport>, Option<String>) {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let mut transcript = match self.config.kind {
            WorkerKind::Codex => read_codex(&stdout),
            _ => read_claude(&stdout),
        };
        if self.config.kind == WorkerKind::Codex {
            if let Ok(text) = std::fs::read_to_string(last) {
                if !text.trim().is_empty() {
                    transcript.final_message = Some(text);
                }
            }
        }
        {
            let mut usage = self.usage.lock().unwrap_or_else(|e| e.into_inner());
            let entry = usage.entry(run.to_string()).or_default();
            if resumed {
                entry.tokens += transcript.usage.tokens;
                entry.iterations = entry.iterations.saturating_add(transcript.usage.iterations);
            } else {
                *entry = transcript.usage;
            }
        }
        {
            let mut commands = self.commands.lock().unwrap_or_else(|e| e.into_inner());
            let ran = std::mem::take(&mut transcript.commands);
            if resumed {
                commands.entry(brief.item.clone()).or_default().extend(ran);
            } else {
                commands.insert(brief.item.clone(), ran);
            }
        }
        if self
            .max_runtime
            .as_ref()
            .is_some_and(|runtime| runtime.observe_quota(&output.stdout))
        {
            return (
                Err(Error::ModelRateLimit(
                    "Claude Max usage limit reached; no API fallback attempted".into(),
                )),
                None,
            );
        }
        if self
            .codex_runtime
            .as_ref()
            .is_some_and(|runtime| runtime.observe_quota(&output.stdout))
        {
            return (
                Err(Error::ModelRateLimit(
                    "Codex subscription usage limit reached; no API fallback attempted".into(),
                )),
                None,
            );
        }
        if let Some(failure) = transcript.failure.take() {
            let resume = match failure {
                RunFailure::Budget {
                    budget: BudgetKind::Iterations,
                    ..
                } if self.config.kind == WorkerKind::ClaudeCode => transcript.session.take(),
                _ => None,
            };
            return (Err(failure.into_error()), resume);
        }
        let outcome = match &transcript.final_message {
            Some(text) => parse_contract(text).map_err(|why| {
                RunFailure::Model {
                    problem: ProviderProblem::Format,
                    detail: why,
                }
                .into_error()
            }),
            None if !output.status.success() => Err(process_failure(output.status.code(), &stderr)),
            None => Err(RunFailure::Model {
                problem: ProviderProblem::Empty,
                detail: format!(
                    "{} exited without a final message",
                    self.config.command.display()
                ),
            }
            .into_error()),
        };
        (outcome, None)
    }

    /// The turns a Claude Code run gets: the worker's cap, or the item's
    /// iteration budget when that is smaller.
    fn turn_limit(&self, brief: &Brief) -> u32 {
        self.config
            .max_turns
            .min(brief.budget.iterations.max(1))
            .max(1)
    }

    /// The process for one run, or with `resume`, the Claude Code session
    /// to continue for [`RESUME_TURNS`] turns.
    fn command(
        &self,
        prompt: &str,
        dir: &Path,
        brief: &Brief,
        last: &Path,
        resume: Option<&str>,
    ) -> tokio::process::Command {
        let c = &self.config;
        let mut cmd = tokio::process::Command::new(&c.command);
        let build = is_tool_build(brief);
        let review = brief.kind == rustykrab_core::work::WorkKind::Research
            && brief
                .artifact_refs
                .iter()
                .any(|r| r.kind == rustykrab_core::dream_review::REVIEW_ONLY && r.value == "true");
        match c.kind {
            WorkerKind::Codex => {
                if c.require_chatgpt {
                    cmd.arg("--no-daemon");
                }
                cmd.arg("exec")
                    .arg("--json")
                    .arg("--skip-git-repo-check")
                    .arg("--cd")
                    .arg(dir)
                    .arg("--output-last-message")
                    .arg(last);
                if c.require_chatgpt {
                    cmd.args([
                        "--ignore-user-config",
                        "--ephemeral",
                        "-c",
                        "forced_login_method=\"chatgpt\"",
                        "-c",
                        "model_provider=\"openai\"",
                        "-c",
                        "web_search=\"disabled\"",
                    ]);
                }
                if review {
                    cmd.args(["--sandbox", "read-only", "-c", "approval_policy=\"never\""]);
                } else if c.require_chatgpt {
                    cmd.arg("--approve-for-me");
                } else {
                    cmd.args(["--sandbox", "workspace-write"]);
                }
                if let Some(model) = &c.model {
                    cmd.args(["--model", model]);
                }
                if build {
                    cmd.arg("--add-dir").arg(&c.skills_dir);
                }
                cmd.arg(prompt);
            }
            _ => {
                let turns = match resume {
                    Some(_) => RESUME_TURNS,
                    None => self.turn_limit(brief),
                }
                .to_string();
                let allowed = if c.allowed_tools.is_empty() {
                    CLAUDE_DEFAULT_TOOLS.join(",")
                } else {
                    c.allowed_tools.join(",")
                };
                cmd.arg("-p").arg(prompt);
                if review {
                    cmd.args(["--tools", ""]);
                }
                if let Some(session) = resume {
                    cmd.args(["--resume", session]);
                }
                cmd.args(["--output-format", "stream-json", "--verbose"])
                    .args(["--max-turns", &turns])
                    .args(["--permission-mode", &c.permission_mode])
                    .args(["--allowedTools", &allowed])
                    .args(["--disallowedTools", &denied_list(&c.denied_tools)]);
                if let Some(model) = &c.model {
                    cmd.args(["--model", model]);
                }
                if build {
                    cmd.arg("--add-dir").arg(&c.skills_dir);
                }
            }
        }
        cmd.current_dir(dir)
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        // Its own group, so the whole tree it starts can be ended at once.
        #[cfg(unix)]
        cmd.process_group(0);
        let own: &[&str] = match c.kind {
            WorkerKind::Codex => &CODEX_ENV,
            _ => &CLAUDE_ENV,
        };
        for name in BASE_ENV.iter().chain(own).copied() {
            if let Some(v) = std::env::var_os(name) {
                cmd.env(name, v);
            }
        }
        for name in &c.env {
            if let Some(v) = std::env::var_os(name) {
                cmd.env(name, v);
            }
        }
        if let Some(runtime) = &self.max_runtime {
            runtime.isolate(&mut cmd);
            // Profile settings may contain apiKeyHelper/provider overrides.
            // Max-only runtimes use native tools but no inherited MCP/settings.
            cmd.args([
                "--setting-sources",
                "",
                "--strict-mcp-config",
                "--mcp-config",
                "{\"mcpServers\":{}}",
            ]);
            for name in &c.env {
                if let Some(value) = std::env::var_os(name) {
                    cmd.env(name, value);
                }
            }
        } else if let Some(dir) = &c.claude_config_dir {
            cmd.env("CLAUDE_CONFIG_DIR", dir);
        }
        if let Some(runtime) = &self.codex_runtime {
            runtime.isolate(&mut cmd);
            for name in &c.env {
                if let Some(value) = std::env::var_os(name) {
                    cmd.env(name, value);
                }
            }
        } else if let Some(dir) = &c.codex_home {
            cmd.env("CODEX_HOME", dir);
        }
        cmd.env("RUSTYKRAB_DATA_DIR", &c.data_dir)
            .env("RUSTYKRAB_SKILLS_DIR", &c.skills_dir);
        cmd
    }

    /// Start `cmd` in a process group recorded in [`RunGroups`] and wait
    /// for it, up to `limit`. Whatever it left running in its group is
    /// killed when this returns. Its output is read as it arrives, so a run
    /// its wall limit ended still hands back what it had printed (the
    /// second value, only then). Also says whether daemon shutdown
    /// ([`RunGroups::terminate_all`]) ended the group, which must be read
    /// before the group is let go.
    async fn execute(
        &self,
        mut cmd: tokio::process::Command,
        limit: Duration,
    ) -> (
        Result<std::process::Output>,
        Option<std::process::Output>,
        bool,
    ) {
        let mut child = match cmd.spawn() {
            Ok(child) => child,
            Err(e) => {
                let why = format!("cannot start {}: {e}", self.config.command.display());
                return (Err(process_failure(None, &why)), None, false);
            }
        };
        // The agent leads its own group, so the group id is its pid.
        let group = child
            .id()
            .and_then(|pid| i32::try_from(pid).ok())
            .map(|pid| self.groups.enter(pid));
        let (stdout, mut stdout_task) = capture(child.stdout.take());
        let (stderr, mut stderr_task) = capture(child.stderr.take());
        let finished = tokio::time::timeout(limit, async {
            let status = child.wait().await;
            let _ = (&mut stdout_task).await;
            let _ = (&mut stderr_task).await;
            status
        })
        .await;
        if finished.is_err() {
            let _ = child.start_kill();
        }
        let interrupted = group.as_ref().is_some_and(GroupGuard::interrupted);
        // Whatever the agent left running in its group goes with it, which
        // also closes the pipes it held.
        drop(group);
        let taken = |buf: &Arc<Mutex<Vec<u8>>>| {
            std::mem::take(&mut *buf.lock().unwrap_or_else(|e| e.into_inner()))
        };
        match finished {
            Ok(Ok(status)) => {
                let output = std::process::Output {
                    status,
                    stdout: taken(&stdout),
                    stderr: taken(&stderr),
                };
                (Ok(output), None, interrupted)
            }
            Ok(Err(e)) => (
                Err(process_failure(None, &e.to_string())),
                None,
                interrupted,
            ),
            Err(_) => {
                // Let the readers drain what is still in the pipes, briefly.
                let _ = tokio::time::timeout(DRAIN_GRACE, async {
                    let _ = (&mut stdout_task).await;
                    let _ = (&mut stderr_task).await;
                })
                .await;
                stdout_task.abort();
                stderr_task.abort();
                let partial = child.wait().await.ok().map(|status| std::process::Output {
                    status,
                    stdout: taken(&stdout),
                    stderr: taken(&stderr),
                });
                let failure = RunFailure::Budget {
                    budget: BudgetKind::Wall,
                    detail: format!(
                        "{}s wall budget spent before {} returned",
                        limit.as_secs(),
                        self.config.command.display()
                    ),
                }
                .into_error();
                (Err(failure), partial, interrupted)
            }
        }
    }

    /// Resume a Claude Code session that stopped at its turn cap and ask
    /// only for the result contract. `None` when there is no time left, the
    /// resume did not return one, or a run in a worktree reported no
    /// commit; beside it, whether daemon shutdown ended the resume.
    async fn recover(
        &self,
        brief: &Brief,
        run: &str,
        session: &str,
        dir: &Path,
        last: &Path,
        limit: Duration,
    ) -> (Option<ResultReport>, bool) {
        if limit.is_zero() {
            return (None, false);
        }
        let cmd = self.command(&resume_prompt(), dir, brief, last, Some(session));
        let (executed, partial, interrupted) = self.execute(cmd, limit).await;
        let recovered = match executed {
            Ok(output) => self.read_output(brief, run, &output, last, true).0,
            Err(e) => {
                if let Some(partial) = &partial {
                    let _ = self.read_output(brief, run, partial, last, true);
                }
                Err(e)
            }
        };
        let report = match recovered {
            // A worktree run that committed nothing has nothing to recover:
            // it stays the budget/iterations failure it already was.
            Ok(report) if brief.workspace.is_some() && report.commit.is_none() => {
                tracing::warn!(
                    worker = %self.name,
                    item = %brief.item,
                    "resume after the turn cap reported no commit"
                );
                None
            }
            Ok(mut report) => {
                report.known_limits.push(format!(
                    "recovered after the turn cap: the run stopped at its {}-turn limit and this \
                     report came from resuming its session for at most {RESUME_TURNS} turns",
                    self.turn_limit(brief)
                ));
                Some(report)
            }
            Err(e) => {
                tracing::warn!(
                    worker = %self.name,
                    item = %brief.item,
                    error = %e,
                    "resume after the turn cap returned no contract"
                );
                None
            }
        };
        (report, interrupted)
    }
}

/// What a resume after the turn cap is asked.
fn resume_prompt() -> String {
    format!(
        "You ran out of turns before returning the result contract. Do no more work and run \
         nothing. Return the result contract for the work already committed on this branch: \
         commit is the sha of your last commit, or null if you committed nothing, and \
         changed_paths are exactly the files your commits change. End with one JSON object and \
         nothing after it:\n{CONTRACT_SHAPE}\n"
    )
}

/// Whether the brief is a capability build of a tool: its facet says
/// build and its need is a tool.
fn is_tool_build(brief: &Brief) -> bool {
    built_tool(brief.capability, &brief.artifact_refs).is_some()
}

fn policy_failure(detail: String) -> Error {
    RunFailure::Policy {
        stop: PolicyStop::Scope,
        detail,
    }
    .into_error()
}

fn process_failure(code: Option<i32>, stderr: &str) -> Error {
    RunFailure::Process {
        code,
        stderr_tail: tail(stderr, STDERR_TAIL),
    }
    .into_error()
}

#[async_trait]
impl Worker for ExternalWorker {
    fn name(&self) -> &str {
        &self.name
    }

    fn kind(&self) -> WorkerKind {
        self.config.kind
    }

    /// No RustyKrab tools: an external agent works with its own. What it
    /// may write is the repositories it was added for.
    fn capabilities(&self) -> WorkerCapabilities {
        let default_model = match self.config.kind {
            WorkerKind::Codex => "codex",
            _ => "claude-code",
        };
        WorkerCapabilities {
            models: vec![self
                .config
                .model
                .clone()
                .unwrap_or_else(|| default_model.to_string())],
            tools: Vec::new(),
            mcp_servers: Vec::new(),
            repos: self.config.repos.clone(),
            machine: None,
            writable_resources: self
                .config
                .repos
                .iter()
                .map(|r| format!("{REPO_PREFIX}{r}"))
                .collect(),
        }
    }

    fn concurrency(&self) -> usize {
        self.config.concurrency
    }

    /// The command resolves and every repository is there.
    fn healthy(&self) -> bool {
        self.command_found()
            && self.config.repos.iter().all(|r| Path::new(r).is_dir())
            && self
                .max_runtime
                .as_ref()
                .is_none_or(|runtime| runtime.health_error().is_none())
            && self
                .codex_runtime
                .as_ref()
                .is_none_or(|runtime| runtime.health_error().is_none())
    }

    fn unhealthy_reason(&self) -> Option<String> {
        if !self.command_found() {
            return Some(format!(
                "command `{}` not found",
                self.config.command.display()
            ));
        }
        self.config
            .repos
            .iter()
            .find(|r| !Path::new(r).is_dir())
            .map(|r| format!("repository {r} is not a directory"))
            .or_else(|| {
                self.max_runtime
                    .as_ref()
                    .and_then(|runtime| runtime.health_error())
            })
            .or_else(|| {
                self.codex_runtime
                    .as_ref()
                    .and_then(|runtime| runtime.health_error())
            })
    }

    fn runtime_status(&self) -> Option<Value> {
        self.max_runtime
            .as_ref()
            .and_then(|runtime| serde_json::to_value(runtime.status()).ok())
            .or_else(|| {
                self.codex_runtime
                    .as_ref()
                    .and_then(|runtime| serde_json::to_value(runtime.status()).ok())
            })
    }

    async fn refresh(&self) -> bool {
        if let Some(runtime) = &self.max_runtime {
            if !runtime.needs_refresh() {
                return false;
            }
            let _ = runtime.verify_login(true).await;
            return true;
        }
        if let Some(runtime) = &self.codex_runtime {
            if !runtime.needs_refresh() {
                return false;
            }
            let _ = runtime.verify_login(true).await;
            return true;
        }
        false
    }

    /// Tokens and turns from the agent's own event stream, and the wall
    /// time of its process.
    fn usage(&self, run: &str) -> Option<RunUsage> {
        self.usage
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(run)
            .copied()
    }

    async fn run(&self, brief: Brief) -> Result<ResultReport> {
        if let Some(runtime) = &self.max_runtime {
            runtime.verify_login(true).await?;
        }
        if let Some(runtime) = &self.codex_runtime {
            runtime.verify_login(true).await?;
        }
        let run_id = brief
            .run
            .clone()
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        // Where it runs: the item's workspace, or a scratch directory.
        let (dir, workspace) = match &brief.workspace {
            Some(ws) => {
                if !self.owns(&ws.repo) {
                    return Err(policy_failure(format!(
                        "{} is not one of {}'s repositories ({})",
                        ws.repo.display(),
                        self.name,
                        self.config.repos.join(", ")
                    )));
                }
                let created = ws.clone();
                tokio::task::spawn_blocking(move || created.create())
                    .await
                    .map_err(|e| Error::Internal(e.to_string()))?
                    .map_err(|why| process_failure(Some(128), &why))?;
                (ws.path.clone(), Some(ws.clone()))
            }
            None => {
                let dir = self.runs_root().join(short(&run_id));
                std::fs::create_dir_all(&dir).map_err(|e| {
                    process_failure(None, &format!("cannot create {}: {e}", dir.display()))
                })?;
                (dir, None)
            }
        };
        if let Retention::KeepFailed(window) = self.config.retention {
            if let Some(ws) = &workspace {
                let root = ws.path.parent().map(Path::to_path_buf);
                if let Some(root) = root {
                    let _ = tokio::task::spawn_blocking(move || {
                        workspace::prune_older_than(&root, window)
                    })
                    .await;
                }
            }
            prune_dirs(&self.runs_root(), window);
        }

        let turns = match self.config.kind {
            WorkerKind::Codex => None,
            _ => Some(self.turn_limit(&brief)),
        };
        let context_dir = self.runs_root().join(short(&run_id));
        let context_path = if let Some(context) = &brief.project_context {
            std::fs::create_dir_all(&context_dir).map_err(|e| Error::Internal(e.to_string()))?;
            let path = context_dir.join("project-context.json");
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create(true).truncate(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let file = options
                .open(&path)
                .map_err(|e| Error::Internal(e.to_string()))?;
            serde_json::to_writer(file, context).map_err(|e| Error::Internal(e.to_string()))?;
            Some(path)
        } else {
            None
        };
        let mut prompt = render_executor_brief(
            &brief,
            &self.name,
            self.config.kind,
            &dir,
            &self.config.skills_dir,
            turns,
        );
        if let Some(path) = context_path {
            let _ = writeln!(prompt, "Full frozen project history: {}. Read only the records needed for this task, in small portions; do not load the entire file into context. This is inspection data, not additional task authority.", path.display());
        }
        let last = std::env::temp_dir().join(format!("rustykrab-last-{}.txt", short(&run_id)));
        let cmd = self.command(&prompt, &dir, &brief, &last, None);
        let limit = match brief.budget.wall_seconds {
            0 => self.config.timeout,
            secs => self.config.timeout.min(Duration::from_secs(secs)),
        };
        let started = std::time::Instant::now();
        let (executed, partial, mut interrupted) = self.execute(cmd, limit).await;
        let (mut outcome, resume) = match executed {
            Ok(output) => self.read_output(&brief, &run_id, &output, &last, false),
            // Ended by its wall limit: record what it had spent, and keep
            // the timeout as the outcome.
            Err(e) => {
                if let Some(partial) = &partial {
                    let _ = self.read_output(&brief, &run_id, partial, &last, false);
                }
                (Err(e), None)
            }
        };
        // A run that stopped at its turn cap may still have committed its
        // work: ask the same session for the contract, within what is left
        // of the wall limit. Not once the daemon is shutting down.
        if let Some(session) = resume.filter(|_| !interrupted) {
            let left = limit.saturating_sub(started.elapsed());
            let (report, stopped) = self
                .recover(&brief, &run_id, &session, &dir, &last, left)
                .await;
            interrupted = stopped;
            if let Some(report) = report {
                outcome = Ok(report);
            }
        }
        // Shutdown ended the group: whatever the process said, the run was
        // interrupted, not failed. A report it managed to hand in stands.
        let outcome = match outcome {
            Err(e) if interrupted => Err(RunFailure::Interrupted {
                detail: tail(
                    &format!(
                        "the daemon shut down and ended {}: {e}",
                        self.config.command.display()
                    ),
                    STDERR_TAIL,
                ),
            }
            .into_error()),
            other => other,
        };
        self.usage
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(run_id.clone())
            .or_default()
            .wall_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let _ = std::fs::remove_file(&last);
        let commands = self
            .commands
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&brief.item)
            .unwrap_or_default();
        let outcome = outcome.and_then(|mut report| {
            let review = brief.kind == rustykrab_core::work::WorkKind::Research
                && brief.artifact_refs.iter().any(|r| {
                    r.kind == rustykrab_core::dream_review::REVIEW_ONLY && r.value == "true"
                });
            if review
                && (!report.discovered.is_empty()
                    || report.commit.is_some()
                    || !report.changed_paths.is_empty())
            {
                return Err(RunFailure::Model {
                    problem: ProviderProblem::Format,
                    detail: "Read-only review returned executable follow-ups or code changes"
                        .into(),
                }
                .into_error());
            }
            attest(&mut report, &commands);
            Ok(report)
        });

        // Retention: a run with a result gives its directory back; one
        // without (an interrupted one included) keeps it for diagnosis
        // unless policy says otherwise.
        let keep = outcome.is_err() && matches!(self.config.retention, Retention::KeepFailed(_));
        if !keep {
            let _ = std::fs::remove_dir_all(&context_dir);
            match workspace {
                Some(ws) => {
                    let _ = tokio::task::spawn_blocking(move || ws.remove()).await;
                }
                None => {
                    let _ = std::fs::remove_dir_all(&dir);
                }
            }
        }
        tracing::info!(
            worker = %self.name,
            item = %brief.item,
            ok = outcome.is_ok(),
            commands = commands.len(),
            "external worker run ended"
        );
        outcome
    }
}

/// Replace any `command_run` the model wrote with the commands the adapter
/// saw it run.
fn attest(report: &mut ResultReport, commands: &[String]) {
    report.artifacts.retain(|a| a.kind != COMMAND_RUN);
    for command in commands.iter().take(COMMANDS_MAX) {
        let r = ArtifactRef {
            kind: COMMAND_RUN.to_string(),
            value: command.clone(),
        };
        if !report.artifacts.contains(&r) {
            report.artifacts.push(r);
        }
    }
}

/// Remove directories under `root` older than `window`.
fn prune_dirs(root: &Path, window: Duration) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    let now = std::time::SystemTime::now();
    for entry in entries.flatten() {
        let old = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| now.duration_since(t).ok())
            .is_some_and(|age| age > window);
        if old && entry.path().is_dir() {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

// ── reading what the agent printed ────────────────────────────────────────

/// Claude Code's `stream-json`: one JSON event per line. Tool uses of
/// `Bash` are the commands it ran; the last `result` event is the envelope
/// with the final message, the turn count and the cost. A run ended before
/// that envelope (daemon shutdown) keeps what its assistant turns reported:
/// one turn per distinct message, and that message's usage.
fn read_claude(stdout: &str) -> Transcript {
    let mut t = Transcript::default();
    // Tokens per assistant message id. Claude Code prints one event per
    // content block, each repeating its message's usage, so the last seen
    // stands for the message.
    let mut turns_seen: Vec<(String, u64)> = Vec::new();
    let mut enveloped = false;
    for (n, line) in stdout.lines().enumerate() {
        let Ok(event) = serde_json::from_str::<Value>(line.trim()) else {
            continue;
        };
        if let Some(session) = event["session_id"].as_str().filter(|s| !s.is_empty()) {
            t.session = Some(session.to_string());
        }
        match event["type"].as_str() {
            Some("assistant") => {
                let message = &event["message"];
                let id = message["id"]
                    .as_str()
                    .map_or_else(|| format!("line-{n}"), str::to_string);
                let tokens = claude_tokens(&message["usage"]);
                match turns_seen.iter_mut().find(|(seen, _)| *seen == id) {
                    Some(turn) => turn.1 = tokens,
                    None => turns_seen.push((id, tokens)),
                }
                for block in message["content"].as_array().into_iter().flatten() {
                    if block["type"] == "tool_use" && block["name"] == "Bash" {
                        if let Some(c) = block["input"]["command"].as_str() {
                            t.commands.push(c.to_string());
                        }
                    }
                }
            }
            Some("result") => {
                enveloped = true;
                let turns = event["num_turns"].as_u64().unwrap_or(0);
                t.usage = RunUsage {
                    tokens: claude_tokens(&event["usage"]),
                    iterations: u32::try_from(turns).unwrap_or(u32::MAX),
                    ..RunUsage::default()
                };
                let subtype = event["subtype"].as_str().unwrap_or("success");
                if subtype == "error_max_turns" {
                    t.failure = Some(RunFailure::Budget {
                        budget: BudgetKind::Iterations,
                        detail: format!("claude stopped at its turn cap after {turns} turns"),
                    });
                } else if subtype != "success" || event["is_error"] == true {
                    t.failure = Some(RunFailure::Process {
                        code: None,
                        stderr_tail: tail(
                            &format!(
                                "claude ended with {subtype}: {}",
                                event["result"].as_str().unwrap_or_default()
                            ),
                            STDERR_TAIL,
                        ),
                    });
                }
                t.final_message = event["result"].as_str().map(str::to_string);
            }
            _ => {}
        }
    }
    if !enveloped {
        t.usage = RunUsage {
            tokens: turns_seen.iter().map(|(_, tokens)| tokens).sum(),
            iterations: u32::try_from(turns_seen.len()).unwrap_or(u32::MAX),
            ..RunUsage::default()
        };
    }
    t
}

/// The tokens a Claude Code `usage` object counts.
fn claude_tokens(usage: &Value) -> u64 {
    [
        "input_tokens",
        "output_tokens",
        "cache_creation_input_tokens",
    ]
    .iter()
    .filter_map(|k| usage[k].as_u64())
    .sum()
}

/// `codex exec --json`: one JSON event per line. Command executions are the
/// commands it ran; agent messages are its text, the last one its final
/// message (the `--output-last-message` file wins when it is written).
/// Reads both the `item.*` event shape and the older `msg` one.
/// Codex displays a shell argv as a quoted command string. Decode only an
/// exact `<known shell> -c/-lc <one command>` invocation, never arbitrary
/// strings or compound wrappers. Preserve the command argument byte-for-byte:
/// its quoting and heredocs are the check evidence, not display escapes.
fn codex_command(value: &Value) -> Option<String> {
    let (display, argv) = match value {
        Value::String(display) => (display.clone(), shlex::split(display)),
        Value::Array(parts) => {
            let argv = parts
                .iter()
                .map(|p| p.as_str().map(str::to_owned))
                .collect::<Option<Vec<_>>>()?;
            (argv.join(" "), Some(argv))
        }
        _ => return None,
    };
    if let Some(argv) = argv {
        if argv.len() == 3
            && matches!(
                Path::new(&argv[0]).file_name().and_then(|n| n.to_str()),
                Some("sh" | "bash" | "zsh" | "dash")
            )
            && matches!(argv[1].as_str(), "-c" | "-lc")
        {
            return Some(argv[2].clone());
        }
    }
    Some(display)
}

fn read_codex(stdout: &str) -> Transcript {
    let mut t = Transcript::default();
    let mut turns = 0u32;
    let mut started = 0u32;
    for line in stdout.lines() {
        let Ok(event) = serde_json::from_str::<Value>(line.trim()) else {
            continue;
        };
        let item = &event["item"];
        let msg = &event["msg"];
        match (
            event["type"].as_str(),
            item["type"].as_str(),
            msg["type"].as_str(),
        ) {
            (Some("item.completed"), Some("command_execution"), _) => {
                if let Some(c) = codex_command(&item["command"]) {
                    t.commands.push(c);
                }
            }
            (Some("item.completed"), Some("agent_message"), _) => {
                if let Some(text) = item["text"].as_str() {
                    t.final_message = Some(text.to_string());
                }
            }
            (Some("turn.started"), _, _) => started += 1,
            (Some("turn.completed"), _, _) => {
                turns += 1;
                let usage = &event["usage"];
                t.usage.tokens += ["input_tokens", "output_tokens"]
                    .iter()
                    .filter_map(|k| usage[k].as_u64())
                    .sum::<u64>();
            }
            (Some("turn.failed") | Some("error"), _, _) => {
                let why = event["error"]["message"]
                    .as_str()
                    .or_else(|| event["message"].as_str())
                    .unwrap_or("codex reported an error");
                t.failure = Some(RunFailure::Process {
                    code: None,
                    stderr_tail: tail(why, STDERR_TAIL),
                });
            }
            (_, _, Some("exec_command_begin")) => {
                let Some(command) = codex_command(&msg["command"]) else {
                    continue;
                };
                t.commands.push(command);
            }
            (_, _, Some("agent_message")) => {
                if let Some(text) = msg["message"].as_str() {
                    t.final_message = Some(text.to_string());
                }
            }
            _ => {}
        }
    }
    t.usage.iterations = turns.max(started);
    t
}

/// The section 5 result contract from an agent's final message: the whole
/// message as JSON, else a fenced ```json block, else the last balanced
/// `{...}` in it. A worker's `error` gets the fields only the controller
/// fills (fingerprint, observer), and a class, subclass or blocked reason
/// this build does not know reads as `unknown/unclassified` or
/// `needs_decision`, so the controller classifies it rather than the run
/// failing to parse. A question given as a plain string reads as a question
/// with that text, an empty class and no options.
pub fn parse_contract(text: &str) -> std::result::Result<ResultReport, String> {
    let candidates = contract_candidates(text);
    let mut last_error = String::from("no JSON object in the final message");
    for raw in candidates {
        let Ok(mut value) = serde_json::from_str::<Value>(&raw) else {
            continue;
        };
        if !value.is_object() {
            continue;
        }
        if let Err(e) = sanitise(&mut value) {
            last_error = format!("the result contract did not parse: {e}");
            continue;
        }
        match serde_json::from_value::<ResultReport>(value) {
            Ok(report) => return Ok(report),
            Err(e) => last_error = format!("the result contract did not parse: {e}"),
        }
    }
    Err(format!("{last_error}: {}", tail(text.trim(), 300)))
}

fn contract_candidates(text: &str) -> Vec<String> {
    let mut out = vec![text.trim().to_string()];
    let mut rest = text;
    while let Some(start) = rest.find("```") {
        let after = &rest[start + 3..];
        let body_start = after.find('\n').map_or(0, |n| n + 1);
        let Some(end) = after[body_start..].find("```") else {
            break;
        };
        out.push(after[body_start..body_start + end].trim().to_string());
        rest = &after[body_start + end + 3..];
    }
    // The last balanced object, scanning back from the last `}`.
    if let Some(close) = text.rfind('}') {
        let bytes = text.as_bytes();
        let mut depth = 0i32;
        let mut in_string = false;
        let mut i = close as isize;
        while i >= 0 {
            let c = bytes[i as usize];
            if c == b'"' && (i == 0 || bytes[i as usize - 1] != b'\\') {
                in_string = !in_string;
            } else if !in_string {
                if c == b'}' {
                    depth += 1;
                } else if c == b'{' {
                    depth -= 1;
                    if depth == 0 {
                        out.push(text[i as usize..=close].to_string());
                        break;
                    }
                }
            }
            i -= 1;
        }
    }
    out.reverse();
    out
}

fn sanitise(value: &mut Value) -> std::result::Result<(), String> {
    if value.get("summary").is_none() {
        value["summary"] = Value::String(String::new());
    }
    if let Some(error) = value.get_mut("error").filter(|e| e.is_object()) {
        let known = |key: &str| -> bool {
            let raw = Value::String(error[key].as_str().unwrap_or_default().to_string());
            match key {
                "class" => serde_json::from_value::<rustykrab_core::work::ErrorClass>(raw).is_ok(),
                _ => serde_json::from_value::<rustykrab_core::work::ErrorSubclass>(raw).is_ok(),
            }
        };
        if !known("class") || !known("subclass") {
            error["class"] = Value::String("unknown".into());
            error["subclass"] = Value::String("unclassified".into());
        }
        if error.get("fingerprint").is_none() {
            error["fingerprint"] = Value::String(String::new());
        }
        if error.get("observed_by").is_none() {
            error["observed_by"] = Value::String("worker_report".into());
        }
        if error.get("detail").is_none() {
            error["detail"] = Value::String(String::new());
        }
    }
    // A question given as a plain string is that text with no class or
    // options. Anything else that is not an object is refused here, since
    // serde would otherwise read an array as the struct's fields in order.
    if let Some(Value::Array(questions)) = value.get_mut("questions") {
        for question in questions.iter_mut() {
            match question {
                Value::String(text) => {
                    *question = serde_json::json!({ "text": std::mem::take(text) });
                }
                Value::Object(_) => {}
                other => {
                    return Err(format!(
                        "a question must be an object or a string, not {other}"
                    ))
                }
            }
        }
    }
    let summary = value["summary"].as_str().map(str::to_string);
    if let Some(blocked) = value.get_mut("blocked").filter(|b| b.is_object()) {
        let reason = Value::String(blocked["reason"].as_str().unwrap_or_default().to_string());
        if serde_json::from_value::<rustykrab_core::work::BlockedReason>(reason).is_err() {
            blocked["reason"] = Value::String("needs_decision".into());
        }
        // A worker that puts its question under another key, or leaves it
        // out, still has it read: the first non-blank string among the
        // blocked object's other fields (the usual names first), else the
        // report's summary.
        let text = |v: &Value| {
            v.as_str()
                .filter(|s| !s.trim().is_empty())
                .map(str::to_string)
        };
        if blocked.get("detail").and_then(text).is_none() {
            let fields = blocked.as_object().into_iter().flatten();
            let usual = ["question", "need", "what_you_need", "message", "text"];
            let found = usual
                .iter()
                .find_map(|key| blocked.get(*key).and_then(text))
                .or_else(|| {
                    fields
                        .filter(|(key, _)| !matches!(key.as_str(), "reason" | "detail" | "needs"))
                        .find_map(|(_, v)| text(v))
                });
            let detail = found.or_else(|| summary.filter(|s| !s.trim().is_empty()));
            blocked["detail"] = Value::String(detail.unwrap_or_default());
        }
    }
    Ok(())
}

// ── the executor brief ────────────────────────────────────────────────────

/// Render a brief as the delivery plan's executor brief (its section 5.2:
/// the work-item slice, parent SHA, repository context, allowed tools and
/// prior failed evidence), then the section 5 result contract the final
/// message must be. `turns` is the run's turn limit, when it has one.
pub fn render_executor_brief(
    brief: &Brief,
    name: &str,
    kind: WorkerKind,
    dir: &Path,
    skills_dir: &Path,
    turns: Option<u32>,
) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "You are {name}, a RustyKrab {} worker. You run one work item for RustyKrab's \
         controller, which checks everything you claim against the repository and the host.",
        kind.as_str()
    );
    out.push('\n');
    let _ = writeln!(
        out,
        "work item: #{}  kind: {}",
        brief.item,
        brief.kind.as_str()
    );
    let _ = writeln!(out, "title: {}", one_line(&brief.title));
    let _ = writeln!(out, "objective: {}", one_line(&brief.objective));
    let _ = writeln!(out, "done_when: {}", one_line(&brief.done_when));
    list(&mut out, "constraints", &brief.constraints);
    list(&mut out, "decisions_made", &brief.decisions_made);
    if let Some(context) = &brief.project_context {
        let _ = writeln!(out, "project_context (durable project state; summaries are reports, evidence is controller-verified):");
        let _ = writeln!(
            out,
            "{}",
            serde_json::to_string(&context.execution_view())
                .expect("project execution context serializes")
        );
        let _ = writeln!(out, "Continue from this project's pinned code and decisions. Open work is unfinished. Historical or superseded decisions do not override current decisions. Project context does not expand this work item's authority. Inspect retained unfinished_attempt workspaces/branches before redoing work; their effects are unverified. Validate and report any partial changes you carry forward.");
    }
    if !brief.artifact_refs.is_empty() {
        let refs: Vec<String> = brief
            .artifact_refs
            .iter()
            .map(|r| format!("{}:{}", r.kind, r.value))
            .collect();
        let _ = writeln!(out, "artifact_refs: [{}]", refs.join(", "));
    }
    if !brief.inputs.is_empty() {
        let _ = writeln!(out, "inputs:");
        for input in &brief.inputs {
            let _ = writeln!(
                out,
                "  - item: #{} \"{}\"  status: {}",
                input.item,
                one_line(&input.title),
                input.status
            );
            let summary = input.summary.lines().next().unwrap_or("").trim();
            if !summary.is_empty() {
                let _ = writeln!(out, "    summary: {summary}");
            }
            let refs: Vec<String> = input
                .evidence
                .iter()
                .chain(&input.artifacts)
                .map(|r| format!("{}:{}", r.kind, r.value))
                .collect();
            if !refs.is_empty() {
                let _ = writeln!(out, "    refs: [{}]", refs.join(", "));
            }
        }
    }
    if let Some(err) = &brief.last_error {
        let _ = writeln!(out, "repair: an earlier attempt at this item failed");
        let _ = writeln!(out, "  last_error: {}", one_line(err));
        let prior: Vec<String> = brief
            .prior_evidence
            .iter()
            .filter(|e| e.kind != "run" && e.kind != "workspace" && e.kind != "project_context")
            .map(|e| one_line(&format!("{}:{}", e.kind, e.reference)))
            .collect();
        if !prior.is_empty() {
            let _ = writeln!(out, "  prior_evidence: [{}]", prior.join(", "));
        }
    }
    match &brief.workspace {
        Some(ws) => {
            let _ = writeln!(out, "repository: {}", ws.repo.display());
            let _ = writeln!(
                out,
                "worktree: {} (your working directory: an isolated checkout)",
                ws.path.display()
            );
            let _ = writeln!(out, "branch: {}", ws.branch);
            let _ = writeln!(out, "parent commit: {}", ws.base);
        }
        None => {
            let _ = writeln!(out, "working directory: {} (scratch)", dir.display());
        }
    }
    if let Some(tool) = built_tool(brief.capability, &brief.artifact_refs) {
        let _ = writeln!(
            out,
            "build: write the `{tool}` tool as a RustyKrab skill at {}/{tool}/SKILL.md: front \
             matter between `---` lines with `name = \"{tool}\"` and a one-line \
             `description = \"...\"`, then a body that says how to use it. RustyKrab loads \
             skills written while it runs; the controller checks that a tool named `{tool}` \
             exists before it counts this done.",
            skills_dir.display()
        );
    }
    out.push('\n');
    out.push_str("How to finish:\n");
    if let Some(turns) = turns {
        let _ = writeln!(
            out,
            "- You have {turns} turns in all. Commit and return the result contract with a few \
             turns to spare; a run that reaches the limit without it loses its work."
        );
    }
    if let Some(ws) = &brief.workspace {
        let _ = writeln!(
            out,
            "- Commit your change on {} in this worktree. Do not push, switch branches, or \
             touch any other checkout.",
            ws.branch
        );
        out.push_str(
            "- changed_paths must be exactly the files your commit changes, relative to the \
             repository root; the controller compares them with git.\n",
        );
    }
    out.push_str(
        "- Keep notes or task lists of your own if they help; nothing but the JSON below is \
         read.\n\
         - Each checks_run entry is a shell command you actually ran via Bash, copied \
         exactly, with no results or descriptions. Checks done with Grep, Read or Glob belong \
         in the summary or known_limits, not checks_run.\n\
         - Your summary becomes a notification that links to the detailed execution. Write \
         one or two sentences stating the outcome or what needs the user's attention. Put \
         supporting detail in artifacts, changed_paths, checks_run and known_limits.\n\
         - Work in a small, verifiable execution slice. Finish this item's done_when, then \
         create post-tasks in \"discovered\" for further work instead of continuing through \
         the whole project. Each draft needs a tmp name, precise objective and done_when, \
         required resources, and pointers to the committed results. Use inputs_from plus \
         a blocks edge for tasks that need an earlier result; independent tasks may run in \
         parallel. Keep tasks sharing a writable resource ordered. Another agent picks up \
         the next task after your run ends. Do not claim an unfinished done_when as complete.\n\
         - If you cannot finish, set \"blocked\" or \"error\" (class, subclass, detail) \
         instead of guessing. ",
    );
    out.push_str(&rustykrab_core::work::BLOCKED_SHAPE_GUIDANCE);
    out.push_str("\nEnd with one JSON object and nothing after it, the result contract:\n");
    out.push_str(CONTRACT_SHAPE);
    out.push('\n');
    out
}

fn list(out: &mut String, label: &str, entries: &[String]) {
    if entries.is_empty() {
        return;
    }
    let _ = writeln!(out, "{label}:");
    for entry in entries {
        let _ = writeln!(out, "  - {}", one_line(entry));
    }
}

fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn short(id: &str) -> String {
    id.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(12)
        .collect()
}

/// The last `max` characters of `text`.
fn tail(text: &str, max: usize) -> String {
    let count = text.chars().count();
    if count <= max {
        return text.to_string();
    }
    let skip = count - max;
    format!("...{}", text.chars().skip(skip).collect::<String>())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    use rustykrab_control::errors::{classify, Context};
    use rustykrab_control::worker::run_failure_input;
    use rustykrab_control::workspace::Workspace;
    use rustykrab_core::work::{Budget, ErrorClass, ErrorSubclass, WorkKind};

    fn git(dir: &Path, args: &[&str]) -> String {
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
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    struct Fixture {
        repo: tempfile::TempDir,
        data: tempfile::TempDir,
        bin: tempfile::TempDir,
    }

    impl Fixture {
        fn new() -> Fixture {
            let repo = tempfile::tempdir().unwrap();
            git(repo.path(), &["init", "--initial-branch=main"]);
            std::fs::create_dir_all(repo.path().join("src")).unwrap();
            std::fs::write(repo.path().join("src/lib.rs"), "pub fn a() {}\n").unwrap();
            git(repo.path(), &["add", "."]);
            git(
                repo.path(),
                &[
                    "-c",
                    "user.name=t",
                    "-c",
                    "user.email=t@x.invalid",
                    "commit",
                    "-q",
                    "--no-gpg-sign",
                    "-m",
                    "init",
                ],
            );
            Fixture {
                repo,
                data: tempfile::tempdir().unwrap(),
                bin: tempfile::tempdir().unwrap(),
            }
        }

        /// Write an executable script as the agent's command.
        fn agent(&self, name: &str, script: &str) -> PathBuf {
            let path = self.bin.path().join(name);
            std::fs::write(&path, script).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
            path
        }

        fn worker(&self, kind: WorkerKind, command: PathBuf) -> ExternalWorker {
            let spec = WorkerSpec {
                kind,
                repos: vec![self.repo.path().display().to_string()],
                command: Some(command.display().to_string()),
                timeout_seconds: Some(20),
                ..WorkerSpec::default()
            };
            let mut config = ExternalConfig::from_spec(&spec, self.data.path()).unwrap();
            config.retention = Retention::Always;
            ExternalWorker::new("pinch", config)
        }

        fn workspace(&self) -> Workspace {
            let base = git(self.repo.path(), &["rev-parse", "HEAD"]);
            Workspace::plan(
                &self.data.path().join("worktrees"),
                self.repo.path(),
                &base,
                "item-1",
                "run-1",
            )
        }
    }

    #[tokio::test]
    async fn native_workers_inspect_full_history_without_inlining_it_into_the_prompt() {
        use rustykrab_control::handoff::{ProjectContext, ProjectWork};
        use rustykrab_core::work::{Evidence, Status};
        let f = Fixture::new();
        let id = "af054c9a-0e4a-4d92-9990-b33d7e2592bc";
        let at = "2026-10-10T00:00:00Z";
        let mut context: ProjectContext = serde_json::from_value(serde_json::json!({
            "snapshot": {
                "project": {"id": id, "repository_id": null, "title": "Fixture", "status": "active",
                    "judgment_policy": {"statement": "Keep the user's constraints", "delegated_scopes": [], "reserved_decisions": []},
                    "canonical_conversation_id": null, "created_at": at, "updated_at": at},
                "revision": {"id": "a".repeat(64), "request_id": "fixture", "project_id": id,
                    "parent_revision": null, "sequence": 1, "author": "user", "summary": "Current intent",
                    "source_message": null, "project_provenance": [], "created_at": at, "nodes": {}, "edges": {}}
            },
            "work": [], "questions": [], "base_sources": [], "execution_items": ["item-1"]
        })).unwrap();
        let current = ProjectWork {
            item: "item-1".into(),
            title: "Current slice".into(),
            status: Status::Ready,
            worker: None,
            summary: String::new(),
            objective: "Finish a small slice".into(),
            done_when: "The slice is verified".into(),
            constraints: vec!["Preserve current constraints".into()],
            decisions_made: vec![],
            evidence: vec![],
            repository: None,
            unfinished_attempt: None,
        };
        let mut old = current.clone();
        old.item = "old-item".into();
        old.status = Status::Done;
        old.summary = "historical-detail ".repeat(20_000);
        context.work = vec![current, old];
        let script = r#"#!/usr/bin/env python3
import json,os,pathlib,re,sys
prompt=next(a for a in sys.argv if "Full frozen project history:" in a)
assert "historical-detail" not in prompt
assert len(prompt)<16000
path=pathlib.Path(re.search(r"Full frozen project history: (.*)\. Read only",prompt).group(1))
assert path.stat().st_mode&0o777==0o600
full=json.loads(path.read_text())
assert len(full["work"])==2 and "historical-detail" in full["work"][1]["summary"]
view=json.loads(next(line for line in prompt.splitlines() if line.startswith('{"snapshot":')))
assert len(view["work"])==1 and view["history_items"]==2
assert view["snapshot"]==full["snapshot"]
pathlib.Path(os.environ["RUSTYKRAB_DATA_DIR"],"context-observed.json").write_text(json.dumps(full))
contract=json.dumps({"summary":"Inspected a small slice"})
if "exec" in sys.argv:
 print(json.dumps({"type":"item.completed","item":{"type":"agent_message","text":contract}}))
else:
 print(json.dumps({"type":"result","subtype":"success","result":contract}))
"#;
        for kind in [WorkerKind::ClaudeCode, WorkerKind::Codex] {
            let worker = f.worker(kind, f.agent("context-cli", script));
            let mut b = brief(None);
            b.kind = WorkKind::Research;
            b.project_context = Some(context.clone());
            b.prior_evidence.push(Evidence {
                item: b.item.clone(),
                kind: "project_context".into(),
                reference: serde_json::to_string(&context).unwrap(),
                hash: None,
                verified_by: None,
                at: chrono::Utc::now(),
            });
            let result = worker.run(b).await.unwrap();
            assert_eq!(result.summary, "Inspected a small slice");
            let observed: serde_json::Value = serde_json::from_slice(
                &std::fs::read(f.data.path().join("context-observed.json")).unwrap(),
            )
            .unwrap();
            assert_eq!(observed, serde_json::to_value(&context).unwrap());
            assert!(!worker
                .runs_root()
                .join("run1/project-context.json")
                .exists());
        }
    }

    fn brief(workspace: Option<Workspace>) -> Brief {
        Brief {
            item: "item-1".into(),
            kind: WorkKind::Code,
            title: "Add a status helper".into(),
            objective: "Add status() to src/lib.rs".into(),
            done_when: "status() exists".into(),
            constraints: vec!["no new dependencies".into()],
            decisions_made: Vec::new(),
            artifact_refs: Vec::new(),
            required_tools: Vec::new(),
            required_mcp_servers: Vec::new(),
            writable_resources: Vec::new(),
            inputs: Vec::new(),
            more_inputs: Vec::new(),
            prior_evidence: Vec::new(),
            last_error: None,
            budget: Budget::default(),
            origin_conversation_id: None,
            run: Some("run-1".into()),
            workspace,
            capability: None,
            project_context: None,
        }
    }

    /// The value `--disallowedTools` carries for a worker of `kind` whose
    /// spec denies `denied`.
    fn disallowed(kind: WorkerKind, denied: &[&str]) -> Option<String> {
        let data = tempfile::tempdir().unwrap();
        let spec = WorkerSpec {
            kind,
            repos: vec!["/src/app".into()],
            denied_tools: denied.iter().map(|t| t.to_string()).collect(),
            ..WorkerSpec::default()
        };
        let worker = ExternalWorker::new(
            "pinch",
            ExternalConfig::from_spec(&spec, data.path()).unwrap(),
        );
        let cmd = worker.command(
            "go",
            data.path(),
            &brief(None),
            &data.path().join("last"),
            None,
        );
        let args: Vec<String> = cmd
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let at = args.iter().position(|a| a == "--disallowedTools")?;
        args.get(at + 1).cloned()
    }

    #[test]
    fn a_spec_s_denied_tools_extend_the_built_in_deny_list() {
        let built_in = CLAUDE_DENIED_TOOLS.join(",");
        assert_eq!(
            disallowed(WorkerKind::ClaudeCode, &[]).as_deref(),
            Some(built_in.as_str()),
            "a spec without denied_tools is unchanged"
        );
        assert_eq!(
            disallowed(
                WorkerKind::ClaudeCode,
                &[
                    "Read(~/.config/**)",
                    " Edit(//Users/someone/secrets/**) ",
                    ""
                ]
            ),
            Some(format!(
                "{built_in},Read(~/.config/**),Edit(//Users/someone/secrets/**)"
            ))
        );
        // Codex has no such flag: the list is dropped, not passed on.
        assert_eq!(disallowed(WorkerKind::Codex, &["Read(~/.config/**)"]), None);
        let spec = WorkerSpec {
            kind: WorkerKind::Codex,
            repos: vec!["/src/app".into()],
            denied_tools: vec!["Read(~/.config/**)".into()],
            ..WorkerSpec::default()
        };
        let config = ExternalConfig::from_spec(&spec, Path::new("/tmp")).unwrap();
        assert!(config.denied_tools.is_empty());
    }

    /// A Claude Code stand-in: commits in its working directory, prints one
    /// Bash tool use and the result envelope, and records its arguments and
    /// whether the daemon's environment leaked into it.
    const CLAUDE: &str = r#"#!/bin/sh
printf '%s\n' "$@" > "$RUSTYKRAB_DATA_DIR/args.txt"
if [ -n "$CARGO_PKG_NAME" ]; then echo leaked > "$RUSTYKRAB_DATA_DIR/leaked.txt"; fi
echo 'pub fn status() {}' >> src/lib.rs
git add src/lib.rs
git -c user.name=t -c user.email=t@x.invalid commit -q --no-gpg-sign -m change
sha=$(git rev-parse HEAD)
echo '{"type":"system","subtype":"init"}'
echo '{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash","input":{"command":"cargo test -p fixture"}}]}}'
contract="Done. {\\\"summary\\\":\\\"added status\\\",\\\"changed_paths\\\":[\\\"src/lib.rs\\\"],\\\"commit\\\":\\\"$sha\\\",\\\"checks_run\\\":[\\\"cargo test\\\"],\\\"artifacts\\\":[{\\\"kind\\\":\\\"command_run\\\",\\\"value\\\":\\\"forged\\\"}]}"
echo "{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"num_turns\":4,\"total_cost_usd\":0.0125,\"usage\":{\"input_tokens\":100,\"output_tokens\":20},\"result\":\"$contract\"}"
"#;

    #[tokio::test]
    async fn claude_code_runs_in_its_worktree_and_returns_the_attested_contract() {
        let f = Fixture::new();
        let worker = f.worker(WorkerKind::ClaudeCode, f.agent("claude", CLAUDE));
        assert!(worker.healthy());
        assert_eq!(
            worker.capabilities().writable_resources,
            [format!("repo:{}", f.repo.path().display())]
        );
        let ws = f.workspace();
        let report = worker.run(brief(Some(ws.clone()))).await.unwrap();

        assert_eq!(report.summary, "added status");
        assert_eq!(report.changed_paths, ["src/lib.rs"]);
        let sha = report.commit.clone().unwrap();
        assert_eq!(git(f.repo.path(), &["cat-file", "-t", &sha]), "commit");
        assert_eq!(ws.tip().unwrap().as_deref(), Some(sha.as_str()));
        // The adapter's record of commands replaces the model's.
        let ran: Vec<&str> = report
            .artifacts
            .iter()
            .filter(|a| a.kind == COMMAND_RUN)
            .map(|a| a.value.as_str())
            .collect();
        assert_eq!(ran, ["cargo test -p fixture"]);
        let usage = worker.usage("run-1").unwrap();
        assert_eq!((usage.tokens, usage.iterations), (120, 4));
        assert!(usage.wall_ms > 0);
        // The worktree is gone, the branch stays, the user's checkout
        // never moved.
        assert!(!ws.path.exists());
        assert_eq!(git(f.repo.path(), &["status", "--short"]), "");
        assert_eq!(git(f.repo.path(), &["rev-parse", "HEAD"]), ws.base);
        // Headless, with a turn cap, a permission mode and an allowlist; and
        // none of the daemon's environment.
        let args = std::fs::read_to_string(f.data.path().join("args.txt")).unwrap();
        for want in [
            "-p",
            "stream-json",
            "--verbose",
            "--max-turns",
            "--permission-mode",
            "acceptEdits",
            "--allowedTools",
            "--disallowedTools",
        ] {
            assert!(
                args.lines().any(|l| l == want),
                "{want} missing from {args}"
            );
        }
        assert!(
            args.contains("parent commit: "),
            "the brief names the parent"
        );
        assert!(args.contains(&ws.branch));
        assert!(
            !f.data.path().join("leaked.txt").exists(),
            "environment leaked"
        );
    }

    /// An agent that starts a child of its own and then waits: it never
    /// returns unless something ends it.
    const SLEEPER: &str = r#"#!/bin/sh
sleep 60 &
echo $! > "$RUSTYKRAB_DATA_DIR/child.pid.tmp"
mv "$RUSTYKRAB_DATA_DIR/child.pid.tmp" "$RUSTYKRAB_DATA_DIR/child.pid"
echo $$ > "$RUSTYKRAB_DATA_DIR/agent.pid.tmp"
mv "$RUSTYKRAB_DATA_DIR/agent.pid.tmp" "$RUSTYKRAB_DATA_DIR/agent.pid"
wait
"#;

    /// The process group `pid` is in, as `ps` reports it.
    #[cfg(unix)]
    fn group_of(pid: i32) -> Option<i32> {
        let out = Command::new("ps")
            .args(["-o", "pgid=", "-p", &pid.to_string()])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim().parse().ok()
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn shutdown_ends_the_agent_and_everything_in_its_process_group() {
        let f = Fixture::new();
        let groups = Arc::new(RunGroups::default());
        let mut worker = f
            .worker(WorkerKind::ClaudeCode, f.agent("claude", SLEEPER))
            .with_groups(groups.clone());
        worker.config.retention = Retention::KeepFailed(Duration::from_secs(3600));
        let worker = Arc::new(worker);
        let ws = f.workspace();
        let run = tokio::spawn({
            let worker = worker.clone();
            let ws = ws.clone();
            async move { worker.run(brief(Some(ws))).await }
        });

        let read_pid = |name: &str| {
            std::fs::read_to_string(f.data.path().join(name))
                .ok()
                .and_then(|s| s.trim().parse::<i32>().ok())
        };
        let started = std::time::Instant::now();
        let (agent, child) = loop {
            if let (Some(a), Some(c)) = (read_pid("agent.pid"), read_pid("child.pid")) {
                break (a, c);
            }
            assert!(
                started.elapsed() < Duration::from_secs(15),
                "agent never started"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        };

        // The agent leads a group of its own, its child is in it, and the
        // daemon (this test) is not.
        assert_eq!(groups.live(), [agent]);
        assert_eq!(group_of(agent), Some(agent));
        assert_eq!(group_of(child), Some(agent));
        assert_ne!(group_of(std::process::id() as i32), Some(agent));

        assert_eq!(groups.terminate_all(Duration::from_secs(2)).await, 1);
        let outcome = tokio::time::timeout(Duration::from_secs(10), run)
            .await
            .expect("the run ends once its agent is terminated")
            .unwrap();
        let err = outcome.expect_err("a terminated run has no result");
        assert!(
            RunFailure::from_error(&err).is_some_and(|f| f.is_interrupted()),
            "shutdown reports an interruption, not a failure: {err}"
        );
        assert!(groups.live().is_empty());
        assert!(groups.interrupted.lock().unwrap().is_empty());

        // Nothing is left in the group, the child included.
        let started = std::time::Instant::now();
        while signal_group(agent, 0) {
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "process group {agent} outlived shutdown"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        // The worktree is kept for retention.
        assert!(ws.path.exists());
    }

    /// An agent that prints two assistant turns (the first as two content
    /// blocks repeating its usage) and then waits to be ended, never
    /// reaching its `result` envelope.
    const HALFWAY: &str = r#"#!/bin/sh
echo '{"type":"system","subtype":"init","session_id":"sess-1"}'
echo '{"type":"assistant","message":{"id":"msg-1","content":[{"type":"text","text":"looking"}],"usage":{"input_tokens":100,"output_tokens":20,"cache_creation_input_tokens":5}}}'
echo '{"type":"assistant","message":{"id":"msg-1","content":[{"type":"tool_use","name":"Bash","input":{"command":"ls"}}],"usage":{"input_tokens":100,"output_tokens":20,"cache_creation_input_tokens":5}}}'
echo '{"type":"user","message":{"content":[{"type":"tool_result","content":"src"}]}}'
echo '{"type":"assistant","message":{"id":"msg-2","content":[{"type":"text","text":"editing"}],"usage":{"input_tokens":200,"output_tokens":30}}}'
echo $$ > "$RUSTYKRAB_DATA_DIR/agent.pid.tmp"
mv "$RUSTYKRAB_DATA_DIR/agent.pid.tmp" "$RUSTYKRAB_DATA_DIR/agent.pid"
sleep 60 &
wait
"#;

    #[cfg(unix)]
    #[tokio::test]
    async fn an_interrupted_run_keeps_the_usage_it_had_printed() {
        let f = Fixture::new();
        let groups = Arc::new(RunGroups::default());
        let worker = Arc::new(
            f.worker(WorkerKind::ClaudeCode, f.agent("claude", HALFWAY))
                .with_groups(groups.clone()),
        );
        let run = tokio::spawn({
            let worker = worker.clone();
            let ws = f.workspace();
            async move { worker.run(brief(Some(ws))).await }
        });
        let pid = f.data.path().join("agent.pid");
        let started = std::time::Instant::now();
        while !pid.exists() {
            assert!(
                started.elapsed() < Duration::from_secs(15),
                "agent never started"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        assert_eq!(groups.terminate_all(Duration::from_secs(2)).await, 1);
        let outcome = tokio::time::timeout(Duration::from_secs(10), run)
            .await
            .expect("the run ends once its agent is terminated")
            .unwrap();
        let err = outcome.expect_err("a terminated run has no result");
        assert!(RunFailure::from_error(&err).is_some_and(|f| f.is_interrupted()));

        // Two turns, msg-1 counted once: 125 + 230 tokens.
        let usage = worker.usage("run-1").expect("an interrupted run has usage");
        assert_eq!((usage.tokens, usage.iterations), (355, 2));
        assert!(usage.wall_ms > 0);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_timed_out_run_keeps_the_usage_it_had_printed() {
        let f = Fixture::new();
        let mut worker = f.worker(WorkerKind::ClaudeCode, f.agent("claude", HALFWAY));
        worker.config.timeout = Duration::from_millis(1500);
        let started = std::time::Instant::now();
        let err = worker.run(brief(None)).await.unwrap_err();
        assert!(
            started.elapsed() < Duration::from_secs(15),
            "killed, not waited"
        );
        let e = classify(&run_failure_input(&err), &Context::default());
        assert_eq!(e.subclass, ErrorSubclass::Wall);

        // Two turns, msg-1 counted once: 125 + 230 tokens.
        let usage = worker.usage("run-1").expect("a timed-out run has usage");
        assert_eq!((usage.tokens, usage.iterations), (355, 2));
        assert!(usage.wall_ms > 0);
    }

    #[test]
    fn a_result_envelope_outranks_the_turns_seen_before_it() {
        let stdout = [
            r#"{"type":"assistant","message":{"id":"m1","content":[],"usage":{"input_tokens":1,"output_tokens":1}}}"#,
            r#"{"type":"result","subtype":"success","num_turns":3,"usage":{"input_tokens":40,"output_tokens":2},"result":"{}"}"#,
        ]
        .join("\n");
        let t = read_claude(&stdout);
        assert_eq!((t.usage.tokens, t.usage.iterations), (42, 3));
    }

    #[tokio::test]
    async fn terminate_all_with_no_live_runs_signals_nothing() {
        let groups = RunGroups::default();
        assert_eq!(groups.terminate_all(Duration::from_millis(10)).await, 0);
    }

    #[tokio::test]
    async fn a_repository_it_was_not_added_for_is_a_scope_stop() {
        let f = Fixture::new();
        let other = Fixture::new();
        let worker = f.worker(WorkerKind::ClaudeCode, f.agent("claude", CLAUDE));
        let err = worker
            .run(brief(Some(other.workspace())))
            .await
            .unwrap_err();
        let error = classify(&run_failure_input(&err), &Context::default());
        assert_eq!(error.class, ErrorClass::Policy);
        assert_eq!(error.subclass, ErrorSubclass::Scope);
    }

    #[tokio::test]
    async fn the_turn_cap_the_timeout_and_a_failed_process_are_typed() {
        let f = Fixture::new();
        let capped = f.agent(
            "capped",
            "#!/bin/sh\necho '{\"type\":\"result\",\"subtype\":\"error_max_turns\",\"is_error\":true,\"num_turns\":30}'\n",
        );
        let err = f
            .worker(WorkerKind::ClaudeCode, capped)
            .run(brief(None))
            .await
            .unwrap_err();
        let e = classify(&run_failure_input(&err), &Context::default());
        assert_eq!(e.subclass, ErrorSubclass::Iterations);

        let slow = f.agent("slow", "#!/bin/sh\nsleep 30\n");
        let mut worker = f.worker(WorkerKind::ClaudeCode, slow);
        worker.config.timeout = Duration::from_millis(300);
        let started = std::time::Instant::now();
        let err = worker.run(brief(None)).await.unwrap_err();
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "killed, not waited"
        );
        let e = classify(&run_failure_input(&err), &Context::default());
        assert_eq!(e.subclass, ErrorSubclass::Wall);

        let broken = f.agent(
            "broken",
            "#!/bin/sh\necho 'error: unknown option --bogus' >&2\nexit 2\n",
        );
        let err = f
            .worker(WorkerKind::ClaudeCode, broken)
            .run(brief(None))
            .await
            .unwrap_err();
        assert!(
            matches!(
                RunFailure::from_error(&err),
                Some(RunFailure::Process { code: Some(2), .. })
            ),
            "{err}"
        );

        let missing = f.worker(WorkerKind::ClaudeCode, f.bin.path().join("nope"));
        assert!(!missing.healthy());
    }

    /// A Claude Code stand-in that commits, then stops at its turn cap with
    /// session `sess-1`. Resumed (`--resume`), it records its arguments and
    /// runs `on_resume`.
    fn capped_claude(on_resume: &str) -> String {
        let first = r#"#!/bin/sh
resume=""
prev=""
for a in "$@"; do
  if [ "$prev" = "--resume" ]; then resume="$a"; fi
  prev="$a"
done
if [ -z "$resume" ]; then
  printf '%s\n' "$@" > "$RUSTYKRAB_DATA_DIR/args.txt"
  echo 'pub fn status() {}' >> src/lib.rs
  git add src/lib.rs
  git -c user.name=t -c user.email=t@x.invalid commit -q --no-gpg-sign -m change
  echo '{"type":"system","subtype":"init","session_id":"sess-1"}'
  echo '{"type":"assistant","session_id":"sess-1","message":{"content":[{"type":"tool_use","name":"Bash","input":{"command":"cargo test -p fixture"}}]}}'
  echo '{"type":"result","subtype":"error_max_turns","is_error":true,"num_turns":25,"session_id":"sess-1","usage":{"input_tokens":100,"output_tokens":20}}'
  exit 1
fi
printf '%s\n' "$@" > "$RUSTYKRAB_DATA_DIR/resume.txt"
"#;
        format!("{first}{on_resume}\n")
    }

    /// The resume answers with the contract for the commit already made.
    const RESUME_CONTRACT: &str = r#"sha=$(git rev-parse HEAD)
echo '{"type":"assistant","session_id":"sess-1","message":{"content":[{"type":"tool_use","name":"Bash","input":{"command":"git log -1"}}]}}'
contract="{\\\"summary\\\":\\\"added status\\\",\\\"changed_paths\\\":[\\\"src/lib.rs\\\"],\\\"commit\\\":\\\"$sha\\\",\\\"checks_run\\\":[\\\"cargo test -p fixture\\\"]}"
echo "{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"num_turns\":2,\"session_id\":\"sess-1\",\"usage\":{\"input_tokens\":10,\"output_tokens\":5},\"result\":\"$contract\"}""#;

    #[tokio::test]
    async fn the_brief_names_the_turn_limit() {
        let f = Fixture::new();
        let mut worker = f.worker(WorkerKind::ClaudeCode, f.agent("claude", CLAUDE));
        worker.config.max_turns = 12;
        worker.run(brief(Some(f.workspace()))).await.unwrap();
        let args = std::fs::read_to_string(f.data.path().join("args.txt")).unwrap();
        assert!(args.contains("--max-turns\n12\n"), "{args}");
        assert!(args.contains("You have 12 turns in all"), "{args}");
        assert!(args.contains("with a few turns to spare"), "{args}");
        // Codex has no turn cap to name.
        let codex = render_executor_brief(
            &brief(None),
            "pinch",
            WorkerKind::Codex,
            Path::new("/tmp/run"),
            Path::new("/tmp/skills"),
            None,
        );
        assert!(!codex.contains("turns in all"), "{codex}");
    }

    #[tokio::test]
    async fn a_run_at_its_turn_cap_is_resumed_once_for_its_contract() {
        let f = Fixture::new();
        let worker = f.worker(
            WorkerKind::ClaudeCode,
            f.agent("claude", &capped_claude(RESUME_CONTRACT)),
        );
        let ws = f.workspace();
        let report = worker.run(brief(Some(ws.clone()))).await.unwrap();

        assert_eq!(report.summary, "added status");
        assert_eq!(report.changed_paths, ["src/lib.rs"]);
        let sha = report.commit.clone().unwrap();
        assert_eq!(git(f.repo.path(), &["cat-file", "-t", &sha]), "commit");
        assert!(
            report
                .known_limits
                .iter()
                .any(|l| l.contains("recovered after the turn cap")),
            "{:?}",
            report.known_limits
        );
        // Attested as usual, with what both invocations ran.
        let ran: Vec<&str> = report
            .artifacts
            .iter()
            .filter(|a| a.kind == COMMAND_RUN)
            .map(|a| a.value.as_str())
            .collect();
        assert_eq!(ran, ["cargo test -p fixture", "git log -1"]);
        let usage = worker.usage("run-1").unwrap();
        assert_eq!((usage.tokens, usage.iterations), (135, 27));

        // The same session, allowlist and permission mode, for two turns,
        // asked only for the contract.
        let first = std::fs::read_to_string(f.data.path().join("args.txt")).unwrap();
        let resume = std::fs::read_to_string(f.data.path().join("resume.txt")).unwrap();
        assert!(resume.contains("--resume\nsess-1\n"), "{resume}");
        assert!(resume.contains("--max-turns\n2\n"), "{resume}");
        assert!(
            resume.contains("--permission-mode\nacceptEdits\n"),
            "{resume}"
        );
        let after = |args: &str, flag: &str| {
            let mut lines = args.lines();
            lines.find(|l| *l == flag);
            lines.next().map(str::to_string)
        };
        assert_eq!(
            after(&resume, "--allowedTools"),
            after(&first, "--allowedTools")
        );
        assert_eq!(
            after(&resume, "--disallowedTools"),
            after(&first, "--disallowedTools")
        );
        assert!(resume.contains("already committed"), "{resume}");
        assert!(!ws.path.exists(), "a recovered run gives its worktree back");
    }

    #[tokio::test]
    async fn a_failed_resume_leaves_the_run_at_its_turn_cap() {
        let f = Fixture::new();
        for on_resume in [
            "echo 'resume failed' >&2\nexit 1",
            "echo '{\"type\":\"result\",\"subtype\":\"error_max_turns\",\"is_error\":true,\"num_turns\":2,\"session_id\":\"sess-1\"}'",
            "echo '{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"num_turns\":1,\"result\":\"I could not finish.\"}'",
        ] {
            let worker = f.worker(
                WorkerKind::ClaudeCode,
                f.agent("claude", &capped_claude(on_resume)),
            );
            let err = worker.run(brief(None)).await.unwrap_err();
            let e = classify(&run_failure_input(&err), &Context::default());
            assert_eq!(e.subclass, ErrorSubclass::Iterations, "{on_resume}: {err}");
            assert!(f.data.path().join("resume.txt").exists(), "resumed once");
            std::fs::remove_file(f.data.path().join("resume.txt")).unwrap();
        }
    }

    /// The resume answers with a contract that names no commit.
    const RESUME_NO_COMMIT: &str = r#"contract="{\\\"summary\\\":\\\"ran out\\\",\\\"changed_paths\\\":[],\\\"commit\\\":null,\\\"checks_run\\\":[],\\\"error\\\":{\\\"class\\\":\\\"budget\\\",\\\"subclass\\\":\\\"iterations\\\",\\\"detail\\\":\\\"Turn limit reached before committing\\\"}}"
echo "{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"num_turns\":1,\"session_id\":\"sess-1\",\"result\":\"$contract\"}""#;

    #[tokio::test]
    async fn a_resume_in_a_worktree_that_committed_nothing_stays_at_the_turn_cap() {
        let f = Fixture::new();
        let worker = f.worker(
            WorkerKind::ClaudeCode,
            f.agent("claude", &capped_claude(RESUME_NO_COMMIT)),
        );
        let err = worker.run(brief(Some(f.workspace()))).await.unwrap_err();
        assert!(f.data.path().join("resume.txt").exists(), "resumed once");
        assert!(
            matches!(
                RunFailure::from_error(&err),
                Some(RunFailure::Budget {
                    budget: BudgetKind::Iterations,
                    ..
                })
            ),
            "{err}"
        );
        let e = classify(&run_failure_input(&err), &Context::default());
        assert_eq!(e.subclass, ErrorSubclass::Iterations, "{err}");
    }

    #[tokio::test]
    async fn a_capability_build_is_told_where_the_skills_are() {
        let f = Fixture::new();
        let worker = f.worker(WorkerKind::ClaudeCode, f.agent("claude", CLAUDE));
        let mut b = brief(None);
        b.kind = WorkKind::Capability;
        b.artifact_refs = vec![rustykrab_control::routing::CapabilityRef {
            gap: rustykrab_control::errors::GapKind::Tool,
            subject: "tide_table".into(),
        }
        .to_ref()];
        // An acquisition of the same need is not told to build anything.
        b.capability = Some(rustykrab_core::work::CapabilityMode::Acquire);
        let acquiring = render_executor_brief(
            &b,
            "pinch",
            WorkerKind::ClaudeCode,
            Path::new("/tmp/run"),
            &f.data.path().join("skills"),
            None,
        );
        assert!(!acquiring.contains("SKILL.md"), "{acquiring}");
        b.capability = Some(rustykrab_core::work::CapabilityMode::Build);
        let prompt = render_executor_brief(
            &b,
            "pinch",
            WorkerKind::ClaudeCode,
            Path::new("/tmp/run"),
            &f.data.path().join("skills"),
            None,
        );
        let skill = f.data.path().join("skills/tide_table/SKILL.md");
        assert!(prompt.contains(&skill.display().to_string()), "{prompt}");
        assert!(prompt.contains("name = \"tide_table\""), "{prompt}");
        // The scratch run is outside any repository and is removed after.
        let _ = worker.run(b).await;
        let args = std::fs::read_to_string(f.data.path().join("args.txt")).unwrap();
        assert!(args.lines().any(|l| l == "--add-dir"), "{args}");
        let runs = f.data.path().join("workers/pinch/runs");
        assert_eq!(std::fs::read_dir(runs).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn codex_exec_is_read_from_its_events_and_last_message() {
        let f = Fixture::new();
        let codex = f.agent(
            "codex",
            r#"#!/bin/sh
out=""
dir="."
while [ $# -gt 0 ]; do
  case "$1" in
    --output-last-message) out="$2"; shift ;;
    --cd) dir="$2"; shift ;;
  esac
  shift
done
cd "$dir"
echo 'pub fn status() {}' >> src/lib.rs
git add src/lib.rs
git -c user.name=t -c user.email=t@x.invalid commit -q --no-gpg-sign -m change
echo '{"type":"item.completed","item":{"type":"command_execution","command":"bash -lc cargo test"}}'
echo '{"type":"turn.completed","usage":{"input_tokens":50,"output_tokens":5}}'
printf '{"summary":"added status","changed_paths":["src/lib.rs"],"checks_run":["cargo test"]}' > "$out"
"#,
        );
        let worker = f.worker(WorkerKind::Codex, codex);
        assert_eq!(worker.kind(), WorkerKind::Codex);
        let report = worker.run(brief(Some(f.workspace()))).await.unwrap();
        assert_eq!(report.summary, "added status");
        assert_eq!(report.checks_run, ["cargo test"]);
        assert!(report
            .artifacts
            .iter()
            .any(|a| a.kind == COMMAND_RUN && a.value == "bash -lc cargo test"));
        assert_eq!(worker.usage("run-1").unwrap().tokens, 55);
    }

    #[test]
    fn the_contract_is_found_in_prose_fences_or_alone_and_made_classifiable() {
        let alone = parse_contract(r#"{"summary":"ok","changed_paths":["a.rs"]}"#).unwrap();
        assert_eq!(alone.changed_paths, ["a.rs"]);
        let fenced = parse_contract(
            "Here it is:\n```json\n{\"summary\":\"fenced\",\"commit\":null}\n```\nThanks.",
        )
        .unwrap();
        assert_eq!(fenced.summary, "fenced");
        let trailing = parse_contract(
            "I changed things {braces in prose}.\n{\"summary\":\"last\",\"known_limits\":[\"x}\"]}",
        )
        .unwrap();
        assert_eq!(trailing.summary, "last");
        let odd = parse_contract(
            r#"{"summary":"stuck","error":{"class":"weird","subclass":"odd","detail":"E-17"},
               "blocked":{"reason":"needs_a_hug"}}"#,
        )
        .unwrap();
        let error = odd.error.unwrap();
        assert_eq!(error.class, ErrorClass::Unknown);
        assert_eq!(error.detail, "E-17");
        assert_eq!(
            odd.blocked.unwrap().reason,
            rustykrab_core::work::BlockedReason::NeedsDecision
        );
        assert!(parse_contract("no json here").is_err());
    }

    #[test]
    fn a_blocked_report_keeps_the_question_under_another_key_or_takes_the_summary() {
        let elsewhere = parse_contract(
            r#"{"summary":"stuck","blocked":{"reason":"needs_decision","question":"Which port?"}}"#,
        )
        .unwrap();
        assert_eq!(elsewhere.blocked.unwrap().detail, "Which port?");
        let blank = parse_contract(
            r#"{"summary":"stuck","blocked":{"reason":"needs_decision","detail":"  ",
               "what_you_need":"the API key's scope"}}"#,
        )
        .unwrap();
        assert_eq!(blank.blocked.unwrap().detail, "the API key's scope");
        let odd_key = parse_contract(
            r#"{"summary":"stuck","blocked":{"reason":"needs_decision","ask":"Keep the flag?"}}"#,
        )
        .unwrap();
        assert_eq!(odd_key.blocked.unwrap().detail, "Keep the flag?");
        let none = parse_contract(
            r#"{"summary":"Which of the two schemas should win?","blocked":{"reason":"needs_decision"}}"#,
        )
        .unwrap();
        assert_eq!(
            none.blocked.unwrap().detail,
            "Which of the two schemas should win?"
        );
        let kept = parse_contract(
            r#"{"summary":"stuck","blocked":{"reason":"needs_decision","detail":"Merge now?","question":"other"}}"#,
        )
        .unwrap();
        assert_eq!(kept.blocked.unwrap().detail, "Merge now?");
    }

    #[test]
    fn the_brief_shows_where_a_blocked_report_puts_its_question() {
        let prompt = render_executor_brief(
            &brief(None),
            "shipwright",
            WorkerKind::ClaudeCode,
            Path::new("/tmp/w"),
            Path::new("/tmp/skills"),
            Some(80),
        );
        assert!(
            prompt.contains(
                r#"{"reason": "needs_decision", "detail": "the question or what you need", "needs": []}"#
            ),
            "{prompt}"
        );
    }

    #[test]
    fn the_brief_says_checks_run_holds_verbatim_shell_commands() {
        let prompt = render_executor_brief(
            &brief(None),
            "shipwright",
            WorkerKind::ClaudeCode,
            Path::new("/tmp/w"),
            Path::new("/tmp/skills"),
            Some(80),
        );
        assert!(
            prompt.contains(
                "Each checks_run entry is a shell command you actually ran via Bash, copied \
                 exactly, with no results or descriptions."
            ),
            "{prompt}"
        );
        assert!(
            prompt.contains(
                "Checks done with Grep, Read or Glob belong in the summary or known_limits, \
                 not checks_run."
            ),
            "{prompt}"
        );
    }

    fn question(text: &str, class: &str, options: &[&str]) -> rustykrab_core::work::Question {
        rustykrab_core::work::Question {
            text: text.into(),
            class: class.into(),
            options: options.iter().map(|o| o.to_string()).collect(),
        }
    }

    #[test]
    fn string_questions_read_as_questions_with_no_class_or_options() {
        let report =
            parse_contract(r#"{"summary":"ok","questions":["Which port?","Keep the flag?"]}"#)
                .unwrap();
        assert_eq!(
            report.questions,
            [
                question("Which port?", "", &[]),
                question("Keep the flag?", "", &[])
            ]
        );
    }

    #[test]
    fn object_questions_still_parse() {
        let report = parse_contract(
            r#"{"summary":"ok","questions":[{"text":"Which port?","class":"decision","options":["80","443"]}]}"#,
        )
        .unwrap();
        assert_eq!(
            report.questions,
            [question("Which port?", "decision", &["80", "443"])]
        );
    }

    #[test]
    fn a_mix_of_string_and_object_questions_parses() {
        let report = parse_contract(
            r#"{"summary":"ok","questions":["Plain?",{"text":"Typed?","class":"preference"}]}"#,
        )
        .unwrap();
        assert_eq!(
            report.questions,
            [
                question("Plain?", "", &[]),
                question("Typed?", "preference", &[])
            ]
        );
    }

    #[test]
    fn a_question_that_is_neither_object_nor_string_still_fails() {
        let err = parse_contract(r#"{"summary":"ok","questions":["fine",42]}"#).unwrap_err();
        assert!(err.contains("did not parse"), "{err}");
        assert!(parse_contract(r#"{"summary":"ok","questions":[null]}"#).is_err());
        assert!(parse_contract(r#"{"summary":"ok","questions":[["nested"]]}"#).is_err());
    }

    #[tokio::test]
    async fn max_worker_checks_selected_profile_and_disables_api_overrides() {
        let f = Fixture::new();
        let script = format!(
            r#"#!/bin/sh
if [ "$1" = auth ]; then
 printf '%s\n' '{{"loggedIn":true,"authMethod":"claude.ai","apiProvider":"firstParty","subscriptionType":"max","email":"worker@example.invalid"}}'
 exit 0
fi
[ "$CLAUDE_CONFIG_DIR" = "{}" ] || exit 91
[ -z "$ANTHROPIC_API_KEY$ANTHROPIC_AUTH_TOKEN$ANTHROPIC_BASE_URL$CLAUDE_CODE_OAUTH_TOKEN" ] || exit 92
printf '%s\n' "$@" > "$RUSTYKRAB_DATA_DIR/max-args.txt"
printf '%s\n' '{{"type":"result","subtype":"success","is_error":false,"result":"{{\"summary\":\"native runtime executed\"}}","usage":{{"input_tokens":20,"output_tokens":5}},"num_turns":1}}'
"#,
            f.bin.path().display()
        );
        let cli = f.agent("max-cli", &script);
        let spec = WorkerSpec {
            kind: WorkerKind::ClaudeCode,
            command: Some(cli.display().to_string()),
            claude_config_dir: Some(f.bin.path().display().to_string()),
            require_max: true,
            ..WorkerSpec::default()
        };
        let worker = ExternalWorker::new(
            "max-one",
            ExternalConfig::from_spec(&spec, f.data.path()).unwrap(),
        );
        assert!(!worker.healthy());
        assert!(worker.refresh().await);
        assert!(worker.healthy());
        assert!(!worker.refresh().await);
        assert_eq!(worker.runtime_status().unwrap()["subscription"], "max");
        let report = worker.run(brief(None)).await.unwrap();
        assert_eq!(report.summary, "native runtime executed");
        let args = std::fs::read_to_string(f.data.path().join("max-args.txt")).unwrap();
        assert!(args.contains("--setting-sources\n\n"));
        assert!(args.contains("--strict-mcp-config"));
        let mut bad = spec.clone();
        bad.env = vec!["ANTHROPIC_API_KEY".into()];
        assert!(ExternalConfig::from_spec(&bad, f.data.path()).is_err());
        bad = spec.clone();
        bad.kind = WorkerKind::Codex;
        assert!(ExternalConfig::from_spec(&bad, f.data.path()).is_err());
        let wrong = f.agent("api-cli", "#!/bin/sh\nprintf '%s\\n' '{\"loggedIn\":true,\"authMethod\":\"api_key\",\"apiProvider\":\"firstParty\",\"subscriptionType\":\"max\"}'\n");
        bad = spec;
        bad.command = Some(wrong.display().to_string());
        let worker = ExternalWorker::new(
            "wrong",
            ExternalConfig::from_spec(&bad, f.data.path()).unwrap(),
        );
        worker.refresh().await;
        assert!(!worker.healthy());
        assert!(matches!(
            worker.run(brief(None)).await,
            Err(Error::ModelAuthError(_))
        ));
    }

    #[tokio::test]
    async fn max_worker_observed_quota_is_unavailable_and_typed() {
        let f = Fixture::new();
        let script = r#"#!/bin/sh
if [ "$1" = auth ]; then
 printf '%s\n' '{"loggedIn":true,"authMethod":"claude.ai","apiProvider":"firstParty","subscriptionType":"max","email":"worker@example.invalid"}'
else
 printf '%s\n' '{"type":"result","subtype":"success","is_error":true,"result":"You have hit your weekly limit","usage":{"input_tokens":0,"output_tokens":0},"num_turns":1}'
fi
"#;
        let cli = f.agent("quota-cli", script);
        let spec = WorkerSpec {
            kind: WorkerKind::ClaudeCode,
            command: Some(cli.display().to_string()),
            require_max: true,
            ..WorkerSpec::default()
        };
        let worker = ExternalWorker::new(
            "limited",
            ExternalConfig::from_spec(&spec, f.data.path()).unwrap(),
        );
        worker.refresh().await;
        assert!(matches!(
            worker.run(brief(None)).await,
            Err(Error::ModelRateLimit(_))
        ));
        assert!(!worker.healthy());
        assert!(worker.runtime_status().unwrap()["rate_limited_until"].is_string());
        assert!(matches!(
            worker.run(brief(None)).await,
            Err(Error::ModelRateLimit(_))
        ));
    }
    #[tokio::test]
    async fn chatgpt_codex_profile_execution_quota_and_login_switch() {
        let f = Fixture::new();
        let script = r#"#!/usr/bin/env python3
import os,sys,json
state=json.load(open(os.path.join(os.environ['CODEX_HOME'],'probe-state.json')))
if 'app-server' in sys.argv:
 for line in sys.stdin:
  r=json.loads(line)
  if 'id' not in r: continue
  method=r['method']
  if method=='initialize': result={}
  elif method=='account/read': result={'requiresOpenaiAuth':True,'account':state['account']}
  elif method=='account/rateLimits/read':
   if state.get('quota_failure'):
    print(json.dumps({'id':r['id'],'error':{'message':'private account detail'}}),flush=True);continue
   result={'ordinaryUsageAllowed':state.get('allowed',True),'accountId':'private-uuid','rateLimitsByLimitId':{'codex':{'primary':{'usedPercent':state.get('used',12),'windowDurationMins':300,'resetsAt':2000000000},'secondary':None}}}
  print(json.dumps({'id':r['id'],'result':result}),flush=True)
else:
 assert not any(k.startswith(('OPENAI_','AZURE_')) for k in os.environ)
 assert '--ignore-user-config' in sys.argv and '--no-daemon' in sys.argv and '--approve-for-me' in sys.argv
 assert 'model_provider="openai"' in sys.argv and 'forced_login_method="chatgpt"' in sys.argv
 assert '--sandbox' not in sys.argv # --approve-for-me already selects workspace-write
 assert '--dangerously-bypass-approvals-and-sandbox' not in sys.argv
 open(os.path.join(os.environ['RUSTYKRAB_DATA_DIR'],'codex-executed'),'w').write('yes')
 if state.get('infer_limit'):
  print(json.dumps({'type':'turn.failed','error':{'message':'Usage limit reached'}}));sys.exit(1)
 print(json.dumps({'type':'item.completed','item':{'type':'command_execution','command':'cargo check','status':'completed'}}))
 print(json.dumps({'type':'item.completed','item':{'type':'agent_message','text':json.dumps({'summary':'Codex ran','checks_run':['cargo check']})}}))
 print(json.dumps({'type':'turn.completed','usage':{'input_tokens':20,'cached_input_tokens':10,'output_tokens':5}}))
"#;
        let cli = f.agent("codex-chatgpt", script);
        let state_file = f.bin.path().join("probe-state.json");
        let account =
            serde_json::json!({"type":"chatgpt","planType":"pro","email":"one@example.invalid"});
        let write = |value: Value| {
            std::fs::write(&state_file, serde_json::to_vec(&value).unwrap()).unwrap()
        };
        write(serde_json::json!({"account":account}));
        let spec = WorkerSpec {
            kind: WorkerKind::Codex,
            command: Some(cli.display().to_string()),
            codex_home: Some(f.bin.path().display().to_string()),
            require_chatgpt: true,
            ..WorkerSpec::default()
        };
        let worker = ExternalWorker::new(
            "codex-one",
            ExternalConfig::from_spec(&spec, f.data.path()).unwrap(),
        );
        assert!(!worker.healthy());
        assert!(worker.refresh().await);
        assert!(worker.healthy());
        assert!(!worker.refresh().await);
        let runtime = worker.runtime_status().unwrap();
        assert_eq!(runtime["subscription"], "pro");
        assert_eq!(
            runtime["rate_limits"]["codex"]["primary"]["used_percent"],
            12.0
        );
        let sanitized = runtime.to_string();
        assert!(!sanitized.contains("example.invalid") && !sanitized.contains("private-uuid"));
        let report = worker.run(brief(None)).await.unwrap();
        assert_eq!(report.summary, "Codex ran");
        assert!(report
            .artifacts
            .iter()
            .any(|a| a.kind == COMMAND_RUN && a.value == "cargo check"));
        {
            let usage = worker.usage.lock().unwrap();
            assert_eq!(usage.values().next().unwrap().tokens, 25);
        }
        write(serde_json::json!({"account":account,"infer_limit":true}));
        assert!(matches!(
            worker.run(brief(None)).await,
            Err(Error::ModelRateLimit(_))
        ));
        assert!(!worker.healthy());
        assert!(matches!(
            worker.run(brief(None)).await,
            Err(Error::ModelRateLimit(_))
        ));
        // A new login clears the previous account's cooldown. Failed quota
        // reads still report the verified account with unknown capacity.
        let other =
            serde_json::json!({"type":"chatgpt","planType":"plus","email":"two@example.invalid"});
        write(serde_json::json!({"account":other,"quota_failure":true}));
        worker
            .codex_runtime
            .as_ref()
            .unwrap()
            .verify_login(true)
            .await
            .unwrap();
        assert!(worker.healthy());
        assert!(worker.runtime_status().unwrap()["quota_error"].is_string());
        assert!(worker.runtime_status().unwrap()["quota_checked_at"].is_null());
        // The server's exhausted quota prevents process execution altogether.
        write(serde_json::json!({"account":other,"allowed":false,"used":100}));
        assert!(matches!(
            worker.run(brief(None)).await,
            Err(Error::ModelRateLimit(_))
        ));
        std::fs::remove_file(f.data.path().join("codex-executed")).unwrap();
        write(serde_json::json!({"account":{"type":"apiKey"}}));
        assert!(matches!(
            worker.run(brief(None)).await,
            Err(Error::ModelAuthError(_))
        ));
        assert!(!f.data.path().join("codex-executed").exists());
        for env in [
            "OPENAI_API_KEY",
            "CODEX_API_KEY",
            "CODEX_HOME",
            "AZURE_OPENAI_API_KEY",
        ] {
            let mut bad = spec.clone();
            bad.env = vec![env.into()];
            assert!(ExternalConfig::from_spec(&bad, f.data.path()).is_err());
        }
        let mut bad = spec.clone();
        bad.kind = WorkerKind::ClaudeCode;
        assert!(ExternalConfig::from_spec(&bad, f.data.path()).is_err());
        let mut bad = spec;
        bad.codex_home = Some("relative".into());
        assert!(ExternalConfig::from_spec(&bad, f.data.path()).is_err());
    }
    #[test]
    fn codex_failed_started_turn_is_recorded_without_inventing_tokens() {
        let transcript = read_codex(
            r#"{"type":"turn.started"}
{"type":"turn.failed","error":{"message":"Usage limit reached"}}"#,
        );
        assert_eq!(transcript.usage.iterations, 1);
        assert_eq!(transcript.usage.tokens, 0);
        assert!(transcript.failure.is_some());
    }
    #[test]
    fn codex_attestation_decodes_shell_display_quotes_without_rewriting_commands() {
        // This exact display shape was emitted by the installed native CLI.
        let inner = r#"python3 -c 'from pathlib import Path; assert Path("proof.txt").read_bytes() == ("nonce" + chr(10)).encode(); print("Proof content verified exactly.")'"#;
        let displayed = r#"/bin/bash -c "python3 -c 'from pathlib import Path; assert Path(\"proof.txt\").read_bytes() == (\"nonce\" + chr(10)).encode(); print(\"Proof content verified exactly.\")'""#;
        assert_eq!(
            codex_command(&serde_json::json!(displayed)).as_deref(),
            Some(inner)
        );
        let event = serde_json::json!({"type":"item.completed","item":{"type":"command_execution","command":displayed}});
        assert_eq!(read_codex(&event.to_string()).commands, [inner]);
        assert_eq!(
            codex_command(&serde_json::json!(["/bin/zsh", "-lc", inner])).as_deref(),
            Some(inner)
        );
        let heredoc = "python3 - <<'PY'\nprint('Unicode: 水; literal: \\n')\nPY";
        let displayed = format!("/bin/bash -c {}", shlex::try_quote(heredoc).unwrap());
        assert_eq!(
            codex_command(&serde_json::json!(displayed)).as_deref(),
            Some(heredoc)
        );
        // A heredoc stays a heredoc; no claim to a rewritten python -c is attested.
        assert!(!codex_command(&serde_json::json!(displayed))
            .unwrap()
            .starts_with("python3 -c"));
        for unrelated in [
            "echo bash -c 'cargo test'",
            "bash -lc cargo test",
            "/bin/bash -c 'cargo test' && echo done",
            "malformed '",
        ] {
            assert_eq!(
                codex_command(&serde_json::json!(unrelated)).as_deref(),
                Some(unrelated)
            );
        }
    }
    #[test]
    fn frozen_research_reviews_use_enforced_native_read_only_modes() {
        let f = Fixture::new();
        let cli = f.agent("read-only-review", "#!/bin/sh\nexit 0\n");
        let mut b = brief(None);
        b.kind = rustykrab_core::work::WorkKind::Research;
        b.artifact_refs.push(rustykrab_core::work::ArtifactRef {
            kind: rustykrab_core::dream_review::REVIEW_ONLY.into(),
            value: "true".into(),
        });
        for kind in [WorkerKind::Codex, WorkerKind::ClaudeCode] {
            let mut worker = f.worker(kind, cli.clone());
            worker.config.require_chatgpt = kind == WorkerKind::Codex;
            let command = worker.command(
                "review",
                f.data.path(),
                &b,
                &f.data.path().join("last"),
                None,
            );
            let args: Vec<_> = command
                .as_std()
                .get_args()
                .map(|s| s.to_string_lossy().into_owned())
                .collect();
            if kind == WorkerKind::Codex {
                assert!(args.windows(2).any(|a| a == ["--sandbox", "read-only"]));
                assert!(args.contains(&"approval_policy=\"never\"".into()));
                assert!(!args.contains(&"--approve-for-me".into()));
            } else {
                assert!(args.windows(2).any(|a| a == ["--tools", ""]));
            }
        }
    }
}
