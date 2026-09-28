//! `rustykrab update check` and `rustykrab update stage`: where a new
//! version comes from and how it is checked (slice 5 of
//! `docs/plans/update-flow.md`).
//!
//! Neither command touches the running daemon. `check` asks GitHub for the
//! latest release of `RUSTYKRAB_UPDATE_REPO` and says whether it is newer.
//! `stage` downloads the release's archive for this target, or copies a
//! local build given with `--from`, and puts it under
//! `<data dir>/updates/<version>/` only after every check has passed, in
//! this order: the digest before anything is extracted, the signature
//! before anything is run, and the staged binary's own `--version` last.
//! Swapping it in is slice 6.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{anyhow, bail, Context};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

const USAGE: &str = "\
usage: rustykrab update <command>

  check                          whether the latest release is newer than
                                 this binary
  stage [--from PATH] [--force]  download the latest release for this target,
                                 verify its digest, signature and version, and
                                 stage it under <data dir>/updates/<version>/;
                                 --from stages a local RustyKrab.app or binary
                                 instead, --force stages a version recorded
                                 as bad

Reads RUSTYKRAB_UPDATE_REPO (default gcbh/rustykrab), RUSTYKRAB_GITHUB_TOKEN
(optional, for rate limits), RUSTYKRAB_UPDATE_API_BASE (default
https://api.github.com) and RUSTYKRAB_UPDATE_TEAM_ID (default 3RRX845C4X).";

pub const DEFAULT_REPO: &str = "gcbh/rustykrab";
pub const DEFAULT_API_BASE: &str = "https://api.github.com";
/// The Developer ID team every release is signed by. A correctly signed
/// bundle from any other team is refused; a fork overrides the pin with
/// `RUSTYKRAB_UPDATE_TEAM_ID`.
pub const DEFAULT_TEAM_ID: &str = "3RRX845C4X";
pub const BUNDLE_ID: &str = "com.gcbh.rustykrab";
pub const APP_NAME: &str = "RustyKrab.app";
pub const BINARY_NAME: &str = "rustykrab-cli";
/// Versions a rollback (slice 6) recorded as bad, under `<data>/updates/`.
pub const BAD_FILE: &str = "bad.json";
pub const STAGED_FILE: &str = "staged.json";

/// Everything the commands read from the environment, so tests can point
/// them at a local stand-in.
#[derive(Debug, Clone)]
pub struct Config {
    pub data_dir: PathBuf,
    pub repo: String,
    pub api_base: String,
    pub token: Option<String>,
    pub target: String,
    pub running_version: String,
    pub team_id: String,
}

impl Config {
    pub fn from_env(data_dir: &Path) -> Self {
        let var = |name: &str| {
            std::env::var(name)
                .ok()
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        };
        Self {
            data_dir: data_dir.to_path_buf(),
            repo: var("RUSTYKRAB_UPDATE_REPO").unwrap_or_else(|| DEFAULT_REPO.to_string()),
            api_base: var("RUSTYKRAB_UPDATE_API_BASE")
                .unwrap_or_else(|| DEFAULT_API_BASE.to_string()),
            token: var("RUSTYKRAB_GITHUB_TOKEN"),
            target: env!("RUSTYKRAB_TARGET").to_string(),
            running_version: crate::VERSION.to_string(),
            team_id: var("RUSTYKRAB_UPDATE_TEAM_ID").unwrap_or_else(|| DEFAULT_TEAM_ID.to_string()),
        }
    }

    fn updates_dir(&self) -> PathBuf {
        self.data_dir.join("updates")
    }

    fn asset_name(&self) -> String {
        format!("rustykrab-{}.tar.gz", self.target)
    }
}

/// The two checks that need the host: the signature of a staged bundle and
/// one run of the staged binary. Behind a trait so tests can script them.
pub trait Verifier {
    /// Refuse `app` unless it is validly signed as [`BUNDLE_ID`] by `team_id`.
    fn verify_signature(&self, app: &Path, team_id: &str) -> anyhow::Result<()>;
    /// Run `binary --version` once and return what it printed.
    fn run_version(&self, binary: &Path) -> anyhow::Result<String>;
}

/// The real checks: `codesign` on macOS, and the staged binary itself.
pub struct SystemVerifier;

impl Verifier for SystemVerifier {
    fn verify_signature(&self, app: &Path, team_id: &str) -> anyhow::Result<()> {
        if !cfg!(target_os = "macos") {
            // No signature to check off macOS yet; the digest is the check.
            return Ok(());
        }
        let verify = Command::new("codesign")
            .args(["--verify", "--deep", "--strict"])
            .arg(app)
            .output()
            .context("running codesign --verify")?;
        if !verify.status.success() {
            bail!(
                "{} fails codesign --verify --deep --strict: {}",
                app.display(),
                String::from_utf8_lossy(&verify.stderr).trim()
            );
        }
        let details = Command::new("codesign")
            .arg("-dv")
            .arg(app)
            .output()
            .context("running codesign -dv")?;
        if !details.status.success() {
            bail!(
                "codesign -dv {} failed: {}",
                app.display(),
                String::from_utf8_lossy(&details.stderr).trim()
            );
        }
        // `codesign -dv` writes its report to stderr.
        let mut text = String::from_utf8_lossy(&details.stderr).into_owned();
        text.push_str(&String::from_utf8_lossy(&details.stdout));
        check_signing_details(&text, team_id)
    }

    fn run_version(&self, binary: &Path) -> anyhow::Result<String> {
        let out = Command::new(binary)
            .arg("--version")
            .output()
            .with_context(|| format!("running {} --version", binary.display()))?;
        if !out.status.success() {
            bail!("{} --version exited with {}", binary.display(), out.status);
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }
}

/// Require `Identifier=` [`BUNDLE_ID`] and `TeamIdentifier=<team_id>` in a
/// `codesign -dv` report.
pub fn check_signing_details(text: &str, team_id: &str) -> anyhow::Result<()> {
    let field = |key: &str| {
        text.lines()
            .find_map(|line| line.trim().strip_prefix(key))
            .map(str::trim)
    };
    match field("Identifier=") {
        Some(id) if id == BUNDLE_ID => {}
        other => bail!(
            "signed as identifier {}, expected {BUNDLE_ID}",
            other.unwrap_or("(none)")
        ),
    }
    match field("TeamIdentifier=") {
        Some(team) if team == team_id => Ok(()),
        other => bail!(
            "signed by team {}, expected {team_id}",
            other.unwrap_or("(none)")
        ),
    }
}

/// Parse `rustykrab X.Y.Z (commit, date)` into the version and the commit.
pub fn parse_version_output(text: &str) -> Option<(String, Option<String>)> {
    let line = text
        .lines()
        .map(str::trim)
        .find(|l| l.starts_with("rustykrab "))?;
    let rest = line.strip_prefix("rustykrab ")?;
    let version = rest.split_whitespace().next()?.to_string();
    let commit = rest
        .split_once('(')
        .and_then(|(_, tail)| tail.split([',', ')']).next())
        .map(str::trim)
        .filter(|c| !c.is_empty() && *c != "unknown")
        .map(str::to_string);
    Some((version, commit))
}

/// `vX.Y.Z` or `X.Y.Z` as numbers, for comparison.
pub fn parse_semver(text: &str) -> Option<Vec<u64>> {
    let text = text.trim();
    let text = text.strip_prefix('v').unwrap_or(text);
    let parts: Option<Vec<u64>> = text.split('.').map(|p| p.parse().ok()).collect();
    parts.filter(|p| p.len() == 3)
}

/// Whether `candidate` is above `running`, compared as numbers.
pub fn is_newer(candidate: &str, running: &str) -> anyhow::Result<bool> {
    let c = parse_semver(candidate).ok_or_else(|| anyhow!("unreadable version {candidate:?}"))?;
    let r = parse_semver(running).ok_or_else(|| anyhow!("unreadable version {running:?}"))?;
    Ok(c > r)
}

/// SHA-256 of `bytes` as lowercase hex. The ring provider rustls already
/// links for TLS supplies the hash, so this needs no dependency of its own.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let rustls::SupportedCipherSuite::Tls13(suite) =
        rustls::crypto::ring::cipher_suite::TLS13_AES_128_GCM_SHA256
    else {
        unreachable!("TLS13_AES_128_GCM_SHA256 is a TLS 1.3 suite")
    };
    let hash = suite.common.hash_provider;
    debug_assert_eq!(
        hash.algorithm(),
        rustls::crypto::hash::HashAlgorithm::SHA256
    );
    hash.hash(bytes)
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[derive(Debug, Deserialize)]
struct Release {
    tag_name: String,
    #[serde(default)]
    assets: Vec<Asset>,
}

#[derive(Debug, Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
    #[serde(default)]
    digest: Option<String>,
}

/// What `check` found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Latest {
    pub tag: String,
    pub version: String,
    pub newer: bool,
}

