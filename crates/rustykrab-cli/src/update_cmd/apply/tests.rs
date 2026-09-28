//! `rustykrab update apply` with a scripted service manager, a scripted
//! verifier and a local HTTP stand-in for `/api/version`. The stand-in
//! reports whichever version the installed binary names, so a swap and a
//! rollback show up in what it says, the way they would with a real daemon.
//! Nothing here runs `launchctl` or signals a process.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use axum::http::{HeaderMap, StatusCode};
use axum::routing::get;
use axum::Router;

use super::*;
use crate::update_cmd::{read_bad, stage_from, Verifier, BINARY_NAME, DEFAULT_TEAM_ID};

const TOKEN: &str = "test-token";
const OLD: &str = "old1111";
const NEW: &str = "new2222";

/// How the new version behaves once it is started.
#[derive(Clone, Copy)]
enum NewMode {
    Healthy,
    WrongCommit,
    NoLock,
}

/// The daemon the stand-in pretends to be.
struct Daemon {
    running: bool,
    /// The binary whose content is the commit it reports.
    installed: PathBuf,
    new_mode: NewMode,
    ticks: i64,
    /// How the old version's controller looks.
    old_lock: &'static str,
    old_failed_ticks: u32,
}

type Shared = Arc<Mutex<Daemon>>;

/// Serve `/api/version` for `daemon`; requires the bearer token and an
/// Origin, like the gateway.
async fn stand_in(daemon: Shared) -> Url {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = Url::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
    let app = Router::new().route(
        "/api/version",
        get(move |headers: HeaderMap| {
            let daemon = daemon.clone();
            async move {
                let authed = headers.get("authorization").and_then(|v| v.to_str().ok())
                    == Some(&format!("Bearer {TOKEN}"));
                if !authed || headers.get("origin").is_none() {
                    return Err(StatusCode::UNAUTHORIZED);
                }
                let mut d = daemon.lock().unwrap();
                if !d.running {
                    return Err(StatusCode::SERVICE_UNAVAILABLE);
                }
                d.ticks += 1;
                let tick = DateTime::<Utc>::from_timestamp(1_800_000_000 + d.ticks, 0).unwrap();
                let installed = std::fs::read_to_string(&d.installed).unwrap();
                let installed = installed.trim();
                let (commit, lock, failed) = match (installed == NEW, d.new_mode) {
                    (false, _) => (installed, d.old_lock, d.old_failed_ticks),
                    (true, NewMode::Healthy) => (installed, "held", 0),
                    (true, NewMode::WrongCommit) => ("stale99", "held", 0),
                    (true, NewMode::NoLock) => (NEW, "waiting", 0),
                };
                Ok(axum::Json(serde_json::json!({
                    "version": "5.3.6",
                    "commit": commit,
                    "controller": {
                        "wired": true,
                        "last_tick": tick,
                        "consecutive_failed_ticks": failed,
                        "lock": lock,
                    },
                })))
            }
        }),
    );
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    base
}

/// A service manager that flips the stand-in on and off and records calls.
struct ScriptedService {
    daemon: Shared,
    launchd: bool,
    calls: Mutex<Vec<&'static str>>,
}

impl ScriptedService {
    fn calls(&self) -> Vec<&'static str> {
        self.calls.lock().unwrap().clone()
    }
}

impl ServiceManager for ScriptedService {
    fn is_launchd(&self) -> bool {
        self.launchd
    }

    fn describe(&self) -> String {
        "scripted".to_string()
    }

    fn stop(&self) -> anyhow::Result<()> {
        self.calls.lock().unwrap().push("stop");
        self.daemon.lock().unwrap().running = false;
        Ok(())
    }

    fn start(&self) -> anyhow::Result<()> {
        self.calls.lock().unwrap().push("start");
        self.daemon.lock().unwrap().running = true;
        Ok(())
    }
}

/// A [`Verifier`] for the copy beside the install: `--version` prints the
/// configured version and the binary's content as its commit (or the
/// override), and the signature is good while `signed` holds.
struct ScriptedVerifier {
    version: Mutex<String>,
    commit_override: Mutex<Option<String>>,
    signed: Mutex<bool>,
    signature_checked: Mutex<Vec<PathBuf>>,
}

impl ScriptedVerifier {
    fn new(version: &str) -> Self {
        Self {
            version: Mutex::new(version.to_string()),
            commit_override: Mutex::new(None),
            signed: Mutex::new(true),
            signature_checked: Mutex::new(Vec::new()),
        }
    }
}

