//! `rustykrab update apply`: the supervisor that swaps the newest staged
//! version in, verifies it and rolls back (slice 6 of
//! `docs/plans/update-flow.md`).
//!
//! The steps: read the newest `staged.json` and the running version from
//! `/api/version`; copy the stage beside the install and check the copy,
//! while the daemon is still up; stop the daemon through its service
//! manager and wait for it to exit; rename the installed bundle or binary to
//! `.prev` and move the copy into place, both in one directory; start it;
//! verify within 90 s that it reports the staged commit, holds
//! `controller.lock`, ticks twice and has no failed ticks. On any failure
//! the new version is recorded in `bad.json`, stopped, `.prev` restored,
//! started and verified the same way. Without `--yes` (or
//! `RUSTYKRAB_UPDATE_AUTO=1`) nothing changes: the plan is printed.
//!
//! A journal (`.<name>.apply-state.json`, beside the install rather than in
//! the data dir a worker can write) records how far an apply got, so the
//! next run can put an interrupted one right before anything else: a new
//! version that is running and verifies is kept, and anything else is
//! rolled back. Before it rolls anything back it checks that the installed
//! and `.prev` binaries report the journal's commits, and `.prev` passes the
//! symlink check (and under launchd the signature check) before it is run
//! or restored. Recovery verifies the service, started or already up, before
//! it clears the journal; a daemon reporting `controller.draining` never
//! passes. A rollback, a recovery or a restart after a failed stop or swap
//! that does not finish writes `.<name>.apply-failed.json` beside the
//! install, saying what is installed, and every later run refuses until a
//! person deletes it.
//!
//! Before anything changes, the installed binary must report the running
//! commit, and `apply` refuses to run inside the daemon's own launchd job
//! or from a binary under the install it swaps.
//!
//! `staged.json` sits in the data dir, which a worker can write, so apply
//! trusts none of it: the record must be canonical, the copy beside the
//! install is checked again (no symlink, the signature under launchd, its own
//! `--version`), a release must be newer than the running version, and the
//! running daemon must be healthy before anything changes.
//!
//! The service manager, the swap root, the version probe, the checks that
//! run the staged binary and the host's processes are traits, so the tests
//! script them and never reach `launchctl` or a real daemon.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use reqwest::Url;
use serde::{Deserialize, Serialize};

use super::{
    is_bad, is_newer, is_plain_version, output_within, parse_semver, parse_version_output,
    record_bad, BadVersion, Config, Staged, Verifier, APP_NAME, BINARY_NAME, BUNDLE_ID,
    STAGED_FILE,
};

/// How long the new version has to come up healthy.
pub const VERIFY_WITHIN: Duration = Duration::from_secs(90);
/// How long a stopped daemon has to exit: its drain is 20 s by default.
const STOP_WITHIN: Duration = Duration::from_secs(60);

/// Which service manager runs the daemon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServiceSpec {
    /// The `com.gcbh.rustykrab` LaunchAgent.
    Launchd,
    /// No manager: SIGTERM the process listening on the daemon's port, and
    /// run this shell command, detached, to start it.
    Script(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplyArgs {
    pub yes: bool,
    pub service: ServiceSpec,
    pub url: Option<String>,
    pub installed: Option<PathBuf>,
}

/// `apply [--yes] [--service launchd|script:<cmd>] [--url URL] [--installed PATH]`.
pub fn parse_args(args: &[String]) -> Result<ApplyArgs, String> {
    let mut parsed = ApplyArgs {
        yes: false,
        service: ServiceSpec::Launchd,
        url: None,
        installed: None,
    };
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--yes" => parsed.yes = true,
            "--service" => {
                let spec = rest
                    .next()
                    .ok_or("--service needs launchd or script:<cmd>")?;
                parsed.service = match spec.as_str() {
                    "launchd" => ServiceSpec::Launchd,
                    other => match other.strip_prefix("script:") {
                        Some(cmd) if !cmd.trim().is_empty() => ServiceSpec::Script(cmd.to_string()),
                        _ => {
                            return Err(format!(
                                "unknown service '{other}': launchd or script:<start-command>"
                            ))
                        }
                    },
                };
            }
            "--url" => parsed.url = Some(rest.next().ok_or("--url needs a URL")?.clone()),
            "--installed" => {
                parsed.installed = Some(PathBuf::from(
                    rest.next().ok_or("--installed needs a path")?,
                ))
            }
            other => return Err(format!("unknown apply argument '{other}'")),
        }
    }
    Ok(parsed)
}

/// Stops and starts the daemon. `stop` returns once the process has exited.
pub trait ServiceManager: Send + Sync {
    /// Whether this is launchd, which only takes a signed bundle.
    fn is_launchd(&self) -> bool;
    fn describe(&self) -> String;
    fn stop(&self) -> anyhow::Result<()>;
    fn start(&self) -> anyhow::Result<()>;
    /// Whether the daemon is up, as the service manager sees it.
    fn running(&self) -> bool;
}

/// Where the installed version lives, and the two renames beside it.
pub trait SwapRoot: Send + Sync {
    fn installed(&self) -> &Path;
    /// Copy `staged` beside the installed version (`.next`, replacing any
    /// left over) and pass the copy through `check`. Nothing installed is
    /// touched, so this runs while the daemon is still up; on an error the
    /// copy is removed.
    fn prepare(&self, staged: &Path, check: &NextCheck<'_>) -> anyhow::Result<()>;
    /// Move the installed version to `.prev`, replacing an older one, and
    /// the prepared copy into its place. On an error the installed version
    /// is put back where it was.
    fn commit(&self) -> anyhow::Result<()>;
    /// Remove a prepared copy that will not be committed.
    fn discard(&self) -> anyhow::Result<()>;
    /// Whether a `.prev` is beside the installed version.
    fn has_prev(&self) -> bool;
    /// Put `.prev` back in place of whatever is installed.
    fn restore(&self) -> anyhow::Result<()>;
    /// `<name>.prev`, beside the installed version.
    fn prev_path(&self) -> anyhow::Result<PathBuf> {
        sibling(self.installed(), "", ".prev")
    }
    /// `.<name>.next`, the prepared copy.
    fn next_path(&self) -> anyhow::Result<PathBuf> {
        sibling(self.installed(), ".", ".next")
    }
    /// `.<name>.<file>`, beside the installed version: where the journal
    /// and the failure record live, out of the data dir a worker can write.
    fn state_path(&self, file: &str) -> anyhow::Result<PathBuf> {
        sibling(self.installed(), ".", &format!(".{file}"))
    }
}

/// Reads `/api/version` from the daemon.
#[async_trait]
pub trait VersionProbe: Send + Sync {
    async fn probe(&self) -> anyhow::Result<VersionReport>;
}

/// The part of `/api/version` the supervisor reads.
#[derive(Debug, Clone, Deserialize)]
pub struct VersionReport {
    pub version: String,
    pub commit: Option<String>,
    #[serde(default)]
    pub controller: ControllerReport,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ControllerReport {
    pub last_tick: Option<DateTime<Utc>>,
    pub consecutive_failed_ticks: Option<u32>,
    pub lock: Option<String>,
    /// Set once the daemon is shutting down. A draining daemon still
    /// answers, but it is on its way out, so it never counts as running.
    pub draining: Option<bool>,
}

/// The host the supervisor acts on, and how patiently.
pub struct Host<'a> {
    pub service: &'a dyn ServiceManager,
    pub swap: &'a dyn SwapRoot,
    pub probe: &'a dyn VersionProbe,
    /// Checks the copy beside the install: its signature and `--version`.
    pub verifier: &'a (dyn Verifier + Sync),
    pub verify_within: Duration,
    pub poll: Duration,
}

/// What `apply` would do, or did.
#[derive(Debug)]
pub struct Plan {
    pub staged: Staged,
    pub running: VersionReport,
    pub installed: PathBuf,
    pub service: String,
}

#[derive(Debug)]
pub enum Outcome {
    /// No `--yes`: nothing changed.
    Planned(Plan),
    /// The staged commit is already the running one.
    AlreadyRunning(Plan),
    /// The new version is in place and healthy; `.prev` is kept.
    Applied(Plan),
    /// The new version failed verification, was replaced by `.prev` and
    /// recorded as bad. `bad_record_error` is set when writing `bad.json`
    /// failed; the rollback went on regardless.
    RolledBack {
        plan: Plan,
        reason: String,
        bad_record_error: Option<String>,
    },
    /// An interrupted apply was found (a journal, or an install path gone
    /// with `.prev` beside it) and put right: finished when its new version
    /// verified, rolled back otherwise. Nothing new was applied.
    Recovered(Recovery),
}

/// What `recover` found and did.
#[derive(Debug, Default)]
pub struct Recovery {
    /// The journal's phase, when there was one.
    pub phase: Option<Phase>,
    /// The install path was missing and `.prev` was moved back.
    pub restored_prev: bool,
    /// The new version was rolled back and recorded as bad.
    pub rolled_back: bool,
    /// The service was not running and was started, and verified.
    pub started: bool,
    /// The journal was at `swapped` or `started` and the new version was
    /// running and verified: the apply had got that far, so only the
    /// journal was cleared.
    pub finished: bool,
    /// Writing `bad.json` failed; the recovery went on regardless.
    pub bad_record_error: Option<String>,
}

/// The journal of an apply in progress, beside the install
/// ([`SwapRoot::state_path`]).
pub const STATE_FILE: &str = "apply-state.json";
/// Written by a rollback that failed; every later apply refuses while it is
/// there.
pub const FAILED_FILE: &str = "apply-failed.json";

/// How far an apply got. Written before the step it names finishes: at
/// `Stopping` nothing is swapped, at `Swapped` the new version is in place,
/// at `Started` it has been started.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Stopping,
    Swapped,
    Started,
}