/// `<data>/updates/<version>/staged.json`: what was staged, from where, and
/// what it was checked against. Slice 6 reads it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Staged {
    pub version: String,
    pub tag: Option<String>,
    pub commit: Option<String>,
    pub source: String,
    pub digest: Option<String>,
    pub path: PathBuf,
    pub staged_at: DateTime<Utc>,
}

/// One entry of `<data>/updates/bad.json`, written by a rollback.
#[derive(Debug, Clone, Deserialize)]
pub struct BadVersion {
    pub version: String,
}

#[derive(Debug)]
pub enum StageOutcome {
    Staged(Staged),
    NotNewer(Latest),
}

fn http_client() -> anyhow::Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(concat!("rustykrab-updater/", env!("CARGO_PKG_VERSION")))
        .timeout(std::time::Duration::from_secs(600))
        .build()
        .context("building the HTTP client")
}

fn with_auth(req: reqwest::RequestBuilder, cfg: &Config) -> reqwest::RequestBuilder {
    match &cfg.token {
        Some(token) => req.bearer_auth(token),
        None => req,
    }
}

async fn fetch_latest(client: &reqwest::Client, cfg: &Config) -> anyhow::Result<Release> {
    let url = format!(
        "{}/repos/{}/releases/latest",
        cfg.api_base.trim_end_matches('/'),
        cfg.repo
    );
    let resp = with_auth(client.get(&url), cfg)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    if !resp.status().is_success() {
        bail!("GET {url}: {}", resp.status());
    }
    resp.json().await.context("reading the release")
}

