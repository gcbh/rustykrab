//! `ExternalWorker`: the `claude_code` and `codex` worker kinds of the
//! control layer (`docs/plans/control-layer-and-worker-fleet.md`, section
//! 5).
//!
//! One run is one headless invocation of the agent's own CLI:
//!
//! - **claude_code**: `claude -p <brief> --output-format stream-json
//!   --verbose --max-turns <n> --permission-mode <mode> --allowedTools <list>
//!   --disallowedTools <list>`, plus `--model` and, for a capability build,
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
    /// Claude Code's `--allowedTools`; empty takes [`CLAUDE_DEFAULT_TOOLS`].
    pub allowed_tools: Vec<String>,
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
        Ok(ExternalConfig {
            kind: spec.kind,
            command: PathBuf::from(spec.command.as_deref().unwrap_or(default_command)),
            repos: spec.repos.iter().map(|r| r.trim().to_string()).collect(),
            model: spec.model.clone(),
            allowed_tools: spec.allowed_tools.clone(),
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
        match c.kind {
            WorkerKind::Codex => {
                cmd.arg("exec")
                    .arg("--json")
                    .arg("--skip-git-repo-check")
                    .args(["--sandbox", "workspace-write"])
                    .arg("--cd")
                    .arg(dir)
                    .arg("--output-last-message")
                    .arg(last);
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
                if let Some(session) = resume {
                    cmd.args(["--resume", session]);
                }
                cmd.args(["--output-format", "stream-json", "--verbose"])
                    .args(["--max-turns", &turns])
                    .args(["--permission-mode", &c.permission_mode])
                    .args(["--allowedTools", &allowed])
                    .args(["--disallowedTools", &CLAUDE_DENIED_TOOLS.join(",")]);
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
        cmd.env("RUSTYKRAB_DATA_DIR", &c.data_dir)
            .env("RUSTYKRAB_SKILLS_DIR", &c.skills_dir);
        cmd
    }

    /// Start `cmd` in a process group recorded in [`RunGroups`] and wait
    /// for it, up to `limit`. Whatever it left running in its group is
    /// killed when this returns. Also says whether daemon shutdown
    /// ([`RunGroups::terminate_all`]) ended the group, which must be read
    /// before the group is let go.
    async fn execute(
        &self,
        mut cmd: tokio::process::Command,
        limit: Duration,
    ) -> (Result<std::process::Output>, bool) {
        let child = match cmd.spawn() {
            Ok(child) => child,
            Err(e) => {
                let why = format!("cannot start {}: {e}", self.config.command.display());
                return (Err(process_failure(None, &why)), false);
            }
        };
        // The agent leads its own group, so the group id is its pid.
        let group = child
            .id()
            .and_then(|pid| i32::try_from(pid).ok())
            .map(|pid| self.groups.enter(pid));
        let output = match tokio::time::timeout(limit, child.wait_with_output()).await {
            // The child was dropped with the future, and killed; the rest
            // of its group goes when `group` does.
            Err(_) => Err(RunFailure::Budget {
                budget: BudgetKind::Wall,
                detail: format!(
                    "{}s wall budget spent before {} returned",
                    limit.as_secs(),
                    self.config.command.display()
                ),
            }
            .into_error()),
            Ok(Err(e)) => Err(process_failure(None, &e.to_string())),
            Ok(Ok(output)) => Ok(output),
        };
        let interrupted = group.as_ref().is_some_and(GroupGuard::interrupted);
        // Whatever the agent left running in its group goes with it.
        drop(group);
        (output, interrupted)
    }

    /// Resume a Claude Code session that stopped at its turn cap and ask
    /// only for the result contract. `None` when there is no time left or
    /// the resume did not return one; beside it, whether daemon shutdown
    /// ended the resume.
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
        let (executed, interrupted) = self.execute(cmd, limit).await;
        let recovered = match executed {
            Ok(output) => self.read_output(brief, run, &output, last, true).0,
            Err(e) => Err(e),
        };
        let report = match recovered {
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
        self.command_found() && self.config.repos.iter().all(|r| Path::new(r).is_dir())
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
        let prompt = render_executor_brief(
            &brief,
            &self.name,
            self.config.kind,
            &dir,
            &self.config.skills_dir,
            turns,
        );
        let last = std::env::temp_dir().join(format!("rustykrab-last-{}.txt", short(&run_id)));
        let cmd = self.command(&prompt, &dir, &brief, &last, None);
        let limit = match brief.budget.wall_seconds {
            0 => self.config.timeout,
            secs => self.config.timeout.min(Duration::from_secs(secs)),
        };
        let started = std::time::Instant::now();
        let (executed, mut interrupted) = self.execute(cmd, limit).await;
        let (mut outcome, resume) = match executed {
            Ok(output) => self.read_output(&brief, &run_id, &output, &last, false),
            Err(e) => (Err(e), None),
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
        let outcome = outcome.map(|mut report| {
            attest(&mut report, &commands);
            report
        });

        // Retention: a run with a result gives its directory back; one
        // without (an interrupted one included) keeps it for diagnosis
        // unless policy says otherwise.
        let keep = outcome.is_err() && matches!(self.config.retention, Retention::KeepFailed(_));
        if !keep {
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
/// with the final message, the turn count and the cost.
fn read_claude(stdout: &str) -> Transcript {
    let mut t = Transcript::default();
    for line in stdout.lines() {
        let Ok(event) = serde_json::from_str::<Value>(line.trim()) else {
            continue;
        };
        if let Some(session) = event["session_id"].as_str().filter(|s| !s.is_empty()) {
            t.session = Some(session.to_string());
        }
        match event["type"].as_str() {
            Some("assistant") => {
                for block in event["message"]["content"].as_array().into_iter().flatten() {
                    if block["type"] == "tool_use" && block["name"] == "Bash" {
                        if let Some(c) = block["input"]["command"].as_str() {
                            t.commands.push(c.to_string());
                        }
                    }
                }
            }
            Some("result") => {
                let turns = event["num_turns"].as_u64().unwrap_or(0);
                let usage = &event["usage"];
                let tokens = [
                    "input_tokens",
                    "output_tokens",
                    "cache_creation_input_tokens",
                ]
                .iter()
                .filter_map(|k| usage[k].as_u64())
                .sum();
                t.usage = RunUsage {
                    tokens,
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
    t
}

/// `codex exec --json`: one JSON event per line. Command executions are the
/// commands it ran; agent messages are its text, the last one its final
/// message (the `--output-last-message` file wins when it is written).
/// Reads both the `item.*` event shape and the older `msg` one.
fn read_codex(stdout: &str) -> Transcript {
    let mut t = Transcript::default();
    let mut turns = 0u32;
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
                if let Some(c) = item["command"].as_str() {
                    t.commands.push(c.to_string());
                }
            }
            (Some("item.completed"), Some("agent_message"), _) => {
                if let Some(text) = item["text"].as_str() {
                    t.final_message = Some(text.to_string());
                }
            }
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
                let command = match &msg["command"] {
                    Value::Array(parts) => parts
                        .iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(" "),
                    Value::String(s) => s.clone(),
                    _ => continue,
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
    t.usage.iterations = turns;
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
            .filter(|e| e.kind != "run" && e.kind != "workspace")
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
         - Follow-up work you notice goes in \"discovered\", one draft each; do not do it.\n\
         - If you cannot finish, set \"blocked\" or \"error\" (class, subclass, detail) \
         instead of guessing. ",
    );
    out.push_str(rustykrab_core::work::BLOCKED_SHAPE_GUIDANCE);
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
        }
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
}