/// `.<name>.apply-state.json` beside the install: the phase and both
/// commits, so the next run can finish or roll back an apply that was
/// interrupted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Journal {
    pub phase: Phase,
    /// The commit that was running before the apply.
    pub from_commit: String,
    /// The staged commit being applied.
    pub to_commit: String,
    /// What `bad.json` gets if the new version is rolled back.
    pub bad: BadVersion,
    pub at: DateTime<Utc>,
}

/// `.<name>.apply-failed.json` beside the install: a rollback that did not
/// finish.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApplyFailed {
    pub what: String,
    /// Unknown when there was no journal.
    pub from_commit: Option<String>,
    pub to_commit: Option<String>,
    pub at: DateTime<Utc>,
}

/// Write `value` as JSON to the state file `name` beside the install, as a
/// temp file then a rename.
fn write_atomic<T: Serialize>(swap: &dyn SwapRoot, name: &str, value: &T) -> anyhow::Result<()> {
    let path = swap.state_path(name)?;
    let tmp = swap.state_path(&format!("{name}.tmp"))?;
    std::fs::write(&tmp, serde_json::to_string_pretty(value)? + "\n")
        .with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, &path).with_context(|| format!("writing {}", path.display()))
}

fn write_journal(swap: &dyn SwapRoot, journal: &mut Journal, phase: Phase) -> anyhow::Result<()> {
    journal.phase = phase;
    journal.at = Utc::now();
    write_atomic(swap, STATE_FILE, journal).context("writing the apply journal")
}

/// The journal of an interrupted apply, if there is one.
pub fn read_journal(swap: &dyn SwapRoot) -> anyhow::Result<Option<Journal>> {
    let path = swap.state_path(STATE_FILE)?;
    match std::fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text)
            .map(Some)
            .with_context(|| format!("parsing {}; a person must look at it", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

fn clear_journal(swap: &dyn SwapRoot) -> anyhow::Result<()> {
    let path = swap.state_path(STATE_FILE)?;
    match std::fs::remove_file(&path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            Err(e).with_context(|| format!("removing {}", path.display()))
        }
        _ => Ok(()),
    }
}

/// Clear the journal where a failure to do so must not hide the error
/// being reported: it is logged, and the next run recovers from the
/// journal instead.
fn clear_journal_logged(swap: &dyn SwapRoot) {
    if let Err(e) = clear_journal(swap) {
        tracing::error!("{e:#}; the next run will recover from the journal");
    }
}

/// Refuse to do anything while `apply-failed.json` is there: it names a
/// rollback that did not finish, and only a person can say what state the
/// install is in. Its text is in the error, which reaches stderr.
fn refuse_after_failure(swap: &dyn SwapRoot) -> anyhow::Result<()> {
    let path = swap.state_path(FAILED_FILE)?;
    match std::fs::read_to_string(&path) {
        Ok(text) => bail!(
            "{} exists: an earlier rollback did not finish, so apply changes nothing until \
             a person has checked the install and deleted that file. It says:\n{}",
            path.display(),
            text.trim()
        ),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// The canonical `staged.json` under `<data>/updates/` with the highest
/// version, and of those the latest `staged_at`. A record that does not
/// parse or is not canonical ([`check_canonical`]) is skipped with a
/// warning, so a stray record cannot block a good one; `staged_at` is a
/// field a worker writes, so it only breaks a tie. When records were found
/// but every one was skipped, the reasons are the error.
pub fn newest_staged(cfg: &Config) -> anyhow::Result<Option<Staged>> {
    let updates = cfg.updates_dir();
    let entries = match std::fs::read_dir(&updates) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("reading {}", updates.display())),
    };
    let mut newest: Option<(Vec<u64>, Staged)> = None;
    let mut skipped = Vec::new();
    for entry in entries.flatten() {
        let dir = entry.file_name().to_string_lossy().into_owned();
        if dir.starts_with('.') {
            continue;
        }
        let record = entry.path().join(STAGED_FILE);
        let Ok(text) = std::fs::read_to_string(&record) else {
            continue;
        };
        let staged: Staged = match serde_json::from_str(&text) {
            Ok(staged) => staged,
            Err(e) => {
                tracing::warn!("skipping {}: {e}", record.display());
                continue;
            }
        };
        if let Err(e) = check_canonical(cfg, &dir, &staged) {
            tracing::warn!("skipping {}: {e:#}", record.display());
            skipped.push(format!("{e:#}"));
            continue;
        }
        // Canonical, so the version is plain X.Y.Z and parses.
        let Some(version) = parse_semver(&staged.version) else {
            continue;
        };
        if newest
            .as_ref()
            .is_none_or(|(v, n)| (&version, staged.staged_at) > (v, n.staged_at))
        {
            newest = Some((version, staged));
        }
    }
    match newest {
        Some((_, staged)) => Ok(Some(staged)),
        None if skipped.is_empty() => Ok(None),
        None => bail!(
            "no canonical stage under {}; skipped: {}",
            updates.display(),
            skipped.join("; ")
        ),
    }
}

fn is_symlink(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink())
}

/// A record found in `updates/<dir>/` must be where `stage` puts one: `dir`
/// is its plain `X.Y.Z` version, and its path is
/// `updates/<version>/RustyKrab.app` (kind `app`) or
/// `updates/<version>/rustykrab-cli` (kind `binary`), with no symlink on the
/// way.
pub fn check_canonical(cfg: &Config, dir: &str, staged: &Staged) -> anyhow::Result<()> {
    if !is_plain_version(&staged.version) || dir != staged.version {
        bail!(
            "updates/{dir}/{STAGED_FILE} names version {:?}; a record must sit in \
             updates/<its X.Y.Z version>/, so it is refused",
            staged.version
        );
    }
    let name = match staged.kind.as_str() {
        "app" => APP_NAME,
        "binary" => BINARY_NAME,
        other => bail!(
            "staged {} is kind {other:?}, neither app nor binary; refusing it",
            staged.version
        ),
    };
    let updates = cfg.updates_dir();
    let version_dir = updates.join(&staged.version);
    let expected = version_dir.join(name);
    if staged.path != expected {
        bail!(
            "staged {} records path {}, not the canonical {}; refusing it",
            staged.version,
            staged.path.display(),
            expected.display()
        );
    }
    let mut on_the_way = vec![updates, version_dir, expected.clone()];
    if staged.kind == "app" {
        let macos = expected.join("Contents").join("MacOS");
        on_the_way.extend([expected.join("Contents"), macos.join(BINARY_NAME), macos]);
    }
    if let Some(link) = on_the_way.iter().find(|p| is_symlink(p)) {
        bail!("{} is a symbolic link; refusing the stage", link.display());
    }
    let is_right_type = if staged.kind == "app" {
        expected.is_dir()
    } else {
        expected.is_file()
    };
    if !is_right_type {
        bail!(
            "the staged {} is missing or not a {}",
            expected.display(),
            if staged.kind == "app" {
                "directory"
            } else {
                "file"
            }
        );
    }
    Ok(())
}

/// What the copy beside the install must be before it replaces anything:
/// the checks [`SwapRoot::prepare`] runs on it.
pub struct NextCheck<'a> {
    pub verifier: &'a (dyn Verifier + Sync),
    /// The Developer ID team the copy must be signed by: set under launchd,
    /// which only takes a signed bundle.
    pub team_id: Option<&'a str>,
    pub version: &'a str,
    pub commit: &'a str,
}