impl Verifier for ScriptedVerifier {
    fn verify_signature(&self, app: &Path, _team_id: &str) -> anyhow::Result<()> {
        self.signature_checked
            .lock()
            .unwrap()
            .push(app.to_path_buf());
        if !*self.signed.lock().unwrap() {
            bail!("{} is not signed", app.display());
        }
        Ok(())
    }

    fn run_version(&self, binary: &Path) -> anyhow::Result<String> {
        let commit = match self.commit_override.lock().unwrap().clone() {
            Some(commit) => commit,
            None => std::fs::read_to_string(binary)?.trim().to_string(),
        };
        Ok(format!(
            "rustykrab {} ({commit}, 2026-09-28)\n",
            self.version.lock().unwrap()
        ))
    }
}

fn config(data_dir: &Path) -> Config {
    Config {
        data_dir: data_dir.to_path_buf(),
        repo: super::super::DEFAULT_REPO.to_string(),
        api_base: "http://127.0.0.1:9".to_string(),
        token: None,
        target: "test-target".to_string(),
        running_version: "5.3.6".to_string(),
        team_id: DEFAULT_TEAM_ID.to_string(),
    }
}

/// A `RustyKrab.app` under `dir` whose binary's content is `text`.
fn bundle(dir: &Path, text: &str) -> PathBuf {
    let app = dir.join(APP_NAME);
    let macos = app.join("Contents").join("MacOS");
    std::fs::create_dir_all(&macos).unwrap();
    std::fs::write(macos.join(BINARY_NAME), text).unwrap();
    app
}

fn write_record(dir: &Path, staged: &Staged) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(
        dir.join(STAGED_FILE),
        serde_json::to_string_pretty(staged).unwrap(),
    )
    .unwrap();
}

/// Write a canonical stage of `version`: a bare binary or a bundle whose
/// binary's content is its commit.
fn write_stage_of(
    cfg: &Config,
    version: &str,
    commit: &str,
    tag: Option<&str>,
    kind: &str,
    verified: bool,
) -> Staged {
    let dir = cfg.updates_dir().join(version);
    std::fs::create_dir_all(&dir).unwrap();
    let path = if kind == "app" {
        bundle(&dir, commit)
    } else {
        let path = dir.join(BINARY_NAME);
        std::fs::write(&path, commit).unwrap();
        path
    };
    let staged = Staged {
        version: version.to_string(),
        tag: tag.map(str::to_string),
        commit: Some(commit.to_string()),
        source: "test".to_string(),
        digest: None,
        path,
        staged_at: Utc::now(),
        kind: kind.to_string(),
        signature_verified: verified,
    };
    write_record(&dir, &staged);
    staged
}

fn write_stage(cfg: &Config, commit: &str, tag: Option<&str>, kind: &str, verified: bool) {
    write_stage_of(cfg, "5.3.6", commit, tag, kind, verified);
}

struct Rig {
    _data: tempfile::TempDir,
    _root: tempfile::TempDir,
    cfg: Config,
    /// The binary, bare or inside the installed bundle.
    binary: PathBuf,
    service: ScriptedService,
    swap: DirSwap,
    probe: HttpProbe,
    verifier: ScriptedVerifier,
}

impl Rig {
    /// A bare binary installed, or under launchd a bundle.
    async fn new(new_mode: NewMode, launchd: bool) -> Self {
        let data = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let (installed, binary) = if launchd {
            let app = bundle(root.path(), OLD);
            let binary = app.join("Contents").join("MacOS").join(BINARY_NAME);
            (app, binary)
        } else {
            let binary = root.path().join(BINARY_NAME);
            std::fs::write(&binary, OLD).unwrap();
            (binary.clone(), binary)
        };
        let daemon = Arc::new(Mutex::new(Daemon {
            running: true,
            installed: binary.clone(),
            new_mode,
            ticks: 0,
            old_lock: "held",
            old_failed_ticks: 0,
        }));
        let base = stand_in(daemon.clone()).await;
        Self {
            cfg: config(data.path()),
            _data: data,
            _root: root,
            binary,
            service: ScriptedService {
                daemon,
                launchd,
                calls: Mutex::new(Vec::new()),
            },
            swap: DirSwap { installed },
            probe: HttpProbe::new(&base, TOKEN).unwrap(),
            verifier: ScriptedVerifier::new("5.3.6"),
        }
    }