fn latest_of(release: &Release, cfg: &Config) -> anyhow::Result<Latest> {
    let version = release
        .tag_name
        .strip_prefix('v')
        .unwrap_or(&release.tag_name)
        .to_string();
    let newer = is_newer(&version, &cfg.running_version)?;
    Ok(Latest {
        tag: release.tag_name.clone(),
        version,
        newer,
    })
}

/// `rustykrab update check`.
pub async fn check(cfg: &Config) -> anyhow::Result<Latest> {
    let client = http_client()?;
    let release = fetch_latest(&client, cfg).await?;
    latest_of(&release, cfg)
}

/// Whether `version` is recorded in `<data>/updates/bad.json`.
pub fn is_bad(cfg: &Config, version: &str) -> anyhow::Result<bool> {
    let path = cfg.updates_dir().join(BAD_FILE);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let bad: Vec<BadVersion> =
        serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    Ok(bad.iter().any(|b| b.version == version))
}

fn refuse_bad(cfg: &Config, version: &str, force: bool) -> anyhow::Result<()> {
    if !force && is_bad(cfg, version)? {
        bail!("{version} is recorded as bad by a rollback; pass --force to stage it anyway");
    }
    Ok(())
}

/// A directory under `updates/` that is removed on drop unless kept, so a
/// refused stage leaves nothing behind.
struct Scratch {
    path: PathBuf,
    keep: bool,
}