impl NextCheck<'_> {
    /// `next` must be a real directory (`app`) or regular file, not a
    /// symlink; under launchd its signature must verify; and its binary's
    /// `--version` must report the staged version and commit.
    pub fn check(&self, next: &Path, app: bool) -> anyhow::Result<()> {
        let binary = check_no_symlink(next, app)?;
        if let Some(team) = self.team_id {
            self.verifier
                .verify_signature(next, team)
                .context("signature check refused the copy of the stage")?;
        }
        let printed = self.verifier.run_version(&binary)?;
        let (version, commit) = parse_version_output(&printed).ok_or_else(|| {
            anyhow!(
                "{} --version printed no version: {:?}",
                binary.display(),
                printed.trim()
            )
        })?;
        if version != self.version || commit.as_deref() != Some(self.commit) {
            bail!(
                "the copy of the stage reports {version} ({}), the record says {} ({}); \
                 refusing it",
                commit.as_deref().unwrap_or("no commit"),
                self.version,
                self.commit
            );
        }
        Ok(())
    }
}

/// `path` must be a real directory (`app`) or regular file, with no symlink
/// down to the binary it runs. Returns that binary.
fn check_no_symlink(path: &Path, app: bool) -> anyhow::Result<PathBuf> {
    let meta =
        std::fs::symlink_metadata(path).with_context(|| format!("reading {}", path.display()))?;
    if app {
        if !meta.is_dir() {
            bail!("{} is not a directory; refusing it", path.display());
        }
        let macos = path.join("Contents").join("MacOS");
        let binary = macos.join(BINARY_NAME);
        for p in [&path.join("Contents"), &macos, &binary] {
            if is_symlink(p) {
                bail!("{} is a symbolic link; refusing it", p.display());
            }
        }
        Ok(binary)
    } else {
        if !meta.is_file() {
            bail!("{} is not a regular file; refusing it", path.display());
        }
        Ok(path.to_path_buf())
    }
}

/// Before `.prev` is run or restored: the symlink check [`NextCheck`] runs
/// on the copy, and under launchd its signature check. `.prev` sits beside
/// the install, so it is as writable as the install and is checked again.
fn check_prev(cfg: &Config, host: &Host<'_>) -> anyhow::Result<()> {
    let prev = host.swap.prev_path()?;
    // A bundle is installed as a directory; `.prev` must be the same kind.
    let app = std::fs::symlink_metadata(host.swap.installed())
        .or_else(|_| std::fs::symlink_metadata(&prev))
        .with_context(|| format!("reading {}", prev.display()))?
        .is_dir();
    check_no_symlink(&prev, app).context("the .prev beside the install")?;
    if host.service.is_launchd() {
        host.verifier
            .verify_signature(&prev, &cfg.team_id)
            .context("signature check refused .prev")?;
    }
    Ok(())
}

/// The `bad.json` entry for a stage: a release by its version, a local
/// build (no tag) by its commit, since every local build reports the same
/// package version.
pub fn bad_entry(staged: &Staged) -> BadVersion {
    BadVersion {
        version: staged.version.clone(),
        commit: match staged.tag {
            Some(_) => None,
            None => staged.commit.clone(),
        },
    }
}

/// `rustykrab update apply`, against `host`.
///
/// It refuses while `apply-failed.json` is there, and first puts right an
/// apply that was interrupted ([`recover`]); a run that recovers applies
/// nothing new. Recovery runs without `--yes`: it only finishes what a
/// `--yes` run started.
pub async fn apply(cfg: &Config, host: &Host<'_>, yes: bool) -> anyhow::Result<Outcome> {
    refuse_after_failure(host.swap)?;
    if let Some(recovery) = recover(cfg, host).await? {
        return Ok(Outcome::Recovered(recovery));
    }
    let staged = newest_staged(cfg)?.ok_or_else(|| {
        anyhow!(
            "nothing staged under {}; run `rustykrab update stage` first",
            cfg.updates_dir().display()
        )
    })?;
    let Some(commit) = staged.commit.clone() else {
        bail!(
            "staged {} records no commit, so it cannot be verified; refusing it",
            staged.version
        );
    };
    if host.service.is_launchd() && (staged.kind != "app" || !staged.signature_verified) {
        bail!(
            "launchd only takes a {APP_NAME} whose Developer ID signature was verified; \
             staged {} is kind {:?} with signature_verified {}",
            staged.version,
            staged.kind,
            staged.signature_verified
        );
    }
    // Off launchd nothing checks a signature, so only a bare binary is
    // swapped: a bundle is what launchd runs.
    if !host.service.is_launchd() && staged.kind != "binary" {
        bail!(
            "{} is only for a bare binary launchd does not run; staged {} is kind {:?}, so it \
             is refused and nothing was stopped",
            host.service.describe(),
            staged.version,
            staged.kind
        );
    }
    if is_bad(cfg, &staged.version, Some(&commit))? {
        bail!(
            "staged {} ({commit}) is recorded as bad by a rollback; not applying it again",
            staged.version
        );
    }
    let installed = host.swap.installed().to_path_buf();
    if std::fs::symlink_metadata(&installed).is_err() {
        bail!("nothing is installed at {}", installed.display());
    }
    if installed.is_dir() != (staged.kind == "app") {
        bail!(
            "staged {} is kind {:?} but {} is {}",
            staged.version,
            staged.kind,
            installed.display(),
            if installed.is_dir() {
                "a directory"
            } else {
                "a file"
            }
        );
    }
    let running = host
        .probe
        .probe()
        .await
        .context("reading the running version from /api/version")?;
    let Some(running_commit) = running.commit.clone() else {
        bail!("the running daemon reports no commit, so a rollback could not be verified");
    };
    let plan = Plan {
        staged,
        running,
        installed,
        service: host.service.describe(),
    };
    // Checked before the staged commit is taken as running: a daemon of that
    // commit that does not hold the lock or fails ticks is not "applied".
    let controller = &plan.running.controller;
    if controller.lock.as_deref() != Some("held")
        || controller.consecutive_failed_ticks != Some(0)
        || controller.draining == Some(true)
    {
        bail!(
            "the running daemon is not healthy (controller.lock {}, consecutive_failed_ticks {}, \
             draining {}); an update is only applied over a daemon that holds the lock, is not \
             failing ticks and is not shutting down, so a rollback has a healthy version to \
             return to",
            controller.lock.as_deref().unwrap_or("(unknown)"),
            controller
                .consecutive_failed_ticks
                .map_or("(unknown)".to_string(), |n| n.to_string()),
            controller.draining.unwrap_or(false)
        );
    }
    if running_commit == commit {
        return Ok(Outcome::AlreadyRunning(plan));
    }
    // `tag` is a field of staged.json, which a worker can write, so it
    // cannot decide whether the version check applies: every stage must be
    // at least the running version (an older signed release copied in as a
    // "local build" is a downgrade), and a release must be newer.
    if is_newer(&plan.running.version, &plan.staged.version)? {
        bail!(
            "staged {} is older than the running {}; refusing a downgrade",
            plan.staged.version,
            plan.running.version
        );
    }
    if plan.staged.tag.is_some() && !is_newer(&plan.staged.version, &plan.running.version)? {
        bail!(
            "staged release {} is not newer than the running {}; refusing it",
            plan.staged.version,
            plan.running.version
        );
    }
    if !yes {
        return Ok(Outcome::Planned(plan));
    }

    // What is installed becomes `.prev`, and a rollback returns to it, so it
    // must be the version that is running now.
    binary_reports(host, &plan.installed, &running_commit).context(
        "the installed binary is not the running daemon's commit, so a rollback would not \
         return to it; the daemon was not stopped",
    )?;
    let check = NextCheck {
        verifier: host.verifier,
        team_id: host.service.is_launchd().then_some(cfg.team_id.as_str()),
        version: &plan.staged.version,
        commit: &commit,
    };
    // The copy is checked while the daemon is still up, so a stage that
    // fails its checks never takes it down.
    host.swap
        .prepare(&plan.staged.path, &check)
        .context("checking the copy of the stage; the daemon was not stopped")?;
    let mut journal = Journal {
        phase: Phase::Stopping,
        from_commit: running_commit,
        to_commit: commit.clone(),
        bad: bad_entry(&plan.staged),
        at: Utc::now(),
    };
    if let Err(e) = write_journal(host.swap, &mut journal, Phase::Stopping) {
        let _ = host.swap.discard();
        return Err(e.context("the daemon was not stopped"));
    }
    if let Err(e) = host.service.stop() {
        // Nothing is swapped, so the old version is still installed: make
        // sure it runs. The journal is cleared only once `/api/version`
        // answers with the old commit; otherwise the failure is recorded
        // ([`fail_loudly`]).
        let _ = host.swap.discard();
        let restarted = match start_unless_running(host.service) {
            Ok(()) => wait_for_commit(host, &journal.from_commit).await,
            Err(e) => Err(e.context("starting the old version again")),
        };
        return Err(match restarted {
            Ok(()) => {
                clear_journal_logged(host.swap);
                e.context("stopping the daemon; the old version is running again")
            }
            Err(r) => fail_loudly(
                host,
                Some(&journal),
                e.context(format!(
                    "stopping the daemon failed, and the old version was not seen running \
                     again ({r:#})"
                )),
                None,
            ),
        });
    }
    if let Err(e) = host.swap.commit() {
        let _ = host.swap.discard();
        // `commit` puts the installed version back, but that rename can fail
        // too: with nothing installed there is nothing to start.
        if std::fs::symlink_metadata(&plan.installed).is_err() {
            return Err(fail_loudly(
                host,
                Some(&journal),
                e.context(format!(
                    "swapping the staged version in failed and left nothing at {}",
                    plan.installed.display()
                )),
                None,
            ));
        }
        // As after a failed stop, the journal is cleared only once
        // `/api/version` answers with the old commit, and the failure is
        // recorded otherwise.
        let restarted = match host.service.start() {
            Ok(()) => wait_for_commit(host, &journal.from_commit).await,
            Err(e) => Err(e.context("starting the old version again")),
        };
        return Err(match restarted {
            Ok(()) => {
                clear_journal_logged(host.swap);
                e.context("swapping the staged version in; the old version is running again")
            }
            Err(r) => fail_loudly(
                host,
                Some(&journal),
                e.context(format!(
                    "swapping the staged version in failed, and the old version was not seen \
                     running again ({r:#})"
                )),
                None,
            ),
        });
    }
    let brought_up: anyhow::Result<()> = async {
        write_journal(host.swap, &mut journal, Phase::Swapped)?;
        host.service.start().context("starting the new version")?;
        write_journal(host.swap, &mut journal, Phase::Started)?;
        verify(host, &commit).await
    }
    .await;
    let reason = match brought_up {
        Ok(()) => {
            clear_journal(host.swap).context(
                "the new version is applied and verified, but its journal is left at \
                 `started`, so the next run would roll it back; remove the journal by hand",
            )?;
            return Ok(Outcome::Applied(plan));
        }
        Err(e) => format!("{e:#}"),
    };
    let bad_record_error = roll_back(cfg, host, &journal)
        .await
        .with_context(|| format!("rolling back after: {reason}"))?;
    Ok(Outcome::RolledBack {
        plan,
        reason,
        bad_record_error,
    })
}