    fn host(&self) -> Host<'_> {
        Host {
            service: &self.service,
            swap: &self.swap,
            probe: &self.probe,
            verifier: &self.verifier,
            verify_within: Duration::from_secs(2),
            poll: Duration::from_millis(10),
        }
    }

    fn installed(&self) -> String {
        std::fs::read_to_string(&self.binary).unwrap()
    }

    fn prev(&self) -> Option<PathBuf> {
        let prev = sibling(&self.swap.installed, "", ".prev").unwrap();
        std::fs::symlink_metadata(&prev).is_ok().then_some(prev)
    }

    fn next(&self) -> PathBuf {
        sibling(&self.swap.installed, ".", ".next").unwrap()
    }

    /// Nothing was changed: no service call, the old version in place and
    /// no `.prev` or `.next` beside it.
    fn assert_untouched(&self) {
        assert!(
            self.service.calls().is_empty(),
            "{:?}",
            self.service.calls()
        );
        assert_eq!(self.installed(), OLD);
        assert_eq!(self.prev(), None);
        assert!(!self.next().exists());
    }
}

#[tokio::test]
async fn without_yes_nothing_changes() {
    let rig = Rig::new(NewMode::Healthy, false).await;
    write_stage(&rig.cfg, NEW, None, "binary", false);

    let outcome = apply(&rig.cfg, &rig.host(), false).await.unwrap();
    let Outcome::Planned(plan) = outcome else {
        panic!("expected a plan, got {outcome:?}");
    };
    assert_eq!(plan.running.commit.as_deref(), Some(OLD));
    assert_eq!(plan.staged.commit.as_deref(), Some(NEW));
    rig.assert_untouched();
    assert!(read_bad(&rig.cfg).unwrap().is_empty());
}

#[tokio::test]
async fn a_healthy_new_version_is_left_in_place_with_prev_kept() {
    let rig = Rig::new(NewMode::Healthy, false).await;
    *rig.verifier.version.lock().unwrap() = "5.3.7".to_string();
    write_stage_of(&rig.cfg, "5.3.7", NEW, Some("v5.3.7"), "binary", false);

    let outcome = apply(&rig.cfg, &rig.host(), true).await.unwrap();
    assert!(matches!(outcome, Outcome::Applied(_)), "{outcome:?}");
    assert_eq!(rig.service.calls(), ["stop", "start"]);
    assert_eq!(rig.installed(), NEW);
    let prev = rig.prev().expect(".prev is kept");
    assert_eq!(std::fs::read_to_string(prev).unwrap(), OLD);
    assert!(!rig.next().exists());
    assert!(read_bad(&rig.cfg).unwrap().is_empty());
    // No signature check off launchd.
    assert!(rig.verifier.signature_checked.lock().unwrap().is_empty());

    // Applying again finds it already running and changes nothing.
    let again = apply(&rig.cfg, &rig.host(), true).await.unwrap();
    assert!(matches!(again, Outcome::AlreadyRunning(_)), "{again:?}");
    assert_eq!(rig.service.calls(), ["stop", "start"]);
}

#[tokio::test]
async fn a_version_that_never_reports_the_staged_commit_is_rolled_back() {
    let rig = Rig::new(NewMode::WrongCommit, false).await;
    *rig.verifier.version.lock().unwrap() = "5.3.7".to_string();
    write_stage_of(&rig.cfg, "5.3.7", NEW, Some("v5.3.7"), "binary", false);

    let outcome = apply(&rig.cfg, &rig.host(), true).await.unwrap();
    let Outcome::RolledBack { reason, .. } = outcome else {
        panic!("expected a rollback, got {outcome:?}");
    };
    assert!(reason.contains("stale99"), "{reason}");
    assert_eq!(rig.service.calls(), ["stop", "start", "stop", "start"]);
    assert_eq!(rig.installed(), OLD, ".prev is restored");
    assert_eq!(rig.prev(), None);
    // The old version was verified: the stand-in answers with its commit.
    assert_eq!(
        rig.probe.probe().await.unwrap().commit.as_deref(),
        Some(OLD)
    );
    // A release is recorded by its version alone.
    assert_eq!(
        read_bad(&rig.cfg).unwrap(),
        [BadVersion {
            version: "5.3.7".to_string(),
            commit: None
        }]
    );

    // And it is not applied again.
    let err = apply(&rig.cfg, &rig.host(), true).await.unwrap_err();
    assert!(err.to_string().contains("recorded as bad"), "{err:#}");
    assert_eq!(rig.service.calls().len(), 4, "nothing was stopped");
}

