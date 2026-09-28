//! `rustykrab update apply`: the supervisor that swaps the newest staged
//! version in, verifies it and rolls back (slice 6 of
//! `docs/plans/update-flow.md`).
//!
//! The steps: read the newest `staged.json` and the running version from
//! `/api/version`; stop the daemon through its service manager and wait for
//! it to exit; rename the installed bundle or binary to `.prev` and move the
//! stage into place, both in one directory; start it; verify within 90 s
//! that it reports the staged commit, holds `controller.lock`, ticks twice
//! and has no failed ticks. On any failure the new version is stopped,
//! `.prev` restored, started and verified the same way, and the new version
//! recorded in `bad.json`. Without `--yes` (or `RUSTYKRAB_UPDATE_AUTO=1`)
//! nothing changes: the plan is printed.
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
use serde::Deserialize;

use super::{
    is_bad, is_newer, is_plain_version, output_within, parse_version_output, record_bad,
    BadVersion, Config, Staged, Verifier, APP_NAME, BINARY_NAME, BUNDLE_ID, STAGED_FILE,
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
}

/// Where the installed version lives, and the two renames beside it.
pub trait SwapRoot: Send + Sync {
    fn installed(&self) -> &Path;
    /// Copy `staged` beside the installed version, pass the copy through
    /// `check`, then move the installed version to `.prev`, replacing an
    /// older one, and the copy into its place. On an error the installed
    /// version is left where it was.
    fn swap_in(&self, staged: &Path, check: &NextCheck<'_>) -> anyhow::Result<()>;
    /// Put `.prev` back in place of whatever is installed.
    fn restore(&self) -> anyhow::Result<()>;
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
    /// recorded as bad.
    RolledBack { plan: Plan, reason: String },
}

/// The `staged.json` with the latest `staged_at` under `<data>/updates/`.
/// A record that does not parse is skipped; the newest one is refused
/// unless it is canonical ([`check_canonical`]).
pub fn newest_staged(cfg: &Config) -> anyhow::Result<Option<Staged>> {
    let updates = cfg.updates_dir();
    let entries = match std::fs::read_dir(&updates) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("reading {}", updates.display())),
    };
    let mut newest: Option<(String, Staged)> = None;
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
        if newest
            .as_ref()
            .is_none_or(|(_, n)| staged.staged_at > n.staged_at)
        {
            newest = Some((dir, staged));
        }
    }
    let Some((dir, staged)) = newest else {
        return Ok(None);
    };
    check_canonical(cfg, &dir, &staged)?;
    Ok(Some(staged))
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
/// the checks [`SwapRoot::swap_in`] runs on it.
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
        let meta = std::fs::symlink_metadata(next)
            .with_context(|| format!("reading {}", next.display()))?;
        let binary = if app {
            if !meta.is_dir() {
                bail!("{} is not a directory; refusing it", next.display());
            }
            let macos = next.join("Contents").join("MacOS");
            let binary = macos.join(BINARY_NAME);
            for p in [&next.join("Contents"), &macos, &binary] {
                if is_symlink(p) {
                    bail!("{} is a symbolic link; refusing it", p.display());
                }
            }
            binary
        } else {
            if !meta.is_file() {
                bail!("{} is not a regular file; refusing it", next.display());
            }
            next.to_path_buf()
        };
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
pub async fn apply(cfg: &Config, host: &Host<'_>, yes: bool) -> anyhow::Result<Outcome> {
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
    if running_commit == commit {
        return Ok(Outcome::AlreadyRunning(plan));
    }
    if plan.staged.tag.is_some() && !is_newer(&plan.staged.version, &plan.running.version)? {
        bail!(
            "staged release {} is not newer than the running {}; refusing it",
            plan.staged.version,
            plan.running.version
        );
    }
    let controller = &plan.running.controller;
    if controller.lock.as_deref() != Some("held") || controller.consecutive_failed_ticks != Some(0)
    {
        bail!(
            "the running daemon is not healthy (controller.lock {}, consecutive_failed_ticks {}); \
             an update is only applied over a daemon that holds the lock and is not failing \
             ticks, so a rollback has a healthy version to return to",
            controller.lock.as_deref().unwrap_or("(unknown)"),
            controller
                .consecutive_failed_ticks
                .map_or("(unknown)".to_string(), |n| n.to_string())
        );
    }
    if !yes {
        return Ok(Outcome::Planned(plan));
    }

    let check = NextCheck {
        verifier: host.verifier,
        team_id: host.service.is_launchd().then_some(cfg.team_id.as_str()),
        version: &plan.staged.version,
        commit: &commit,
    };
    host.service.stop().context("stopping the daemon")?;
    if let Err(e) = host.swap.swap_in(&plan.staged.path, &check) {
        // The installed version is where it was: start it again.
        let restarted = host.service.start();
        return Err(e.context(match restarted {
            Ok(()) => "swapping the staged version in; the old one was started again",
            Err(_) => "swapping the staged version in; starting the old one also failed",
        }));
    }
    let failure = match host.service.start() {
        Err(e) => Some(format!("starting the new version: {e:#}")),
        Ok(()) => verify(host, &commit).await.err().map(|e| format!("{e:#}")),
    };
    let Some(reason) = failure else {
        return Ok(Outcome::Applied(plan));
    };

    // Roll back. The new version is recorded first, so it is never tried
    // again even if what follows fails too.
    record_bad(cfg, bad_entry(&plan.staged))?;
    if let Err(e) = host.service.stop() {
        tracing::warn!("stopping the new version for the rollback: {e:#}");
    }
    host.swap
        .restore()
        .with_context(|| format!("restoring the previous version after: {reason}"))?;
    host.service
        .start()
        .with_context(|| format!("starting the previous version after: {reason}"))?;
    verify(host, &running_commit)
        .await
        .with_context(|| format!("verifying the previous version after: {reason}"))?;
    Ok(Outcome::RolledBack { plan, reason })
}