/// Start the service unless it is already up: a second start of a daemon
/// that is running would at best fail and at worst run two.
fn start_unless_running(service: &dyn ServiceManager) -> anyhow::Result<()> {
    if service.running() {
        return Ok(());
    }
    service.start()
}

/// Record the journal's new version as bad. A failure is logged and
/// returned, never raised: it must not stop a rollback.
fn record_bad_logged(cfg: &Config, journal: &Journal) -> Option<String> {
    record_bad(cfg, journal.bad.clone()).err().map(|e| {
        tracing::error!(
            "recording {} ({}) as bad: {e:#}",
            journal.bad.version,
            journal.to_commit
        );
        format!("{e:#}")
    })
}

/// `path`'s binary (the one inside a bundle) must report `commit` from its
/// `--version`.
fn binary_reports(host: &Host<'_>, path: &Path, commit: &str) -> anyhow::Result<()> {
    let binary = installed_executable(path);
    let printed = host.verifier.run_version(&binary)?;
    match parse_version_output(&printed) {
        Some((_, Some(reported))) if reported == commit => Ok(()),
        _ => bail!(
            "{} reports {:?}, not commit {commit}",
            binary.display(),
            printed.trim()
        ),
    }
}

/// A rollback already put `.prev` back: no `.prev` is left and the
/// installed binary reports the journal's `from_commit`.
fn already_rolled_back(host: &Host<'_>, journal: &Journal) -> bool {
    !host.swap.has_prev()
        && binary_reports(host, host.swap.installed(), &journal.from_commit).is_ok()
}

/// Before a recovery rolls anything back: the installed binary must report
/// the journal's `to_commit` and `.prev` its `from_commit`, or a rollback
/// must already have put `.prev` back. A journal that matches neither was
/// not written by the apply that left these files, so nothing is stopped.
fn check_rollback(cfg: &Config, host: &Host<'_>, journal: &Journal) -> anyhow::Result<()> {
    if already_rolled_back(host, journal) {
        return Ok(());
    }
    let matched = binary_reports(host, host.swap.installed(), &journal.to_commit).and_then(|()| {
        check_prev(cfg, host)?;
        binary_reports(host, &host.swap.prev_path()?, &journal.from_commit)
            .context("the .prev binary")
    });
    matched.context(format!(
        "the journal names {} over {}, which the installed and .prev binaries do not \
         match; nothing was stopped",
        journal.to_commit, journal.from_commit
    ))
}

/// Stop the new version, put `.prev` back, start it and verify it reports
/// `journal.from_commit`, then clear the journal. The new version is
/// recorded bad first, so it is not tried again even if what follows fails;
/// a failure to record it is returned with the outcome and the rollback goes
/// on. When a rollback that crashed late already put `.prev` back, it is not
/// stopped or restored again, only started if need be and verified. Any
/// other failure writes `apply-failed.json` ([`fail_loudly`]).
async fn roll_back(
    cfg: &Config,
    host: &Host<'_>,
    journal: &Journal,
) -> anyhow::Result<Option<String>> {
    let bad_record_error = record_bad_logged(cfg, journal);
    let steps: anyhow::Result<()> = async {
        if already_rolled_back(host, journal) {
            start_unless_running(host.service).context("starting the previous version")?;
            return verify(host, &journal.from_commit)
                .await
                .context("verifying the previous version");
        }
        // Checked before the new version is stopped, so a `.prev` that
        // fails leaves the new version up rather than nothing.
        check_prev(cfg, host).context(
            "restoring the previous version: .prev failed its checks, so the new version was \
             not stopped",
        )?;
        if host.service.running() {
            if let Err(first) = host.service.stop() {
                tracing::warn!("stopping the new version for the rollback: {first:#}; once more");
                // Files are never swapped under a running daemon.
                host.service.stop().map_err(|e| {
                    anyhow!(
                        "stopping the new version for the rollback failed twice ({first:#}; \
                         then {e:#}), so the previous version was not restored under it"
                    )
                })?;
            }
        }
        host.swap
            .restore()
            .context("restoring the previous version")?;
        host.service
            .start()
            .context("starting the previous version")?;
        verify(host, &journal.from_commit)
            .await
            .context("verifying the previous version")
    }
    .await;
    match steps {
        Ok(()) => {
            clear_journal(host.swap)?;
            Ok(bad_record_error)
        }
        Err(e) => Err(fail_loudly(
            host,
            Some(journal),
            e,
            bad_record_error.as_deref(),
        )),
    }
}

/// Which version is installed, as far as `fail_loudly` can tell.
enum Installed {
    Nothing,
    Previous,
    New,
    Unknown,
}

/// What is installed, against the journal's two commits. `None` without a
/// journal, when there is nothing to compare with.
fn identify_installed(host: &Host<'_>, journal: Option<&Journal>) -> Option<Installed> {
    let journal = journal?;
    let installed = host.swap.installed();
    Some(if std::fs::symlink_metadata(installed).is_err() {
        Installed::Nothing
    } else if binary_reports(host, installed, &journal.from_commit).is_ok() {
        Installed::Previous
    } else if binary_reports(host, installed, &journal.to_commit).is_ok() {
        Installed::New
    } else {
        Installed::Unknown
    })
}

/// A rollback or recovery that did not finish: write `apply-failed.json`
/// (what failed, what is installed, both commits, the time), clear the
/// journal, since a person takes over from here, and try to start the
/// service last. It is not started when the installed binary reports
/// neither of the journal's commits: an unidentified binary is never
/// started. Returns the error to report, with whatever of that also failed.
///
/// Without a journal (it could not be read) nothing is cleared: the caller
/// keeps an unreadable journal aside for the person who takes over.
fn fail_loudly(
    host: &Host<'_>,
    journal: Option<&Journal>,
    error: anyhow::Error,
    bad_record_error: Option<&str>,
) -> anyhow::Error {
    let installed = identify_installed(host, journal);
    let mut what = format!("{error:#}");
    if let Some(bad) = bad_record_error {
        what.push_str(&format!(
            "; recording the new version as bad also failed: {bad}"
        ));
    }
    // Say what is in place, not what was meant to happen.
    let path = host.swap.installed().display();
    let state = match (&installed, journal) {
        (None, _) | (_, None) => {
            "with no readable journal, nothing was rolled back and which version is installed \
             is not known"
                .to_string()
        }
        (Some(Installed::Nothing), Some(_)) => format!("nothing is installed at {path}"),
        (Some(Installed::Previous), Some(j)) => format!(
            "the previous version ({}) is installed at {path}",
            j.from_commit
        ),
        (Some(Installed::New), Some(j)) => format!(
            "the new version ({}) is still installed at {path}; the previous version ({}) \
             was not restored",
            j.to_commit, j.from_commit
        ),
        (Some(Installed::Unknown), Some(j)) => format!(
            "the binary installed at {path} reports neither {} nor {}, so nothing was \
             restored and it was not started",
            j.from_commit, j.to_commit
        ),
    };
    what.push_str(&format!("; {state}"));
    let record = ApplyFailed {
        what: what.clone(),
        from_commit: journal.map(|j| j.from_commit.clone()),
        to_commit: journal.map(|j| j.to_commit.clone()),
        at: Utc::now(),
    };
    let mut message = format!("apply did not finish: {what}");
    match write_atomic(host.swap, FAILED_FILE, &record) {
        Ok(()) => message.push_str(&format!(
            "; recorded in {}, and apply refuses until a person deletes it",
            host.swap
                .state_path(FAILED_FILE)
                .map_or_else(|_| FAILED_FILE.to_string(), |p| p.display().to_string())
        )),
        Err(e) => message.push_str(&format!("; writing {FAILED_FILE} failed too: {e:#}")),
    }
    if journal.is_some() {
        if let Err(e) = clear_journal(host.swap) {
            message.push_str(&format!("; {e:#}"));
        }
    }
    if matches!(installed, Some(Installed::Unknown)) {
        message.push_str(if host.service.running() {
            "; the service is running"
        } else {
            "; the service is not running"
        });
    } else {
        match start_unless_running(host.service) {
            Ok(()) => message.push_str("; the service was left running"),
            Err(e) => message.push_str(&format!("; starting the service failed: {e:#}")),
        }
    }
    tracing::error!("{message}");
    anyhow!(message)
}

