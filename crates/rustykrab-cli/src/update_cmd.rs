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
//! Swapping it in is slice 6, `rustykrab update apply`, in [`apply`].

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
  apply [--yes] [--service launchd|script:<start-command>]
        [--url URL] [--installed PATH]
                                 swap the newest staged version in: stop the
                                 daemon, move the installed one to .prev, move
                                 the stage into place, start it and verify it
                                 through /api/version within 90 s; on any
                                 failure restore .prev and record the new one
                                 as bad. Without --yes (or
                                 RUSTYKRAB_UPDATE_AUTO=1) it prints the plan
                                 and changes nothing

Reads RUSTYKRAB_UPDATE_REPO (default gcbh/rustykrab), RUSTYKRAB_GITHUB_TOKEN
(optional, for rate limits), RUSTYKRAB_UPDATE_API_BASE (default
https://api.github.com) and RUSTYKRAB_UPDATE_TEAM_ID (default 3RRX845C4X).
apply also reads RUSTYKRAB_UPDATE_AUTO, RUSTYKRAB_AUTH_TOKEN and
RUSTYKRAB_GATEWAY_URL (the default for --url).";

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
/// The largest release asset `stage` downloads: 256 MiB. A release whose
/// declared size is above it is refused before the download starts.
pub const MAX_ASSET_BYTES: u64 = 256 * 1024 * 1024;

/// Everything the commands read from the environment, so tests can point
/// them at a local stand-in.
#[derive(Clone)]
pub struct Config {
    pub data_dir: PathBuf,
    pub repo: String,
    pub api_base: String,
    pub token: Option<String>,
    pub target: String,
    pub running_version: String,
    pub team_id: String,
}

/// The token never reaches a log line or an error message.
impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("data_dir", &self.data_dir)
            .field("repo", &self.repo)
            .field("api_base", &self.api_base)
            .field("token", &self.token.as_ref().map(|_| "<redacted>"))
            .field("target", &self.target)
            .field("running_version", &self.running_version)
            .field("team_id", &self.team_id)
            .finish()
    }
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

/// The code requirement a release bundle must satisfy: signed by a
/// certificate that chains to Apple, issued under Developer ID (the
/// intermediate's and the leaf's Developer ID marker extensions), to team
/// `team_id`, for [`BUNDLE_ID`]. `codesign` evaluates it against the
/// certificate chain. The `Identifier=` and `TeamIdentifier=` lines of
/// `codesign -dv` are not enough on their own: they are fields of the
/// signature, and an ad-hoc signature (`codesign -s - --team-id ...`) can
/// carry any team.
pub fn designated_requirement(team_id: &str) -> String {
    format!(
        "=anchor apple generic and identifier \"{BUNDLE_ID}\" and \
         certificate leaf[subject.OU] = \"{team_id}\" and \
         certificate 1[field.1.2.840.113635.100.6.2.6] exists and \
         certificate leaf[field.1.2.840.113635.100.6.1.13] exists"
    )
}

/// How long the staged binary's `--version` run may take.
const VERSION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