impl Scratch {
    fn new(updates: &Path) -> anyhow::Result<Self> {
        std::fs::create_dir_all(updates)
            .with_context(|| format!("creating {}", updates.display()))?;
        let path = updates.join(format!(".staging-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&path).with_context(|| format!("creating {}", path.display()))?;
        Ok(Self { path, keep: false })
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        if !self.keep {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}

/// The staged thing inside a directory: a bundle and its binary, or a bare
/// binary.
fn payload(dir: &Path) -> anyhow::Result<(Option<PathBuf>, PathBuf)> {
    let app = dir.join(APP_NAME);
    if app.is_dir() {
        let binary = app.join("Contents").join("MacOS").join(BINARY_NAME);
        if !binary.is_file() {
            bail!("{APP_NAME} has no Contents/MacOS/{BINARY_NAME}");
        }
        return Ok((Some(app), binary));
    }
    let binary = dir.join(BINARY_NAME);
    if binary.is_file() {
        return Ok((None, binary));
    }
    bail!("neither {APP_NAME} nor {BINARY_NAME} found")
}

/// Signature, then `--version`. Returns the version and commit it printed.
fn verify_payload(
    cfg: &Config,
    verifier: &dyn Verifier,
    app: Option<&Path>,
    binary: &Path,
) -> anyhow::Result<(String, Option<String>)> {
    if let Some(app) = app {
        verifier
            .verify_signature(app, &cfg.team_id)
            .context("signature check refused the bundle")?;
    }
    let printed = verifier.run_version(binary)?;
    parse_version_output(&printed).ok_or_else(|| {
        anyhow!(
            "{} --version printed no version: {:?}",
            binary.display(),
            printed.trim()
        )
    })
}

/// Move a checked scratch directory to `updates/<version>/` and write its
/// record.
fn commit_stage(
    cfg: &Config,
    mut scratch: Scratch,
    mut staged: Staged,
    relative: &Path,
) -> anyhow::Result<Staged> {
    let dest = cfg.updates_dir().join(&staged.version);
    if dest.exists() {
        std::fs::remove_dir_all(&dest).with_context(|| format!("replacing {}", dest.display()))?;
    }
    std::fs::rename(&scratch.path, &dest)
        .with_context(|| format!("moving the staged version to {}", dest.display()))?;
    scratch.keep = true;
    staged.path = dest.join(relative);
    let record = serde_json::to_string_pretty(&staged)?;
    std::fs::write(dest.join(STAGED_FILE), record + "\n")
        .with_context(|| format!("writing {}", dest.join(STAGED_FILE).display()))?;
    Ok(staged)
}

/// `rustykrab update stage`: the latest release, if it is newer.
pub async fn stage_release(
    cfg: &Config,
    verifier: &dyn Verifier,
    force: bool,
) -> anyhow::Result<StageOutcome> {
    let client = http_client()?;
    let release = fetch_latest(&client, cfg).await?;
    let latest = latest_of(&release, cfg)?;
    if !latest.newer {
        return Ok(StageOutcome::NotNewer(latest));
    }
    refuse_bad(cfg, &latest.version, force)?;

    let name = cfg.asset_name();
    let asset = release
        .assets
        .iter()
        .find(|a| a.name == name)
        .ok_or_else(|| anyhow!("release {} has no asset {name}", latest.tag))?;
    let expected = asset
        .digest
        .as_deref()
        .and_then(|d| d.trim().strip_prefix("sha256:"))
        .map(str::to_ascii_lowercase)
        .filter(|hex| hex.len() == 64)
        .ok_or_else(|| {
            anyhow!(
                "release {} gives no sha256 digest for {name}; refusing it",
                latest.tag
            )
        })?;

    let resp = with_auth(client.get(&asset.browser_download_url), cfg)
        .header("Accept", "application/octet-stream")
        .send()
        .await
        .with_context(|| format!("downloading {name}"))?;
    if !resp.status().is_success() {
        bail!("downloading {name}: {}", resp.status());
    }
    let bytes = resp
        .bytes()
        .await
        .with_context(|| format!("downloading {name}"))?;
    let actual = sha256_hex(&bytes);
    if actual != expected {
        bail!("{name} has sha256 {actual}, the release says {expected}; refusing it");
    }

    // Only now does anything from the download reach the disk.
    let scratch = Scratch::new(&cfg.updates_dir())?;
    let archive = scratch.path.join(&name);
    std::fs::write(&archive, &bytes).with_context(|| format!("writing {}", archive.display()))?;
    let extract = scratch.path.join("x");
    std::fs::create_dir(&extract)?;
    let tar = Command::new("tar")
        .arg("-xzf")
        .arg(&archive)
        .arg("-C")
        .arg(&extract)
        .output()
        .context("running tar")?;
    if !tar.status.success() {
        bail!(
            "tar could not extract {name}: {}",
            String::from_utf8_lossy(&tar.stderr).trim()
        );
    }
    std::fs::remove_file(&archive)?;
    let (app, binary) = payload(&extract)?;
    if cfg!(target_os = "macos") && app.is_none() {
        bail!("{name} holds no {APP_NAME}; a macOS release must be a signed bundle");
    }
    let (printed, commit) = verify_payload(cfg, verifier, app.as_deref(), &binary)?;
    if printed != latest.version {
        bail!(
            "the staged binary reports {printed}, the release is {}",
            latest.version
        );
    }

    let relative = binary_relative(app.is_some());
    let inner = Scratch {
        path: extract,
        keep: false,
    };
    let staged = Staged {
        version: latest.version.clone(),
        tag: Some(latest.tag.clone()),
        commit,
        source: asset.browser_download_url.clone(),
        digest: Some(format!("sha256:{expected}")),
        path: PathBuf::new(),
        staged_at: Utc::now(),
    };
    let staged = commit_stage(cfg, inner, staged, &relative)?;
    drop(scratch);
    Ok(StageOutcome::Staged(staged))
}

fn binary_relative(is_app: bool) -> PathBuf {
    if is_app {
        PathBuf::from(APP_NAME)
    } else {
        PathBuf::from(BINARY_NAME)
    }
}

/// `rustykrab update stage --from <path>`: a local `RustyKrab.app` or bare
/// binary. There is no digest; the signature check still applies to a
/// bundle.
pub fn stage_from(
    cfg: &Config,
    verifier: &dyn Verifier,
    from: &Path,
    force: bool,
) -> anyhow::Result<Staged> {
    let from =
        std::fs::canonicalize(from).with_context(|| format!("reading {}", from.display()))?;
    let is_app = from.is_dir();
    if is_app
        && !from
            .join("Contents")
            .join("MacOS")
            .join(BINARY_NAME)
            .is_file()
    {
        bail!(
            "{} is a directory but not a bundle with Contents/MacOS/{BINARY_NAME}",
            from.display()
        );
    }
    let scratch = Scratch::new(&cfg.updates_dir())?;
    let relative = binary_relative(is_app);
    let copy = Command::new("cp")
        .arg("-Rp")
        .arg(&from)
        .arg(scratch.path.join(&relative))
        .output()
        .context("running cp")?;
    if !copy.status.success() {
        bail!(
            "copying {}: {}",
            from.display(),
            String::from_utf8_lossy(&copy.stderr).trim()
        );
    }
    let (app, binary) = payload(&scratch.path)?;
    let (version, commit) = verify_payload(cfg, verifier, app.as_deref(), &binary)?;
    refuse_bad(cfg, &version, force)?;
    let staged = Staged {
        version,
        tag: None,
        commit,
        source: from.display().to_string(),
        digest: None,
        path: PathBuf::new(),
        staged_at: Utc::now(),
    };
    commit_stage(cfg, scratch, staged, &relative)
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Cmd {
    Help,
    Check,
    Stage { from: Option<PathBuf>, force: bool },
}

fn parse(args: &[String]) -> Result<Cmd, String> {
    match args.first().map(String::as_str) {
        None | Some("help" | "--help" | "-h") => Ok(Cmd::Help),
        Some("check") if args.len() == 1 => Ok(Cmd::Check),
        Some("check") => Err("check takes no arguments".to_string()),
        Some("stage") => {
            let mut from = None;
            let mut force = false;
            let mut rest = args[1..].iter();
            while let Some(arg) = rest.next() {
                match arg.as_str() {
                    "--force" => force = true,
                    "--from" => {
                        let path = rest.next().ok_or("--from needs a path")?;
                        from = Some(PathBuf::from(path));
                    }
                    other => return Err(format!("unknown stage argument '{other}'")),
                }
            }
            Ok(Cmd::Stage { from, force })
        }
        Some(other) => Err(format!("unknown update command '{other}'")),
    }
}

/// Entry point from `main`: `args` start after `update`.
pub async fn run(data_dir: &Path, args: &[String]) -> anyhow::Result<()> {
    let cmd = match parse(args) {
        Ok(Cmd::Help) => {
            println!("{USAGE}");
            return Ok(());
        }
        Ok(cmd) => cmd,
        Err(message) => {
            eprintln!("{message}\n\n{USAGE}");
            std::process::exit(2);
        }
    };
    let cfg = Config::from_env(data_dir);
    match cmd {
        Cmd::Help => unreachable!("handled above"),
        Cmd::Check => {
            let latest = check(&cfg).await?;
            if latest.newer {
                println!(
                    "{} {} is newer than the running {}",
                    cfg.repo, latest.tag, cfg.running_version
                );
            } else {
                println!(
                    "up to date: {} {} is not newer than the running {}",
                    cfg.repo, latest.tag, cfg.running_version
                );
            }
        }
        Cmd::Stage {
            from: Some(from),
            force,
        } => print_staged(&stage_from(&cfg, &SystemVerifier, &from, force)?),
        Cmd::Stage { from: None, force } => {
            match stage_release(&cfg, &SystemVerifier, force).await? {
                StageOutcome::Staged(staged) => print_staged(&staged),
                StageOutcome::NotNewer(latest) => println!(
                    "nothing staged: {} {} is not newer than the running {}",
                    cfg.repo, latest.tag, cfg.running_version
                ),
            }
        }
    }
    Ok(())
}

fn print_staged(staged: &Staged) {
    println!(
        "staged {} ({}) at {}",
        staged.version,
        staged.commit.as_deref().unwrap_or("commit unknown"),
        staged.path.display()
    );
}

#[cfg(test)]
mod tests;