/// Move an unreadable journal aside to `.<name>.apply-state.json.unreadable`,
/// so a person can still read it. Returns where it went.
fn keep_unreadable_journal(swap: &dyn SwapRoot) -> anyhow::Result<PathBuf> {
    let path = swap.state_path(STATE_FILE)?;
    let kept = swap.state_path(&format!("{STATE_FILE}.unreadable"))?;
    std::fs::rename(&path, &kept)
        .with_context(|| format!("moving {} to {}", path.display(), kept.display()))?;
    Ok(kept)
}

/// The commit the installed binary's `--version` reports.
fn installed_commit(host: &Host<'_>) -> anyhow::Result<String> {
    let binary = installed_executable(host.swap.installed());
    let printed = host.verifier.run_version(&binary)?;
    match parse_version_output(&printed) {
        Some((_, Some(commit))) => Ok(commit),
        _ => bail!(
            "{} reports no commit: {:?}",
            binary.display(),
            printed.trim()
        ),
    }
}

/// Put right an apply that did not finish, before anything else:
///
/// - the install path missing with `.prev` beside it: check `.prev`
///   ([`check_prev`]) and move it back;
/// - a journal at `swapped` or `started` whose new version is running and
///   passes [`verify`]: the apply got that far, so the journal is cleared
///   and nothing is rolled back;
/// - otherwise a journal at `swapped` or `started`, or at `stopping` with
///   the copy gone and the installed binary not reporting `from_commit` (a
///   crash between the renames and the `swapped` write, or a `--version`
///   that did not answer): the new version may be in place, so roll it back
///   in full and record it bad, once the installed and `.prev` binaries are
///   seen to report the journal's commits ([`check_rollback`]);
/// - otherwise a journal at `stopping`: nothing was swapped; drop the copy;
/// - then, if the service is not running, start it; either way verify it
///   reports the journal's `from_commit` (without a journal, the commit the
///   installed binary reports), and only then clear the journal. A service
///   that counts as running may be a daemon in its shutdown drain, which
///   [`verify`] refuses.
///
/// `None` when there was nothing to recover. A failure, including a journal
/// that cannot be read or does not match the binaries, and a service that
/// fails verify, writes `apply-failed.json`. An unreadable
/// journal is kept as `.<name>.apply-state.json.unreadable`.
pub async fn recover(cfg: &Config, host: &Host<'_>) -> anyhow::Result<Option<Recovery>> {
    let journal = match read_journal(host.swap) {
        Ok(journal) => journal,
        Err(e) => {
            let e = match keep_unreadable_journal(host.swap) {
                Ok(kept) => e.context(format!("it is kept as {}", kept.display())),
                Err(r) => e.context(format!(
                    "moving it aside failed too ({r:#}); it is left in place"
                )),
            };
            return Err(fail_loudly(host, None, e, None));
        }
    };
    let installed_missing = std::fs::symlink_metadata(host.swap.installed()).is_err();
    let restore_prev = installed_missing && host.swap.has_prev();
    if journal.is_none() && !restore_prev {
        return Ok(None);
    }
    let mut recovery = Recovery {
        phase: journal.as_ref().map(|j| j.phase),
        ..Recovery::default()
    };
    tracing::warn!(
        "recovering an interrupted apply (journal phase {:?}, install path missing {})",
        recovery.phase,
        installed_missing
    );
    if restore_prev {
        let restored = check_prev(cfg, host).and_then(|()| host.swap.restore());
        if let Err(e) = restored {
            return Err(fail_loudly(
                host,
                journal.as_ref(),
                e.context("the install path is missing; restoring .prev"),
                None,
            ));
        }
        recovery.restored_prev = true;
    }
    if let Some(journal) = &journal {
        let swapped = match journal.phase {
            Phase::Swapped | Phase::Started => true,
            // Only a binary seen to report `from_commit` counts as not
            // swapped: a `--version` that fails or times out is taken as
            // swapped, so the checks below run before anything is started.
            Phase::Stopping => {
                let next_gone = host
                    .swap
                    .next_path()
                    .is_ok_and(|next| std::fs::symlink_metadata(next).is_err());
                next_gone
                    && binary_reports(host, host.swap.installed(), &journal.from_commit).is_err()
            }
        };
        let finished = matches!(journal.phase, Phase::Swapped | Phase::Started)
            && !recovery.restored_prev
            && host.service.running()
            && binary_reports(host, host.swap.installed(), &journal.to_commit).is_ok()
            && match verify(host, &journal.to_commit).await {
                Ok(()) => true,
                Err(e) => {
                    tracing::warn!("the new version is running but not healthy: {e:#}");
                    false
                }
            };
        if finished {
            clear_journal(host.swap)?;
            recovery.finished = true;
            return Ok(Some(recovery));
        }
        if swapped {
            let checked = if recovery.restored_prev {
                // `.prev` is already back in place.
                binary_reports(host, host.swap.installed(), &journal.from_commit)
                    .context("the journal does not match the restored .prev")
            } else {
                check_rollback(cfg, host, journal)
            };
            if let Err(e) = checked {
                return Err(fail_loudly(host, Some(journal), e, None));
            }
            recovery.bad_record_error = if recovery.restored_prev {
                record_bad_logged(cfg, journal)
            } else {
                roll_back(cfg, host, journal).await?
            };
            recovery.rolled_back = true;
        } else {
            let _ = host.swap.discard();
        }
    }
    if !host.service.running() {
        if let Err(e) = host.service.start() {
            return Err(fail_loudly(
                host,
                journal.as_ref(),
                e.context("starting the service after recovering an interrupted apply"),
                recovery.bad_record_error.as_deref(),
            ));
        }
        recovery.started = true;
    }
    // Whether it was started here or was already up, the journal is cleared
    // only once the service answers healthy with the commit it was put back
    // to: `start` returning is not the service being up, and a daemon the
    // service manager counts as running may be in its shutdown drain.
    let verified = match &journal {
        Some(journal) => verify(host, &journal.from_commit).await,
        None => match installed_commit(host) {
            Ok(commit) => verify(host, &commit).await,
            Err(e) => Err(e),
        },
    };
    if let Err(e) = verified {
        let what = if recovery.started {
            "verifying the service started after recovering an interrupted apply"
        } else {
            "verifying the running service after recovering an interrupted apply"
        };
        return Err(fail_loudly(
            host,
            journal.as_ref(),
            e.context(what),
            recovery.bad_record_error.as_deref(),
        ));
    }
    clear_journal(host.swap)?;
    Ok(Some(recovery))
}