/// Within `host.verify_within`, `/api/version` must report `commit`,
/// `controller.lock` `held`, a `last_tick` that advances twice and no
/// consecutive failed ticks. An advance counts only while no ticks are
/// failing, and a failing tick starts the count again: a tick that ran and
/// failed is not progress.
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

    fn swap_in(&self, staged: &Path, check: &NextCheck<'_>) -> anyhow::Result<()> {
        // The stage may be on another volume; copy it next to the installed
        // version first, so both moves are renames in one directory.
        let next = sibling(&self.installed, ".", ".next")?;
        let prev = sibling(&self.installed, "", ".prev")?;
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

/// Poll `gone` until it holds or `STOP_WITHIN` passes.
fn wait_until(what: &str, gone: impl Fn() -> bool) -> anyhow::Result<()> {
    let deadline = Instant::now() + STOP_WITHIN;
    while !gone() {
        if Instant::now() >= deadline {
            bail!("{what} did not exit within {}s", STOP_WITHIN.as_secs());
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    Ok(())
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
        let mut cmd = Command::new("launchctl");
        cmd.arg("bootout").arg(self.target());
        let bootout = run_checked(cmd, "launchctl bootout");
        if let Err(e) = bootout {
            if self.loaded() {
                return Err(e);
            }
        }
        wait_until(&self.target(), || !self.loaded())
    }

    fn start(&self) -> anyhow::Result<()> {
        let mut cmd = Command::new("launchctl");
        cmd.arg("bootstrap")
            .arg(format!("gui/{}", self.uid))
            .arg(&self.plist);
        run_checked(cmd, "launchctl bootstrap").map(drop)
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
        let proc_exe = PathBuf::from(format!("/proc/{pid}/exe"));
        if cfg!(target_os = "linux") {
            return std::fs::read_link(&proc_exe)
                .with_context(|| format!("reading {}", proc_exe.display()));
        }
        let mut cmd = Command::new("ps");
        cmd.args(["-o", "comm=", "-p"]).arg(pid.to_string());
        let out = output_within(cmd, INSPECT_WITHIN).context("running ps")?;
        let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !out.status.success() || path.is_empty() {
            bail!("ps names no executable for process {pid}");
        }
        Ok(PathBuf::from(path))
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
    let same = match (std::fs::canonicalize(&exe), std::fs::canonicalize(&want)) {
        (Ok(a), Ok(b)) => a == b,
        _ => exe == want,
    };
    if !same {
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
        wait_until(&format!("process {pid}"), || !self.processes.alive(pid))
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
/// loopback (`127.0.0.1`, `::1` or `localhost`).
pub fn check_url(raw: &str) -> anyhow::Result<Url> {
    let url = Url::parse(raw).map_err(|e| anyhow!("invalid --url `{raw}`: {e}"))?;
    if url.host().is_none() {
        bail!("--url `{raw}` has no host");
    }
    match url.scheme() {
        "https" => Ok(url),
        "http" if matches!(url.host_str(), Some("127.0.0.1" | "[::1]" | "localhost")) => Ok(url),
        "http" => bail!(
            "--url `{raw}` is plain http to a host that is not loopback, which would send \
             the token in the clear; use https, or 127.0.0.1, ::1 or localhost"
        ),
        other => bail!("--url `{raw}` is {other}, not http(s)"),
    }
}

/// Entry point from `update_cmd::run`.
pub async fn run(cfg: &Config, data_dir: &Path, args: ApplyArgs) -> anyhow::Result<()> {
    let yes = args.yes || std::env::var("RUSTYKRAB_UPDATE_AUTO").is_ok_and(|v| v.trim() == "1");
    let base = match &args.url {
        Some(raw) => check_url(raw)?,
        None => crate::daemon_client::gateway_url()?,
    };
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
        Outcome::RolledBack { plan, reason } => bail!(
            "rolled back: staged {} failed verification ({reason}); the previous \
             version is running again and the staged one is recorded as bad",
            plan.staged.version
        ),
    }
    Ok(())
}

#[cfg(test)]
mod tests;