impl Verifier for SystemVerifier {
    fn verify_signature(&self, app: &Path, team_id: &str) -> anyhow::Result<()> {
        if !cfg!(target_os = "macos") {
            // No signature to check off macOS yet; the digest is the check.
            return Ok(());
        }
        if team_id.is_empty() || !team_id.chars().all(|c| c.is_ascii_alphanumeric()) {
            bail!("team id {team_id:?} is not a Developer ID team");
        }
        let verify = Command::new("codesign")
            .args(["--verify", "--deep", "--strict", "-R"])
            .arg(designated_requirement(team_id))
            .arg(app)
            .output()
            .context("running codesign --verify")?;
        if !verify.status.success() {
            bail!(
                "{} is not signed by Developer ID team {team_id} as {BUNDLE_ID} \
                 (codesign --verify with the requirement): {}",
                app.display(),
                String::from_utf8_lossy(&verify.stderr).trim()
            );
        }
        // The requirement decided; the details only make a refusal of an
        // otherwise valid signature clearer.
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
        let mut cmd = Command::new(binary);
        cmd.arg("--version");
        let out = output_within(cmd, VERSION_TIMEOUT)
            .with_context(|| format!("running {} --version", binary.display()))?;
        if !out.status.success() {
            bail!("{} --version exited with {}", binary.display(), out.status);
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }
}

/// Run `cmd` to completion, or kill it once `limit` passes.
pub fn output_within(
    mut cmd: Command,
    limit: std::time::Duration,
) -> anyhow::Result<std::process::Output> {
    use std::process::Stdio;
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let deadline = std::time::Instant::now() + limit;
    loop {
        if child.try_wait()?.is_some() {
            return Ok(child.wait_with_output()?);
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("did not finish within {}s; killed", limit.as_secs());
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
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
    let parts: Option<Vec<u64>> = text
        .split('.')
        .map(|p| {
            (!p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
                .then(|| p.parse().ok())
                .flatten()
        })
        .collect();
    parts.filter(|p| p.len() == 3)
}

/// Whether `version` is exactly `X.Y.Z` in digits, and so safe to name a
/// directory under `updates/` with.
pub fn is_plain_version(version: &str) -> bool {
    version.bytes().all(|b| b.is_ascii_digit() || b == b'.') && parse_semver(version).is_some()
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
    /// Bytes, as the release API declares them.
    #[serde(default)]
    size: Option<u64>,
}

/// What `check` found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Latest {
    pub tag: String,
    pub version: String,
    pub newer: bool,
    /// Whether the tag is a plain `vX.Y.Z`. A pre-release such as
    /// `v5.4.0-rc.1` is not, and is never newer.
    pub plain: bool,
}

impl Latest {
    /// What `check` prints, and `stage` when it stages nothing.
    pub fn describe(&self, cfg: &Config) -> String {
        if !self.plain {
            format!(
                "nothing newer: {} {} is not a plain vX.Y.Z release (a pre-release?), \
                 so the running {} stays",
                cfg.repo, self.tag, cfg.running_version
            )
        } else if self.newer {
            format!(
                "{} {} is newer than the running {}",
                cfg.repo, self.tag, cfg.running_version
            )
        } else {
            format!(
                "up to date: {} {} is not newer than the running {}",
                cfg.repo, self.tag, cfg.running_version
            )
        }
    }
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
    /// `app` for a `RustyKrab.app`, `binary` for a bare binary.
    #[serde(default)]
    pub kind: String,
    /// Whether the Developer ID signature was checked: always for an app on
    /// macOS, never for a bare binary or off macOS. Slice 6 reads it before
    /// it swaps anything in.
    #[serde(default)]
    pub signature_verified: bool,
}

/// One entry of `<data>/updates/bad.json`, written by a rollback.
///
/// A release is recorded by its version alone. A local build has no tag and
/// every local build reports the package version, so it is recorded by its
/// commit too: an entry with a commit matches only that commit, and one bad
/// local build does not block every later one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BadVersion {
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
}

impl BadVersion {
    /// Whether this entry names `version` built from `commit`.
    pub fn matches(&self, version: &str, commit: Option<&str>) -> bool {
        match &self.commit {
            Some(bad) => commit == Some(bad.as_str()),
            None => self.version == version,
        }
    }
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

/// Check an asset URL before anything is fetched from it, and say whether
/// the token may go with the request. The asset must be `https`, unless
/// `api_base` is itself `http` (a local stand-in). The token goes only to
/// the host and port of `api_base`, never to whatever host the release
/// names; reqwest drops it on a redirect to another host.
pub fn asset_request_sends_token(api_base: &str, asset_url: &str) -> anyhow::Result<bool> {
    let api = reqwest::Url::parse(api_base)
        .with_context(|| format!("RUSTYKRAB_UPDATE_API_BASE {api_base:?} is not a URL"))?;
    let asset = reqwest::Url::parse(asset_url)
        .with_context(|| format!("the asset URL {asset_url:?} is not a URL"))?;
    match asset.scheme() {
        "https" => {}
        "http" if api.scheme() == "http" => {}
        other => bail!("the asset URL {asset_url:?} is {other}, not https; refusing it"),
    }
    Ok(api.host_str().is_some()
        && api.host_str() == asset.host_str()
        && api.port_or_known_default() == asset.port_or_known_default())
}

/// Read a response body in chunks, stopping as soon as it passes `declared`
/// bytes or [`MAX_ASSET_BYTES`], and refusing one shorter than `declared`.
async fn read_capped(
    mut resp: reqwest::Response,
    declared: u64,
    name: &str,
) -> anyhow::Result<Vec<u8>> {
    let limit = declared.min(MAX_ASSET_BYTES);
    let mut body = Vec::with_capacity(usize::try_from(limit).unwrap_or(0));
    while let Some(chunk) = resp
        .chunk()
        .await
        .with_context(|| format!("downloading {name}"))?
    {
        if body.len() as u64 + chunk.len() as u64 > limit {
            bail!(
                "{name} is longer than its declared {declared} bytes \
                 (cap {MAX_ASSET_BYTES}); refusing it"
            );
        }
        body.extend_from_slice(&chunk);
    }
    if body.len() as u64 != declared {
        bail!(
            "{name} is {} bytes, the release declares {declared}; refusing it",
            body.len()
        );
    }
    Ok(body)
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
    // A pre-release or any other odd tag is nothing to stage.
    let plain = is_plain_version(&version);
    let newer = plain && is_newer(&version, &cfg.running_version)?;
    Ok(Latest {
        tag: release.tag_name.clone(),
        version,
        newer,
        plain,
    })
}

/// `rustykrab update check`.
pub async fn check(cfg: &Config) -> anyhow::Result<Latest> {
    let client = http_client()?;
    let release = fetch_latest(&client, cfg).await?;
    latest_of(&release, cfg)
}

/// Every entry of `<data>/updates/bad.json`; none when it does not exist.
pub fn read_bad(cfg: &Config) -> anyhow::Result<Vec<BadVersion>> {
    let path = cfg.updates_dir().join(BAD_FILE);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
}

/// Whether `version` built from `commit` is recorded in
/// `<data>/updates/bad.json` (see [`BadVersion::matches`]).
pub fn is_bad(cfg: &Config, version: &str, commit: Option<&str>) -> anyhow::Result<bool> {
    Ok(read_bad(cfg)?.iter().any(|b| b.matches(version, commit)))
}

/// Add `entry` to `<data>/updates/bad.json`, unless it is already there.
pub fn record_bad(cfg: &Config, entry: BadVersion) -> anyhow::Result<()> {
    let mut bad = read_bad(cfg)?;
    if bad.contains(&entry) {
        return Ok(());
    }
    bad.push(entry);
    let dir = cfg.updates_dir();
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = dir.join(BAD_FILE);
    let tmp = dir.join(format!(".{BAD_FILE}.tmp"));
    std::fs::write(&tmp, serde_json::to_string_pretty(&bad)? + "\n")
        .with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, &path).with_context(|| format!("writing {}", path.display()))
}

fn refuse_bad(
    cfg: &Config,
    version: &str,
    commit: Option<&str>,
    force: bool,
) -> anyhow::Result<()> {
    if !force && is_bad(cfg, version, commit)? {
        let which = commit.map(|c| format!(" ({c})")).unwrap_or_default();
        bail!("{version}{which} is recorded as bad by a rollback; pass --force to stage it anyway");
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
        // A killed stage leaves its scratch behind (Drop never ran).
        for entry in std::fs::read_dir(updates)?.flatten() {
            let stale = entry.file_name().to_string_lossy().starts_with(".staging-");
            if stale && entry.file_type().is_ok_and(|t| t.is_dir()) {
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
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
    // Every path it hands on must be what it says, not a link out of the
    // staging directory.
    let is_link = |p: &Path| std::fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_symlink());
    let app = dir.join(APP_NAME);
    let binary_in_app = app.join("Contents").join("MacOS").join(BINARY_NAME);
    for p in [
        &app,
        &app.join("Contents"),
        &app.join("Contents").join("MacOS"),
        &binary_in_app,
        &dir.join(BINARY_NAME),
    ] {
        if is_link(p) {
            bail!("{} is a symbolic link; refusing it", p.display());
        }
    }
    if app.is_dir() {
        if !binary_in_app.is_file() {
            bail!("{APP_NAME} has no Contents/MacOS/{BINARY_NAME}");
        }
        return Ok((Some(app), binary_in_app));
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
    // The version becomes a directory name, and the directory is replaced:
    // only a plain `X.Y.Z` may name it, never `..` or `.`.
    if !is_plain_version(&staged.version) {
        bail!(
            "the staged binary reports version {:?}, which is not X.Y.Z; refusing it",
            staged.version
        );
    }
    let dest = cfg.updates_dir().join(&staged.version);
    staged.path = dest.join(relative);
    // The record goes in first, so a stage that is in place always has one.
    let record = serde_json::to_string_pretty(&staged)?;
    std::fs::write(scratch.path.join(STAGED_FILE), record + "\n")
        .with_context(|| format!("writing {}", scratch.path.join(STAGED_FILE).display()))?;
    if dest.exists() {
        std::fs::remove_dir_all(&dest).with_context(|| format!("replacing {}", dest.display()))?;
    }
    std::fs::rename(&scratch.path, &dest)
        .with_context(|| format!("moving the staged version to {}", dest.display()))?;
    scratch.keep = true;
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
    // A release is recorded by version; its commit is not known until its
    // binary runs, after the download.
    refuse_bad(cfg, &latest.version, None, force)?;

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
    let declared = asset.size.ok_or_else(|| {
        anyhow!(
            "release {} gives no size for {name}; refusing it",
            latest.tag
        )
    })?;
    if declared > MAX_ASSET_BYTES {
        bail!("{name} declares {declared} bytes, over the {MAX_ASSET_BYTES}-byte cap; refusing it");
    }
    let send_token = asset_request_sends_token(&cfg.api_base, &asset.browser_download_url)?;

    let mut req = client
        .get(&asset.browser_download_url)
        .header("Accept", "application/octet-stream");
    if send_token {
        req = with_auth(req, cfg);
    }
    let resp = req
        .send()
        .await
        .with_context(|| format!("downloading {name}"))?;
    if !resp.status().is_success() {
        bail!("downloading {name}: {}", resp.status());
    }
    let bytes = read_capped(resp, declared, &name).await?;
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
    let kind = kind_of(app.is_some());
    let signature_verified = app.is_some() && cfg!(target_os = "macos");
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
        kind,
        signature_verified,
    };
    let staged = commit_stage(cfg, inner, staged, &relative)?;
    drop(scratch);
    Ok(StageOutcome::Staged(staged))
}

fn kind_of(is_app: bool) -> String {
    if is_app { "app" } else { "binary" }.to_string()
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
        .arg("--")
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
    refuse_bad(cfg, &version, commit.as_deref(), force)?;
    let staged = Staged {
        version,
        tag: None,
        commit,
        source: from.display().to_string(),
        digest: None,
        path: PathBuf::new(),
        staged_at: Utc::now(),
        kind: kind_of(app.is_some()),
        signature_verified: app.is_some() && cfg!(target_os = "macos"),
    };
    commit_stage(cfg, scratch, staged, &relative)
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Cmd {
    Help,
    Check,
    Stage { from: Option<PathBuf>, force: bool },
    Apply(apply::ApplyArgs),
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
        Some("apply") => apply::parse_args(&args[1..]).map(Cmd::Apply),
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
        Cmd::Check => println!("{}", check(&cfg).await?.describe(&cfg)),
        Cmd::Stage {
            from: Some(from),
            force,
        } => print_staged(&stage_from(&cfg, &SystemVerifier, &from, force)?),
        Cmd::Stage { from: None, force } => {
            match stage_release(&cfg, &SystemVerifier, force).await? {
                StageOutcome::Staged(staged) => print_staged(&staged),
                StageOutcome::NotNewer(latest) => {
                    println!("nothing staged: {}", latest.describe(&cfg))
                }
            }
        }
        Cmd::Apply(args) => {
            // Held until the slice 6 review's fixes are re-reviewed and its
            // part 3 lands (update-flow.md, "Slice 6: not yet safe").
            if std::env::var("RUSTYKRAB_UPDATE_APPLY_UNREVIEWED").as_deref() != Ok("1") {
                eprintln!(
                    "rustykrab update apply is not yet safe to run: the fixes from its review \
                     are not yet re-reviewed and some of its points are still open. \
                     See docs/plans/update-flow.md."
                );
                std::process::exit(3);
            }
            apply::run(&cfg, data_dir, args).await?
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

pub mod apply;

#[cfg(test)]
mod tests;