/// Within `host.verify_within`, `/api/version` must report `commit`,
/// `controller.lock` `held`, a `last_tick` that advances twice and no
/// consecutive failed ticks, and must not be draining. An advance counts
/// only while no ticks are failing, and a failing tick or a drain starts the
/// count again: a tick that ran and failed is not progress.
pub async fn verify(host: &Host<'_>, commit: &str) -> anyhow::Result<()> {
    let deadline = Instant::now() + host.verify_within;
    let mut last_tick: Option<DateTime<Utc>> = None;
    let mut advances = 0;
    loop {
        let problem = match host.probe.probe().await {
            Err(e) => format!("no reply: {e:#}"),
            Ok(r) if r.commit.as_deref() != Some(commit) => format!(
                "it reports commit {}, not {commit}",
                r.commit.as_deref().unwrap_or("(none)")
            ),
            Ok(r) if r.controller.lock.as_deref() != Some("held") => format!(
                "controller.lock is {}, not held",
                r.controller.lock.as_deref().unwrap_or("(unknown)")
            ),
            // A daemon in its shutdown drain still answers and may still
            // hold the lock, but it is about to exit.
            Ok(r) if r.controller.draining == Some(true) => {
                advances = 0;
                "it is draining, shutting down".to_string()
            }
            Ok(r) => {
                let healthy = r.controller.consecutive_failed_ticks == Some(0);
                if !healthy {
                    advances = 0;
                }
                if let Some(tick) = r.controller.last_tick {
                    match last_tick {
                        Some(prev) if tick > prev && healthy => advances += 1,
                        _ => {}
                    }
                    last_tick = Some(tick);
                }
                match r.controller.consecutive_failed_ticks {
                    Some(0) if advances >= 2 => return Ok(()),
                    Some(0) => format!("last_tick advanced {advances} of 2 times"),
                    other => format!(
                        "consecutive_failed_ticks is {}",
                        other.map_or("(unknown)".to_string(), |n| n.to_string())
                    ),
                }
            }
        };
        if Instant::now() >= deadline {
            bail!(
                "not healthy within {}s: {problem}",
                host.verify_within.as_secs()
            );
        }
        tokio::time::sleep(host.poll).await;
    }
}

/// Within `host.verify_within`, `/api/version` must answer with `commit`,
/// not draining. Less than [`verify`]: it only shows which version is up.
async fn wait_for_commit(host: &Host<'_>, commit: &str) -> anyhow::Result<()> {
    let deadline = Instant::now() + host.verify_within;
    loop {
        let problem = match host.probe.probe().await {
            Ok(r) if r.controller.draining == Some(true) => format!(
                "it reports commit {} but is draining, shutting down",
                r.commit.as_deref().unwrap_or("(none)")
            ),
            Ok(r) if r.commit.as_deref() == Some(commit) => return Ok(()),
            Ok(r) => format!(
                "it reports commit {}",
                r.commit.as_deref().unwrap_or("(none)")
            ),
            Err(e) => format!("no reply: {e:#}"),
        };
        if Instant::now() >= deadline {
            bail!(
                "not answering with {commit} within {}s: {problem}",
                host.verify_within.as_secs()
            );
        }
        tokio::time::sleep(host.poll).await;
    }
}

/// `path` with `suffix` on its file name, in the same directory.
fn sibling(path: &Path, prefix: &str, suffix: &str) -> anyhow::Result<PathBuf> {
    let name = path
        .file_name()
        .ok_or_else(|| anyhow!("{} has no file name", path.display()))?;
    Ok(path.with_file_name(format!("{prefix}{}{suffix}", name.to_string_lossy())))
}

fn remove_any(path: &Path) -> anyhow::Result<()> {
    let result = match std::fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => Err(e),
        Ok(m) if m.is_dir() => std::fs::remove_dir_all(path),
        Ok(_) => std::fs::remove_file(path),
    };
    result.with_context(|| format!("removing {}", path.display()))
}

/// The real swap: the installed bundle or binary and its `.prev` beside it.
pub struct DirSwap {
    pub installed: PathBuf,
}

impl SwapRoot for DirSwap {
    fn installed(&self) -> &Path {
        &self.installed
    }

    fn prepare(&self, staged: &Path, check: &NextCheck<'_>) -> anyhow::Result<()> {
        // The stage may be on another volume; copy it next to the installed
        // version first, so both moves in `commit` are renames in one
        // directory.
        let next = sibling(&self.installed, ".", ".next")?;
        let app = std::fs::symlink_metadata(&self.installed)
            .with_context(|| format!("reading {}", self.installed.display()))?
            .is_dir();
        remove_any(&next)?;
        let copy = Command::new("cp")
            .arg("-Rp")
            .arg("--")
            .arg(staged)
            .arg(&next)
            .output()
            .context("running cp")?;
        if !copy.status.success() {
            let _ = remove_any(&next);
            bail!(
                "copying {}: {}",
                staged.display(),
                String::from_utf8_lossy(&copy.stderr).trim()
            );
        }
        // What is checked is the copy that will be renamed into place, not
        // the stage a worker could still change.
        if let Err(e) = check.check(&next, app) {
            let _ = remove_any(&next);
            return Err(e);
        }
        Ok(())
    }

    fn commit(&self) -> anyhow::Result<()> {
        let next = sibling(&self.installed, ".", ".next")?;
        let prev = sibling(&self.installed, "", ".prev")?;
        if std::fs::symlink_metadata(&next).is_err() {
            bail!("no prepared {} to move into place", next.display());
        }
        remove_any(&prev)?;
        std::fs::rename(&self.installed, &prev)
            .with_context(|| format!("moving {} to .prev", self.installed.display()))?;
        if let Err(e) = std::fs::rename(&next, &self.installed) {
            let _ = std::fs::rename(&prev, &self.installed);
            let _ = remove_any(&next);
            return Err(e)
                .with_context(|| format!("moving the stage to {}", self.installed.display()));
        }
        Ok(())
    }

    fn discard(&self) -> anyhow::Result<()> {
        remove_any(&sibling(&self.installed, ".", ".next")?)
    }

    fn has_prev(&self) -> bool {
        sibling(&self.installed, "", ".prev")
            .is_ok_and(|prev| std::fs::symlink_metadata(prev).is_ok())
    }

    fn restore(&self) -> anyhow::Result<()> {
        let prev = sibling(&self.installed, "", ".prev")?;
        if std::fs::symlink_metadata(&prev).is_err() {
            bail!("no {} to restore", prev.display());
        }
        let failed = sibling(&self.installed, ".", ".failed")?;
        remove_any(&failed)?;
        if std::fs::symlink_metadata(&self.installed).is_ok() {
            std::fs::rename(&self.installed, &failed)
                .with_context(|| format!("moving {} aside", self.installed.display()))?;
        }
        std::fs::rename(&prev, &self.installed)
            .with_context(|| format!("restoring {}", prev.display()))?;
        remove_any(&failed)
    }
}

/// `/api/version` over HTTP, with the bearer token and the daemon's origin.
pub struct HttpProbe {
    url: Url,
    client: reqwest::Client,
}

impl HttpProbe {
    pub fn new(base: &Url, token: &str) -> anyhow::Result<Self> {
        let client = crate::daemon_client::client(base, token, Duration::from_secs(5))?;
        let url = base
            .join("/api/version")
            .context("building the /api/version URL")?;
        Ok(Self { url, client })
    }
}

#[async_trait]
impl VersionProbe for HttpProbe {
    async fn probe(&self) -> anyhow::Result<VersionReport> {
        let resp = self
            .client
            .get(self.url.clone())
            .send()
            .await
            .with_context(|| format!("GET {}", self.url))?;
        if !resp.status().is_success() {
            bail!("GET {}: {}", self.url, resp.status());
        }
        resp.json().await.context("reading /api/version")
    }
}