#[tokio::test]
async fn a_version_that_never_holds_the_lock_is_rolled_back() {
    let rig = Rig::new(NewMode::NoLock, false).await;
    write_stage(&rig.cfg, NEW, None, "binary", false);

    let outcome = apply(&rig.cfg, &rig.host(), true).await.unwrap();
    let Outcome::RolledBack { reason, .. } = outcome else {
        panic!("expected a rollback, got {outcome:?}");
    };
    assert!(reason.contains("waiting"), "{reason}");
    assert_eq!(rig.installed(), OLD);
    assert_eq!(rig.prev(), None);
    // A local build (no tag) is recorded by its commit.
    assert_eq!(
        read_bad(&rig.cfg).unwrap(),
        [BadVersion {
            version: "5.3.6".to_string(),
            commit: Some(NEW.to_string())
        }]
    );
    let err = apply(&rig.cfg, &rig.host(), true).await.unwrap_err();
    assert!(err.to_string().contains("recorded as bad"), "{err:#}");
}

/// A [`Verifier`] whose staged binary prints `rustykrab 5.3.6 (<commit>, ...)`.
struct PrintsCommit(&'static str);

impl Verifier for PrintsCommit {
    fn verify_signature(&self, _app: &Path, _team_id: &str) -> anyhow::Result<()> {
        Ok(())
    }

    fn run_version(&self, _binary: &Path) -> anyhow::Result<String> {
        Ok(format!("rustykrab 5.3.6 ({}, 2026-09-28)\n", self.0))
    }
}

#[tokio::test]
async fn a_rolled_back_local_build_blocks_its_commit_not_its_version() {
    let rig = Rig::new(NewMode::NoLock, false).await;
    write_stage(&rig.cfg, NEW, None, "binary", false);
    let outcome = apply(&rig.cfg, &rig.host(), true).await.unwrap();
    assert!(matches!(outcome, Outcome::RolledBack { .. }), "{outcome:?}");

    let build = tempfile::tempdir().unwrap();
    let binary = build.path().join(BINARY_NAME);
    std::fs::write(&binary, "#!/bin/sh\n").unwrap();

    // The same commit is refused by `stage --from`.
    let err = stage_from(&rig.cfg, &PrintsCommit(NEW), &binary, false).unwrap_err();
    assert!(err.to_string().contains("recorded as bad"), "{err:#}");

    // Another commit of the same version stages.
    let staged = stage_from(&rig.cfg, &PrintsCommit("later33"), &binary, false).unwrap();
    assert_eq!(staged.version, "5.3.6");
    assert_eq!(staged.commit.as_deref(), Some("later33"));

    // A version-only entry, as a release rollback writes, still blocks the
    // version whatever the commit.
    record_bad(
        &rig.cfg,
        BadVersion {
            version: "5.3.6".to_string(),
            commit: None,
        },
    )
    .unwrap();
    assert!(stage_from(&rig.cfg, &PrintsCommit("other44"), &binary, false).is_err());
}

#[tokio::test]
async fn launchd_refuses_a_stage_whose_signature_was_not_verified() {
    let rig = Rig::new(NewMode::Healthy, true).await;

    write_stage(&rig.cfg, NEW, None, "app", false);
    let err = apply(&rig.cfg, &rig.host(), true).await.unwrap_err();
    assert!(err.to_string().contains("signature"), "{err:#}");

    std::fs::remove_dir_all(rig.cfg.updates_dir()).unwrap();
    write_stage(&rig.cfg, NEW, None, "binary", true);
    let err = apply(&rig.cfg, &rig.host(), true).await.unwrap_err();
    assert!(err.to_string().contains("kind"), "{err:#}");

    rig.assert_untouched();
}

#[tokio::test]
async fn launchd_checks_the_signature_of_the_copy_and_refuses_an_unsigned_one() {
    let rig = Rig::new(NewMode::Healthy, true).await;
    // The record claims a verified signature; the copy is checked anyway.
    write_stage(&rig.cfg, NEW, None, "app", true);
    *rig.verifier.signed.lock().unwrap() = false;

    let err = apply(&rig.cfg, &rig.host(), true).await.unwrap_err();
    assert!(format!("{err:#}").contains("not signed"), "{err:#}");
    assert_eq!(
        *rig.verifier.signature_checked.lock().unwrap(),
        [rig.next()],
        "the signature checked is the copy's"
    );
    // The old version was started again and nothing moved.
    assert_eq!(rig.service.calls(), ["stop", "start"]);
    assert_eq!(rig.installed(), OLD);
    assert_eq!(rig.prev(), None);
    assert!(!rig.next().exists());

    // Signed, it applies.
    *rig.verifier.signed.lock().unwrap() = true;
    let outcome = apply(&rig.cfg, &rig.host(), true).await.unwrap();
    assert!(matches!(outcome, Outcome::Applied(_)), "{outcome:?}");
    assert_eq!(rig.installed(), NEW);
}

#[tokio::test]
async fn a_copy_that_reports_another_commit_or_version_is_refused() {
    let rig = Rig::new(NewMode::Healthy, false).await;
    write_stage(&rig.cfg, NEW, None, "binary", false);

    *rig.verifier.commit_override.lock().unwrap() = Some("evil999".to_string());
    let err = apply(&rig.cfg, &rig.host(), true).await.unwrap_err();
    assert!(format!("{err:#}").contains("evil999"), "{err:#}");
    assert_eq!(rig.service.calls(), ["stop", "start"]);
    assert_eq!(rig.installed(), OLD);
    assert_eq!(rig.prev(), None);
    assert!(!rig.next().exists());

    *rig.verifier.commit_override.lock().unwrap() = None;
    *rig.verifier.version.lock().unwrap() = "9.9.9".to_string();
    let err = apply(&rig.cfg, &rig.host(), true).await.unwrap_err();
    assert!(format!("{err:#}").contains("9.9.9"), "{err:#}");
    assert_eq!(rig.installed(), OLD);
    assert!(read_bad(&rig.cfg).unwrap().is_empty());
}

#[tokio::test]
async fn a_record_outside_its_version_directory_is_refused() {
    let rig = Rig::new(NewMode::Healthy, false).await;
    // A record in updates/5.3.9/ that names 5.3.6.
    let staged = write_stage_of(&rig.cfg, "5.3.6", NEW, None, "binary", false);
    let dir = rig.cfg.updates_dir().join("5.3.9");
    write_record(
        &dir,
        &Staged {
            staged_at: Utc::now() + chrono::Duration::seconds(5),
            ..staged
        },
    );
    let err = apply(&rig.cfg, &rig.host(), true).await.unwrap_err();
    assert!(err.to_string().contains("names version"), "{err:#}");
    rig.assert_untouched();

    // A version that is not plain X.Y.Z is refused too, even in its own
    // directory.
    std::fs::remove_dir_all(rig.cfg.updates_dir()).unwrap();
    let mut staged = write_stage_of(&rig.cfg, "5.3.6", NEW, None, "binary", false);
    staged.version = "..".to_string();
    write_record(&rig.cfg.updates_dir().join("5.3.6"), &staged);
    let err = apply(&rig.cfg, &rig.host(), true).await.unwrap_err();
    assert!(err.to_string().contains("names version"), "{err:#}");
    rig.assert_untouched();
}

#[tokio::test]
async fn a_record_whose_path_is_not_canonical_is_refused() {
    let rig = Rig::new(NewMode::Healthy, false).await;
    let elsewhere = tempfile::tempdir().unwrap();
    let outside = elsewhere.path().join(BINARY_NAME);
    std::fs::write(&outside, NEW).unwrap();
    let mut staged = write_stage_of(&rig.cfg, "5.3.6", NEW, None, "binary", false);
    staged.path = outside;
    write_record(&rig.cfg.updates_dir().join("5.3.6"), &staged);

    let err = apply(&rig.cfg, &rig.host(), true).await.unwrap_err();
    assert!(err.to_string().contains("canonical"), "{err:#}");
    rig.assert_untouched();

    // The right directory, but the kind names the other file.
    let mut staged = write_stage_of(&rig.cfg, "5.3.6", NEW, None, "binary", false);
    staged.kind = "app".to_string();
    write_record(&rig.cfg.updates_dir().join("5.3.6"), &staged);
    let err = apply(&rig.cfg, &rig.host(), true).await.unwrap_err();
    assert!(err.to_string().contains("canonical"), "{err:#}");
    rig.assert_untouched();
}

#[tokio::test]
async fn a_symlinked_stage_is_refused() {
    let rig = Rig::new(NewMode::Healthy, false).await;
    let staged = write_stage_of(&rig.cfg, "5.3.6", NEW, None, "binary", false);
    let elsewhere = tempfile::tempdir().unwrap();
    let outside = elsewhere.path().join("payload");
    std::fs::write(&outside, NEW).unwrap();
    std::fs::remove_file(&staged.path).unwrap();
    std::os::unix::fs::symlink(&outside, &staged.path).unwrap();

    let err = apply(&rig.cfg, &rig.host(), true).await.unwrap_err();
    assert!(err.to_string().contains("symbolic link"), "{err:#}");
    rig.assert_untouched();
}

#[test]
fn the_swap_refuses_a_copy_that_is_a_symlink() {
    let root = tempfile::tempdir().unwrap();
    let installed = root.path().join(BINARY_NAME);
    std::fs::write(&installed, OLD).unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    let target = elsewhere.path().join("payload");
    std::fs::write(&target, NEW).unwrap();
    let link = elsewhere.path().join(BINARY_NAME);
    std::os::unix::fs::symlink(&target, &link).unwrap();

    let verifier = ScriptedVerifier::new("5.3.6");
    let check = NextCheck {
        verifier: &verifier,
        team_id: None,
        version: "5.3.6",
        commit: NEW,
    };
    let swap = DirSwap {
        installed: installed.clone(),
    };
    // `cp -R` copies the link as a link, and the copy is refused.
    let err = swap.swap_in(&link, &check).unwrap_err();
    assert!(err.to_string().contains("not a regular file"), "{err:#}");
    assert_eq!(std::fs::read_to_string(&installed).unwrap(), OLD);
    assert!(std::fs::symlink_metadata(sibling(&installed, ".", ".next").unwrap()).is_err());
}

#[tokio::test]
async fn an_older_or_equal_release_is_refused() {
    let rig = Rig::new(NewMode::Healthy, false).await;
    *rig.verifier.version.lock().unwrap() = "5.3.5".to_string();
    write_stage_of(&rig.cfg, "5.3.5", NEW, Some("v5.3.5"), "binary", false);
    let err = apply(&rig.cfg, &rig.host(), true).await.unwrap_err();
    assert!(err.to_string().contains("not newer"), "{err:#}");
    rig.assert_untouched();

    std::fs::remove_dir_all(rig.cfg.updates_dir()).unwrap();
    write_stage(&rig.cfg, NEW, Some("v5.3.6"), "binary", false);
    let err = apply(&rig.cfg, &rig.host(), true).await.unwrap_err();
    assert!(err.to_string().contains("not newer"), "{err:#}");
    rig.assert_untouched();
}

#[tokio::test]
async fn an_unhealthy_running_daemon_is_not_updated() {
    let rig = Rig::new(NewMode::Healthy, false).await;
    write_stage(&rig.cfg, NEW, None, "binary", false);

    rig.service.daemon.lock().unwrap().old_failed_ticks = 3;
    let err = apply(&rig.cfg, &rig.host(), true).await.unwrap_err();
    assert!(
        err.to_string().contains("consecutive_failed_ticks 3"),
        "{err:#}"
    );
    rig.assert_untouched();

    {
        let mut d = rig.service.daemon.lock().unwrap();
        d.old_failed_ticks = 0;
        d.old_lock = "waiting";
    }
    let err = apply(&rig.cfg, &rig.host(), true).await.unwrap_err();
    assert!(
        err.to_string().contains("controller.lock waiting"),
        "{err:#}"
    );
    rig.assert_untouched();
}

#[tokio::test]
async fn an_unparseable_record_is_skipped() {
    let rig = Rig::new(NewMode::Healthy, false).await;
    write_stage(&rig.cfg, NEW, None, "binary", false);
    let junk = rig.cfg.updates_dir().join("5.3.8");
    std::fs::create_dir_all(&junk).unwrap();
    std::fs::write(junk.join(STAGED_FILE), "{ not json").unwrap();

    let outcome = apply(&rig.cfg, &rig.host(), false).await.unwrap();
    let Outcome::Planned(plan) = outcome else {
        panic!("expected a plan, got {outcome:?}");
    };
    assert_eq!(plan.staged.version, "5.3.6");
}

/// A probe that answers from a script, repeating its last answer.
struct Sequence(Mutex<VecDeque<(i64, u32)>>);

#[async_trait]
impl VersionProbe for Sequence {
    async fn probe(&self) -> anyhow::Result<VersionReport> {
        let mut steps = self.0.lock().unwrap();
        let (tick, failed) = if steps.len() > 1 {
            steps.pop_front().unwrap()
        } else {
            steps[0]
        };
        Ok(VersionReport {
            version: "5.3.6".to_string(),
            commit: Some(NEW.to_string()),
            controller: ControllerReport {
                last_tick: DateTime::<Utc>::from_timestamp(1_800_000_000 + tick, 0),
                consecutive_failed_ticks: Some(failed),
                lock: Some("held".to_string()),
            },
        })
    }
}

#[tokio::test]
async fn verify_ignores_tick_advances_while_ticks_fail() {
    let service = ScriptedService {
        daemon: Arc::new(Mutex::new(Daemon {
            running: true,
            installed: PathBuf::new(),
            new_mode: NewMode::Healthy,
            ticks: 0,
            old_lock: "held",
            old_failed_ticks: 0,
        })),
        launchd: false,
        calls: Mutex::new(Vec::new()),
    };
    let swap = DirSwap {
        installed: PathBuf::from("/nonexistent"),
    };
    let verifier = ScriptedVerifier::new("5.3.6");
    let run = |steps: &[(i64, u32)]| {
        let probe = Sequence(Mutex::new(steps.iter().copied().collect()));
        let service = &service;
        let swap = &swap;
        let verifier = &verifier;
        async move {
            let host = Host {
                service,
                swap,
                probe: &probe,
                verifier,
                verify_within: Duration::from_millis(300),
                poll: Duration::from_millis(5),
            };
            verify(&host, NEW).await
        }
    };

    // Ticks advance while failing, one healthy answer, then failing again
    // with the tick stuck: never two healthy advances.
    let err = run(&[(1, 2), (2, 3), (3, 4), (4, 0), (4, 1)])
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("consecutive_failed_ticks"),
        "{err:#}"
    );

    // Ticks advance while failing, then stop: still not healthy.
    let err = run(&[(1, 1), (2, 1), (3, 1), (3, 0)]).await.unwrap_err();
    assert!(err.to_string().contains("advanced 0 of 2"), "{err:#}");

    // Two advances with no failing ticks pass.
    run(&[(1, 1), (2, 0), (3, 0), (4, 0)]).await.unwrap();
}

