//! `rustykrab update apply` with a scripted service manager and a local
//! HTTP stand-in for `/api/version`. The stand-in reports whichever version
//! the installed binary names, so a swap and a rollback show up in what it
//! says, the way they would with a real daemon. Nothing here runs
//! `launchctl` or signals a process.

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
    installed: PathBuf,
    new_mode: NewMode,
    ticks: i64,
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
                let (commit, lock) = match (installed == NEW, d.new_mode) {
                    (false, _) | (true, NewMode::Healthy) => (installed, "held"),
                    (true, NewMode::WrongCommit) => ("stale99", "held"),
                    (true, NewMode::NoLock) => (NEW, "waiting"),
                };
                Ok(axum::Json(serde_json::json!({
                    "version": "5.3.6",
                    "commit": commit,
                    "controller": {
                        "wired": true,
                        "last_tick": tick,
                        "consecutive_failed_ticks": 0,
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

/// Write a stage of a bare binary whose content is its commit.
fn write_stage(cfg: &Config, commit: &str, tag: Option<&str>, kind: &str, verified: bool) {
    let dir = cfg.updates_dir().join("5.3.6");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(BINARY_NAME);
    std::fs::write(&path, commit).unwrap();
    let staged = Staged {
        version: "5.3.6".to_string(),
        tag: tag.map(str::to_string),
        commit: Some(commit.to_string()),
        source: "test".to_string(),
        digest: None,
        path,
        staged_at: Utc::now(),
        kind: kind.to_string(),
        signature_verified: verified,
    };
    std::fs::write(
        dir.join(STAGED_FILE),
        serde_json::to_string_pretty(&staged).unwrap(),
    )
    .unwrap();
}

struct Rig {
    _data: tempfile::TempDir,
    _root: tempfile::TempDir,
    cfg: Config,
    installed: PathBuf,
    service: ScriptedService,
    swap: DirSwap,
    probe: HttpProbe,
}

impl Rig {
    async fn new(new_mode: NewMode, launchd: bool) -> Self {
        let data = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let installed = root.path().join(BINARY_NAME);
        std::fs::write(&installed, OLD).unwrap();
        let daemon = Arc::new(Mutex::new(Daemon {
            running: true,
            installed: installed.clone(),
            new_mode,
            ticks: 0,
        }));
        let base = stand_in(daemon.clone()).await;
        Self {
            cfg: config(data.path()),
            _data: data,
            _root: root,
            installed: installed.clone(),
            service: ScriptedService {
                daemon,
                launchd,
                calls: Mutex::new(Vec::new()),
            },
            swap: DirSwap { installed },
            probe: HttpProbe::new(&base, TOKEN).unwrap(),
        }
    }

    fn host(&self) -> Host<'_> {
        Host {
            service: &self.service,
            swap: &self.swap,
            probe: &self.probe,
            verify_within: Duration::from_secs(2),
            poll: Duration::from_millis(10),
        }
    }

    fn installed(&self) -> String {
        std::fs::read_to_string(&self.installed).unwrap()
    }

    fn prev(&self) -> Option<String> {
        std::fs::read_to_string(sibling(&self.installed, "", ".prev").unwrap()).ok()
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
    assert!(rig.service.calls().is_empty());
    assert_eq!(rig.installed(), OLD);
    assert_eq!(rig.prev(), None);
    assert!(read_bad(&rig.cfg).unwrap().is_empty());
}

#[tokio::test]
async fn a_healthy_new_version_is_left_in_place_with_prev_kept() {
    let rig = Rig::new(NewMode::Healthy, false).await;
    write_stage(&rig.cfg, NEW, Some("v5.3.6"), "binary", false);

    let outcome = apply(&rig.cfg, &rig.host(), true).await.unwrap();
    assert!(matches!(outcome, Outcome::Applied(_)), "{outcome:?}");
    assert_eq!(rig.service.calls(), ["stop", "start"]);
    assert_eq!(rig.installed(), NEW);
    assert_eq!(rig.prev().as_deref(), Some(OLD));
    assert!(read_bad(&rig.cfg).unwrap().is_empty());

    // Applying again finds it already running and changes nothing.
    let again = apply(&rig.cfg, &rig.host(), true).await.unwrap();
    assert!(matches!(again, Outcome::AlreadyRunning(_)), "{again:?}");
    assert_eq!(rig.service.calls(), ["stop", "start"]);
}

#[tokio::test]
async fn a_version_that_never_reports_the_staged_commit_is_rolled_back() {
    let rig = Rig::new(NewMode::WrongCommit, false).await;
    write_stage(&rig.cfg, NEW, Some("v5.3.6"), "binary", false);

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
            version: "5.3.6".to_string(),
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

    write_stage(&rig.cfg, NEW, None, "binary", true);
    let err = apply(&rig.cfg, &rig.host(), true).await.unwrap_err();
    assert!(err.to_string().contains("kind"), "{err:#}");

    assert!(rig.service.calls().is_empty());
    assert_eq!(rig.installed(), OLD);
    assert_eq!(rig.prev(), None);
}

#[test]
fn the_swap_moves_a_bundle_and_restores_it() {
    let root = tempfile::tempdir().unwrap();
    let stage = tempfile::tempdir().unwrap();
    let bundle = |dir: &Path, text: &str| {
        let app = dir.join(APP_NAME);
        let macos = app.join("Contents").join("MacOS");
        std::fs::create_dir_all(&macos).unwrap();
        std::fs::write(macos.join(BINARY_NAME), text).unwrap();
        app
    };
    let installed = bundle(root.path(), OLD);
    let staged = bundle(stage.path(), NEW);
    // An older .prev is replaced.
    std::fs::create_dir_all(root.path().join(format!("{APP_NAME}.prev"))).unwrap();
    let binary = |app: &Path| {
        std::fs::read_to_string(app.join("Contents").join("MacOS").join(BINARY_NAME)).unwrap()
    };

    let swap = DirSwap {
        installed: installed.clone(),
    };
    swap.swap_in(&staged).unwrap();
    assert_eq!(binary(&installed), NEW);
    assert_eq!(binary(&root.path().join(format!("{APP_NAME}.prev"))), OLD);
    assert_eq!(binary(&staged), NEW, "the stage is copied, not consumed");

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