/// Run `cmd`, failing with its stderr when it exits non-zero.
fn run_checked(mut cmd: Command, what: &str) -> anyhow::Result<String> {
    let out = cmd.output().with_context(|| format!("running {what}"))?;
    if !out.status.success() {
        bail!(
            "{what} exited with {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Poll `gone` until it holds or `within` passes.
fn wait_until(what: &str, within: Duration, gone: impl Fn() -> bool) -> anyhow::Result<()> {
    let deadline = Instant::now() + within;
    while !gone() {
        if Instant::now() >= deadline {
            bail!("{what} did not exit within {}s", within.as_secs());
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    Ok(())
}

/// Boot a launchd job out and wait for it to go. `bootout` can return an
/// error while the job is still draining, so its error only counts when the
/// job is still loaded after `within`.
fn stop_job(
    what: &str,
    bootout: impl FnOnce() -> anyhow::Result<()>,
    loaded: impl Fn() -> bool,
    within: Duration,
) -> anyhow::Result<()> {
    let booted = bootout();
    match (wait_until(what, within, || !loaded()), booted) {
        (Ok(()), Ok(())) => Ok(()),
        (Ok(()), Err(e)) => {
            tracing::warn!("{e:#}; {what} exited all the same");
            Ok(())
        }
        (Err(waited), Ok(())) => Err(waited),
        (Err(waited), Err(e)) => Err(e.context(format!("{waited:#}"))),
    }
}

/// The `com.gcbh.rustykrab` LaunchAgent of this user.
pub struct Launchd {
    uid: String,
    plist: PathBuf,
}

impl Launchd {
    pub fn for_current_user() -> anyhow::Result<Self> {
        let uid = run_checked(
            {
                let mut c = Command::new("id");
                c.arg("-u");
                c
            },
            "id -u",
        )?
        .trim()
        .to_string();
        let home = dirs::home_dir().ok_or_else(|| anyhow!("no home directory"))?;
        let plist = home
            .join("Library")
            .join("LaunchAgents")
            .join(format!("{BUNDLE_ID}.plist"));
        Ok(Self { uid, plist })
    }

    fn target(&self) -> String {
        format!("gui/{}/{BUNDLE_ID}", self.uid)
    }

    fn loaded(&self) -> bool {
        Command::new("launchctl")
            .arg("print")
            .arg(self.target())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }
}

impl ServiceManager for Launchd {
    fn is_launchd(&self) -> bool {
        true
    }

    fn describe(&self) -> String {
        format!("launchd {} ({})", self.target(), self.plist.display())
    }

    fn stop(&self) -> anyhow::Result<()> {
        let bootout = || {
            let mut cmd = Command::new("launchctl");
            cmd.arg("bootout").arg(self.target());
            run_checked(cmd, "launchctl bootout").map(drop)
        };
        stop_job(&self.target(), bootout, || self.loaded(), STOP_WITHIN)
    }

    fn start(&self) -> anyhow::Result<()> {
        let mut cmd = Command::new("launchctl");
        cmd.arg("bootstrap")
            .arg(format!("gui/{}", self.uid))
            .arg(&self.plist);
        run_checked(cmd, "launchctl bootstrap").map(drop)
    }

    fn running(&self) -> bool {
        self.loaded()
    }
}

/// One listening socket of a process, as `lsof -F pn` names it
/// (`127.0.0.1:3100`, `[::1]:3100`, `*:3100`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listener {
    pub pid: u32,
    pub address: String,
}

/// The host's processes, as the script service sees them. A trait so the
/// tests script it and never signal anything.
pub trait Processes: Send + Sync {
    /// Every TCP listener on `port`.
    fn listeners(&self, port: u16) -> anyhow::Result<Vec<Listener>>;
    /// The path of the executable `pid` runs.
    fn executable(&self, pid: u32) -> anyhow::Result<PathBuf>;
    fn terminate(&self, pid: u32) -> anyhow::Result<()>;
    fn alive(&self, pid: u32) -> bool;
}

/// How long `lsof` or `ps` may take.
const INSPECT_WITHIN: Duration = Duration::from_secs(10);

/// The real processes: `lsof`, `ps` (or `/proc`) and `kill`.
pub struct SystemProcesses;

impl Processes for SystemProcesses {
    fn listeners(&self, port: u16) -> anyhow::Result<Vec<Listener>> {
        let mut cmd = Command::new("lsof");
        cmd.args(["-nP", "-a", "-sTCP:LISTEN", "-Fpn"])
            .arg(format!("-iTCP:{port}"));
        // `lsof` exits 1 when it finds nothing, so only its output counts.
        let out = output_within(cmd, INSPECT_WITHIN).context("running lsof")?;
        Ok(parse_lsof(&String::from_utf8_lossy(&out.stdout)))
    }

    fn executable(&self, pid: u32) -> anyhow::Result<PathBuf> {
        executable_of(pid)
    }

    fn terminate(&self, pid: u32) -> anyhow::Result<()> {
        let mut cmd = Command::new("kill");
        cmd.arg("-TERM").arg(pid.to_string());
        run_checked(cmd, &format!("kill -TERM {pid}")).map(drop)
    }

    fn alive(&self, pid: u32) -> bool {
        Command::new("kill")
            .arg("-0")
            .arg(pid.to_string())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }
}

/// The executable `pid` runs, as the kernel knows it: `proc_pidpath` on
/// macOS. Not `ps -o comm=`, which prints the process's own `argv[0]`, a
/// string any process can set to the installed path.
#[cfg(target_os = "macos")]
fn executable_of(pid: u32) -> anyhow::Result<PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    let pid_c = libc::c_int::try_from(pid).context("process id out of range")?;
    let mut buf = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    // SAFETY: `buf` is writable for `buf.len()` bytes, the size passed, and
    // `proc_pidpath` writes at most that many.
    let len = unsafe { libc::proc_pidpath(pid_c, buf.as_mut_ptr().cast(), buf.len() as u32) };
    if len <= 0 {
        bail!(
            "proc_pidpath names no executable for process {pid}: {}",
            std::io::Error::last_os_error()
        );
    }
    buf.truncate(len as usize);
    Ok(PathBuf::from(std::ffi::OsStr::from_bytes(&buf)))
}

/// The executable `pid` runs: the `/proc/<pid>/exe` link on Linux.
#[cfg(not(target_os = "macos"))]
fn executable_of(pid: u32) -> anyhow::Result<PathBuf> {
    let proc_exe = PathBuf::from(format!("/proc/{pid}/exe"));
    std::fs::read_link(&proc_exe).with_context(|| format!("reading {}", proc_exe.display()))
}

/// The `p` and `n` fields of `lsof -F pn`.
pub fn parse_lsof(text: &str) -> Vec<Listener> {
    let mut pid = None;
    let mut found = Vec::new();
    for line in text.lines() {
        if let Some(p) = line.strip_prefix('p') {
            pid = p.trim().parse().ok();
        } else if let (Some(address), Some(pid)) = (line.strip_prefix('n'), pid) {
            found.push(Listener {
                pid,
                address: address.trim().to_string(),
            });
        }
    }
    found
}

/// Whether an `lsof` address such as `127.0.0.1:3100` or `[::1]:3100` is a
/// loopback one. `*:3100` (every interface) is not.
pub fn is_loopback_address(address: &str) -> bool {
    let Some((host, _port)) = address.rsplit_once(':') else {
        return false;
    };
    let host = host.trim_start_matches('[').trim_end_matches(']');
    host.parse::<std::net::IpAddr>()
        .is_ok_and(|ip| ip.is_loopback())
}

/// The binary an installed bundle or bare binary runs.
fn installed_executable(installed: &Path) -> PathBuf {
    if installed.is_dir() {
        installed.join("Contents").join("MacOS").join(BINARY_NAME)
    } else {
        installed.to_path_buf()
    }
}

/// The one process the script service may stop: the only listener on
/// `port`, listening on loopback alone, running the installed executable.
/// Anything else is refused, including no listener at all, since the
/// daemon answered on that port.
pub fn the_daemon(processes: &dyn Processes, port: u16, installed: &Path) -> anyhow::Result<u32> {
    let listeners = processes.listeners(port)?;
    let mut pids: Vec<u32> = listeners.iter().map(|l| l.pid).collect();
    pids.sort_unstable();
    pids.dedup();
    let pid = match pids.as_slice() {
        [] => bail!(
            "nothing is found listening on port {port}, yet the daemon answered there; \
             refusing to guess what to stop"
        ),
        [pid] => *pid,
        _ => bail!("processes {pids:?} all listen on port {port}; refusing to pick one"),
    };
    if let Some(open) = listeners.iter().find(|l| !is_loopback_address(&l.address)) {
        bail!(
            "process {pid} listens on {}, not on loopback alone; refusing to stop it",
            open.address
        );
    }
    let exe = processes.executable(pid)?;
    let want = installed_executable(installed);
    if !exe.is_absolute() {
        bail!(
            "process {pid} names its executable by the relative path {}; refusing to stop it",
            exe.display()
        );
    }
    let canonical = |path: &Path| {
        std::fs::canonicalize(path).with_context(|| {
            format!(
                "resolving {}; refusing to stop process {pid} without comparing real paths",
                path.display()
            )
        })
    };
    if canonical(&exe)? != canonical(&want)? {
        bail!(
            "process {pid} on port {port} runs {}, not the installed {}; refusing to stop it",
            exe.display(),
            want.display()
        );
    }
    Ok(pid)
}

/// No service manager: the daemon is the installed binary listening on its
/// port, and a shell command starts it.
pub struct Script {
    command: String,
    port: u16,
    installed: PathBuf,
    processes: Box<dyn Processes>,
}

impl Script {
    pub fn new(
        command: String,
        base: &Url,
        installed: PathBuf,
        processes: Box<dyn Processes>,
    ) -> anyhow::Result<Self> {
        let port = base
            .port_or_known_default()
            .ok_or_else(|| anyhow!("{base} has no port"))?;
        Ok(Self {
            command,
            port,
            installed,
            processes,
        })
    }
}

impl ServiceManager for Script {
    fn is_launchd(&self) -> bool {
        false
    }

    fn describe(&self) -> String {
        format!(
            "script: SIGTERM to the installed {} listening on loopback port {}, start with `{}`",
            installed_executable(&self.installed).display(),
            self.port,
            self.command
        )
    }

    fn stop(&self) -> anyhow::Result<()> {
        let pid = the_daemon(self.processes.as_ref(), self.port, &self.installed)?;
        self.processes.terminate(pid)?;
        wait_until(&format!("process {pid}"), STOP_WITHIN, || {
            !self.processes.alive(pid)
        })
    }

    fn start(&self) -> anyhow::Result<()> {
        use std::os::unix::process::CommandExt;
        // Its own process group, so it outlives the supervisor.
        Command::new("sh")
            .arg("-c")
            .arg(&self.command)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .with_context(|| format!("starting `{}`", self.command))
            .map(drop)
    }

    fn running(&self) -> bool {
        self.processes
            .listeners(self.port)
            .is_ok_and(|l| !l.is_empty())
    }
}

fn describe(plan: &Plan) -> String {
    format!(
        "running {} ({}); staged {} ({}, {}) from {}\n\
         service: {}\n\
         swap: {} -> .prev, {} into its place",
        plan.running.version,
        plan.running.commit.as_deref().unwrap_or("commit unknown"),
        plan.staged.version,
        plan.staged.commit.as_deref().unwrap_or("commit unknown"),
        plan.staged.kind,
        plan.staged.source,
        plan.service,
        plan.installed.display(),
        plan.staged.path.display(),
    )
}

/// `--url` carries the bearer token, so it must be https, or plain http to
/// the loopback address `127.0.0.1` or `[::1]`, any port. `localhost` is
/// refused: it is a name the resolver may send elsewhere.
pub fn check_url(raw: &str) -> anyhow::Result<Url> {
    check_url_named(raw, "--url")
}

fn check_url_named(raw: &str, what: &str) -> anyhow::Result<Url> {
    let url = Url::parse(raw).map_err(|e| anyhow!("invalid {what} `{raw}`: {e}"))?;
    if url.host().is_none() {
        bail!("{what} `{raw}` has no host");
    }
    match url.scheme() {
        "https" => Ok(url),
        "http" if matches!(url.host_str(), Some("127.0.0.1" | "[::1]")) => Ok(url),
        "http" => bail!(
            "{what} `{raw}` is plain http to a host that is not a loopback address, which \
             could send the token in the clear; use https, or http://127.0.0.1 or http://[::1]"
        ),
        other => bail!("{what} `{raw}` is {other}, not http(s)"),
    }
}

/// The daemon's URL: `--url`, or else `default` (the gateway URL from
/// `RUSTYKRAB_GATEWAY_URL`), and either way through the same check.
pub fn base_url(arg: Option<&str>, default: anyhow::Result<Url>) -> anyhow::Result<Url> {
    match arg {
        Some(raw) => check_url(raw),
        None => check_url_named(default?.as_str(), "the gateway URL (RUSTYKRAB_GATEWAY_URL)"),
    }
}

/// `apply` stops the daemon, so it must not be the daemon or run from the
/// binary it swaps: refuse inside the daemon's own launchd job
/// (`XPC_SERVICE_NAME` is [`BUNDLE_ID`]), and when `exe` resolves to a path
/// under `installed`.
pub fn refuse_inside_daemon(
    xpc_service: Option<&str>,
    exe: Option<&Path>,
    installed: &Path,
) -> anyhow::Result<()> {
    if xpc_service == Some(BUNDLE_ID) {
        bail!(
            "apply is running inside the daemon's own launchd job ({BUNDLE_ID}), and stopping \
             the daemon would stop it too; run it from its own job or a shell"
        );
    }
    let resolve = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    if let Some(exe) = exe {
        let (exe, installed) = (resolve(exe), resolve(installed));
        if exe.starts_with(&installed) {
            bail!(
                "apply is running from {}, under the install {} it would swap; run it from a \
                 copy kept outside it",
                exe.display(),
                installed.display()
            );
        }
    }
    Ok(())
}

/// The script service swaps with no signature check, so it is only for a
/// bare binary launchd does not run: `installed` must be a regular file
/// (not a symlink), with no `*.app` component and not under
/// `~/Applications` (`home` joined with `Applications`), as written and as
/// it resolves.
pub fn check_script_installed(installed: &Path, home: Option<&Path>) -> anyhow::Result<()> {
    let refuse = |why: String| {
        anyhow!(
            "--service script:<cmd> is only for a bare binary launchd does not run, and \
             --installed {} {why}; refusing it before anything runs",
            installed.display()
        )
    };
    let meta = std::fs::symlink_metadata(installed)
        .map_err(|e| refuse(format!("cannot be read ({e})")))?;
    if !meta.is_file() {
        return Err(refuse("is not a regular file".to_string()));
    }
    let resolved =
        std::fs::canonicalize(installed).map_err(|e| refuse(format!("does not resolve ({e})")))?;
    let applications = home.map(|h| {
        let dir = h.join("Applications");
        let real = std::fs::canonicalize(&dir).unwrap_or_else(|_| dir.clone());
        (dir, real)
    });
    for path in [installed, resolved.as_path()] {
        let in_bundle = path.components().any(|c| {
            Path::new(c.as_os_str())
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("app"))
        });
        if in_bundle {
            return Err(refuse(format!(
                "is inside an app bundle ({})",
                path.display()
            )));
        }
        if let Some((dir, real)) = &applications {
            if path.starts_with(dir) || path.starts_with(real) {
                return Err(refuse(format!("is under {}", dir.display())));
            }
        }
    }
    Ok(())
}

/// Entry point from `update_cmd::run`.
pub async fn run(cfg: &Config, data_dir: &Path, args: ApplyArgs) -> anyhow::Result<()> {
    let yes = args.yes || std::env::var("RUSTYKRAB_UPDATE_AUTO").is_ok_and(|v| v.trim() == "1");
    if let ServiceSpec::Script(_) = &args.service {
        let installed = args
            .installed
            .as_deref()
            .ok_or_else(|| anyhow!("--service script:... needs --installed PATH"))?;
        check_script_installed(installed, dirs::home_dir().as_deref())?;
    }
    let base = base_url(args.url.as_deref(), crate::daemon_client::gateway_url())?;
    let token = crate::daemon_client::resolve_auth_token(data_dir).await?;
    let probe = HttpProbe::new(&base, &token)?;
    let (service, installed): (Box<dyn ServiceManager>, PathBuf) = match args.service {
        ServiceSpec::Launchd => {
            let installed = match args.installed {
                Some(path) => path,
                None => dirs::home_dir()
                    .ok_or_else(|| anyhow!("no home directory"))?
                    .join("Applications")
                    .join(APP_NAME),
            };
            (Box::new(Launchd::for_current_user()?), installed)
        }
        ServiceSpec::Script(command) => {
            let installed = args
                .installed
                .ok_or_else(|| anyhow!("--service script:... needs --installed PATH"))?;
            let script = Script::new(command, &base, installed.clone(), Box::new(SystemProcesses))?;
            (Box::new(script), installed)
        }
    };
    refuse_inside_daemon(
        std::env::var("XPC_SERVICE_NAME").ok().as_deref(),
        std::env::current_exe().ok().as_deref(),
        &installed,
    )?;
    let swap = DirSwap { installed };
    let host = Host {
        service: service.as_ref(),
        swap: &swap,
        probe: &probe,
        verifier: &super::SystemVerifier,
        verify_within: VERIFY_WITHIN,
        poll: Duration::from_secs(1),
    };
    match apply(cfg, &host, yes).await? {
        Outcome::Planned(plan) => println!(
            "would apply (nothing changed; pass --yes or set RUSTYKRAB_UPDATE_AUTO=1):\n{}",
            describe(&plan)
        ),
        Outcome::AlreadyRunning(plan) => println!(
            "nothing to do: staged {} ({}) is already running",
            plan.staged.version,
            plan.staged.commit.as_deref().unwrap_or("")
        ),
        Outcome::Applied(plan) => println!(
            "applied and verified:\n{}\nthe previous version is kept as .prev",
            describe(&plan)
        ),
        Outcome::RolledBack {
            plan,
            reason,
            bad_record_error,
        } => bail!(
            "rolled back: staged {} failed verification ({reason}); the previous \
             version is running again and {}",
            plan.staged.version,
            match bad_record_error {
                None => "the staged one is recorded as bad".to_string(),
                Some(e) => format!("recording the staged one as bad FAILED ({e})"),
            }
        ),
        Outcome::Recovered(r) => {
            let mut done = Vec::new();
            if r.restored_prev {
                done.push("moved .prev back to the missing install path".to_string());
            }
            if r.rolled_back {
                done.push("rolled the new version back".to_string());
            }
            if r.started {
                done.push("started the service and verified it".to_string());
            }
            if r.finished {
                done.push(
                    "found its new version running and verified, and cleared its journal"
                        .to_string(),
                );
            }
            if done.is_empty() {
                done.push("dropped the copy it had not swapped in".to_string());
            }
            let summary = format!(
                "recovered an interrupted apply (journal phase {}): {}; nothing new was applied",
                r.phase
                    .map_or("none".to_string(), |p| format!("{p:?}").to_lowercase()),
                done.join(", ")
            );
            match (r.rolled_back, r.bad_record_error) {
                (_, Some(e)) => bail!("{summary}; recording the new version as bad FAILED ({e})"),
                (true, None) => bail!("{summary}; the new version is recorded as bad"),
                (false, None) => println!("{summary}"),
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