#[test]
fn the_swap_moves_a_bundle_and_restores_it() {
    let root = tempfile::tempdir().unwrap();
    let stage = tempfile::tempdir().unwrap();
    let installed = bundle(root.path(), OLD);
    let staged = bundle(stage.path(), NEW);
    // An older .prev is replaced.
    std::fs::create_dir_all(root.path().join(format!("{APP_NAME}.prev"))).unwrap();
    let binary = |app: &Path| {
        std::fs::read_to_string(app.join("Contents").join("MacOS").join(BINARY_NAME)).unwrap()
    };

    let verifier = ScriptedVerifier::new("5.3.6");
    let check = NextCheck {
        verifier: &verifier,
        team_id: Some(DEFAULT_TEAM_ID),
        version: "5.3.6",
        commit: NEW,
    };
    let swap = DirSwap {
        installed: installed.clone(),
    };
    swap.swap_in(&staged, &check).unwrap();
    assert_eq!(binary(&installed), NEW);
    assert_eq!(binary(&root.path().join(format!("{APP_NAME}.prev"))), OLD);
    assert_eq!(binary(&staged), NEW, "the stage is copied, not consumed");
    assert_eq!(
        *verifier.signature_checked.lock().unwrap(),
        [root.path().join(format!(".{APP_NAME}.next"))]
    );

    swap.restore().unwrap();
    assert_eq!(binary(&installed), OLD);
    let mut left: Vec<_> = std::fs::read_dir(root.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    left.sort();
    assert_eq!(left, [APP_NAME]);
}

#[test]
fn parses_apply_arguments() {
    let args = |s: &str| s.split_whitespace().map(String::from).collect::<Vec<_>>();
    assert_eq!(
        parse_args(&args("")).unwrap(),
        ApplyArgs {
            yes: false,
            service: ServiceSpec::Launchd,
            url: None,
            installed: None,
        }
    );
    assert_eq!(
        parse_args(&args(
            "--yes --service script:./start.sh --url http://127.0.0.1:3100 --installed /opt/rk"
        ))
        .unwrap(),
        ApplyArgs {
            yes: true,
            service: ServiceSpec::Script("./start.sh".to_string()),
            url: Some("http://127.0.0.1:3100".to_string()),
            installed: Some(PathBuf::from("/opt/rk")),
        }
    );
    assert!(parse_args(&args("--service script:")).is_err());
    assert!(parse_args(&args("--url")).is_err());
}

#[test]
fn the_url_must_be_loopback_or_https() {
    for ok in [
        "http://127.0.0.1:3100",
        "http://[::1]:3100",
        "http://localhost:3100",
        "http://LOCALHOST:3100",
        "https://daemon.example.com",
    ] {
        check_url(ok).unwrap_or_else(|e| panic!("{ok}: {e:#}"));
    }
    for bad in [
        "http://192.168.1.10:3100",
        "http://daemon.example.com",
        "http://127.0.0.1.example.com",
        "http://localhost.evil.com:3100",
        "ftp://127.0.0.1",
    ] {
        assert!(check_url(bad).is_err(), "{bad} was accepted");
    }
    let err = check_url("http://10.0.0.2:3100").unwrap_err();
    assert!(err.to_string().contains("loopback"), "{err:#}");
}

/// The host's processes, scripted: who listens where, and what each runs.
struct ScriptedProcesses {
    listeners: Vec<Listener>,
    exes: Vec<(u32, PathBuf)>,
    terminated: Arc<Mutex<Vec<u32>>>,
}

impl Processes for ScriptedProcesses {
    fn listeners(&self, _port: u16) -> anyhow::Result<Vec<Listener>> {
        Ok(self.listeners.clone())
    }

    fn executable(&self, pid: u32) -> anyhow::Result<PathBuf> {
        self.exes
            .iter()
            .find(|(p, _)| *p == pid)
            .map(|(_, exe)| exe.clone())
            .ok_or_else(|| anyhow!("no process {pid}"))
    }

    fn terminate(&self, pid: u32) -> anyhow::Result<()> {
        self.terminated.lock().unwrap().push(pid);
        Ok(())
    }

    fn alive(&self, pid: u32) -> bool {
        !self.terminated.lock().unwrap().contains(&pid)
    }
}

#[test]
fn the_script_service_stops_only_the_installed_binary_on_loopback() {
    let root = tempfile::tempdir().unwrap();
    let installed = root.path().join(BINARY_NAME);
    std::fs::write(&installed, OLD).unwrap();
    let other = root.path().join("something-else");
    std::fs::write(&other, "").unwrap();
    let base = Url::parse("http://127.0.0.1:3100").unwrap();
    let listen = |pid: u32, address: &str| Listener {
        pid,
        address: address.to_string(),
    };

    let stop = |listeners: Vec<Listener>, exes: Vec<(u32, PathBuf)>| {
        let terminated = Arc::new(Mutex::new(Vec::new()));
        let processes = ScriptedProcesses {
            listeners,
            exes,
            terminated: terminated.clone(),
        };
        let script = Script::new(
            "true".to_string(),
            &base,
            installed.clone(),
            Box::new(processes),
        )
        .unwrap();
        let result = script.stop();
        let terminated = terminated.lock().unwrap().clone();
        (result, terminated)
    };

    // The installed binary on loopback is stopped.
    let (result, terminated) = stop(
        vec![listen(42, "127.0.0.1:3100"), listen(42, "[::1]:3100")],
        vec![(42, installed.clone())],
    );
    result.unwrap();
    assert_eq!(terminated, [42]);

    let refusals = [
        // Another executable on the port.
        (
            vec![listen(42, "127.0.0.1:3100")],
            vec![(42, other.clone())],
            "not the installed",
        ),
        // Listening on every interface.
        (
            vec![listen(42, "*:3100")],
            vec![(42, installed.clone())],
            "not on loopback",
        ),
        (
            vec![listen(42, "127.0.0.1:3100"), listen(42, "10.0.0.2:3100")],
            vec![(42, installed.clone())],
            "not on loopback",
        ),
        // Two processes.
        (
            vec![listen(42, "127.0.0.1:3100"), listen(43, "127.0.0.1:3100")],
            vec![(42, installed.clone()), (43, installed.clone())],
            "refusing to pick one",
        ),
        // Nobody, though the daemon answered.
        (Vec::new(), Vec::new(), "nothing is found listening"),
    ];
    for (listeners, exes, expected) in refusals {
        let (result, terminated) = stop(listeners, exes);
        let err = result.unwrap_err();
        assert!(err.to_string().contains(expected), "{err:#}");
        assert!(terminated.is_empty(), "{expected}: something was signalled");
    }
}

#[test]
fn reads_lsof_listeners() {
    let text = "p42\nf7\nn127.0.0.1:3100\nf8\nn[::1]:3100\np43\nf3\nn*:3100\n";
    assert_eq!(
        parse_lsof(text),
        [
            Listener {
                pid: 42,
                address: "127.0.0.1:3100".to_string()
            },
            Listener {
                pid: 42,
                address: "[::1]:3100".to_string()
            },
            Listener {
                pid: 43,
                address: "*:3100".to_string()
            },
        ]
    );
    assert!(is_loopback_address("127.0.0.1:3100"));
    assert!(is_loopback_address("[::1]:3100"));
    assert!(!is_loopback_address("*:3100"));
    assert!(!is_loopback_address("0.0.0.0:3100"));
    assert!(!is_loopback_address("[::]:3100"));
}
