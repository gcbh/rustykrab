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
//! The service manager, the swap root and the version probe are traits, so
//! the tests script them and never reach `launchctl` or a real daemon.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use reqwest::Url;
use serde::Deserialize;

use super::{is_bad, record_bad, BadVersion, Config, Staged, APP_NAME, BUNDLE_ID, STAGED_FILE};

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
    /// Move the installed version to `.prev`, replacing an older one, and
    /// `staged` into its place. On an error the installed version is left
    /// where it was.
    fn swap_in(&self, staged: &Path) -> anyhow::Result<()>;
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
pub fn newest_staged(cfg: &Config) -> anyhow::Result<Option<Staged>> {
    let updates = cfg.updates_dir();
    let entries = match std::fs::read_dir(&updates) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("reading {}", updates.display())),
    };
    let mut newest: Option<Staged> = None;
    for entry in entries.flatten() {
        if entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        let record = entry.path().join(STAGED_FILE);
        let Ok(text) = std::fs::read_to_string(&record) else {
            continue;
        };
        let staged: Staged =
            serde_json::from_str(&text).with_context(|| format!("parsing {}", record.display()))?;
        if newest
            .as_ref()
            .is_none_or(|n| staged.staged_at > n.staged_at)
        {
            newest = Some(staged);
        }
    }
    Ok(newest)
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
    if !staged.path.exists() {
        bail!("the staged {} is missing", staged.path.display());
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
    if !yes {
        return Ok(Outcome::Planned(plan));
    }

    host.service.stop().context("stopping the daemon")?;
    if let Err(e) = host.swap.swap_in(&plan.staged.path) {
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
/// consecutive failed ticks.
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
                if let Some(tick) = r.controller.last_tick {
                    match last_tick {
                        Some(prev) if tick > prev => advances += 1,
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

    fn swap_in(&self, staged: &Path) -> anyhow::Result<()> {
        // The stage may be on another volume; copy it next to the installed
        // version first, so both moves are renames in one directory.
        let next = sibling(&self.installed, ".", ".next")?;
        let prev = sibling(&self.installed, "", ".prev")?;
        remove_any(&next)?;
        let copy = Command::new("cp")
            .arg("-Rp")
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

/// No service manager: the daemon is whatever listens on its port, and a
/// shell command starts it.
pub struct Script {
    command: String,
    port: u16,
}

impl Script {
    pub fn new(command: String, base: &Url) -> anyhow::Result<Self> {
        let port = base
            .port_or_known_default()
            .ok_or_else(|| anyhow!("{base} has no port"))?;
        Ok(Self { command, port })
    }

    fn listeners(&self) -> Vec<String> {
        Command::new("lsof")
            .args(["-nP", "-t", "-sTCP:LISTEN"])
            .arg(format!("-iTCP:{}", self.port))
            .stderr(Stdio::null())
            .output()
            .map(|o| {
                String::from_utf8_lossy(&o.stdout)
                    .split_whitespace()
                    .filter(|p| p.bytes().all(|b| b.is_ascii_digit()))
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    }
}

fn alive(pid: &str) -> bool {
    Command::new("kill")
        .args(["-0", pid])
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

impl ServiceManager for Script {
    fn is_launchd(&self) -> bool {
        false
    }

    fn describe(&self) -> String {
        format!(
            "script: SIGTERM to the process on port {}, start with `{}`",
            self.port, self.command
        )
    }

    fn stop(&self) -> anyhow::Result<()> {
        let pids = self.listeners();
        for pid in &pids {
            let mut cmd = Command::new("kill");
            cmd.args(["-TERM", pid]);
            run_checked(cmd, &format!("kill -TERM {pid}"))?;
        }
        for pid in &pids {
            wait_until(&format!("process {pid}"), || !alive(pid))?;
        }
        Ok(())
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

/// Entry point from `update_cmd::run`.
pub async fn run(cfg: &Config, data_dir: &Path, args: ApplyArgs) -> anyhow::Result<()> {
    let yes = args.yes || std::env::var("RUSTYKRAB_UPDATE_AUTO").is_ok_and(|v| v.trim() == "1");
    let base = match &args.url {
        Some(raw) => {
            let url = Url::parse(raw).map_err(|e| anyhow!("invalid --url `{raw}`: {e}"))?;
            if !matches!(url.scheme(), "http" | "https") || url.host().is_none() {
                bail!("--url must be an http(s) URL with a host");
            }
            url
        }
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
            (Box::new(Script::new(command, &base)?), installed)
        }
    };
    let swap = DirSwap { installed };
    let host = Host {
        service: service.as_ref(),
        swap: &swap,
        probe: &probe,
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
