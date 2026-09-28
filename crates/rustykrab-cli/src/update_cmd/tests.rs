//! `rustykrab update` against a local stand-in for the GitHub releases API,
//! serving a release and an archive built in the test. The signature check
//! and the `--version` run are scripted, except in the macOS test that runs
//! the real `codesign`.

use std::sync::{Arc, Mutex};

use axum::routing::get;
use axum::Router;

use super::*;

const TARGET: &str = "test-target";
const RUNNING: &str = "5.3.6";

/// A scripted [`Verifier`] that records what it was asked to do.
struct Scripted {
    signature: Result<(), String>,
    version_output: String,
    calls: Mutex<Vec<String>>,
}

impl Scripted {
    fn passing(version: &str) -> Self {
        Self {
            signature: Ok(()),
            version_output: format!("rustykrab {version} (abc1234, 2026-09-28)\n"),
            calls: Mutex::new(Vec::new()),
        }
    }

    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
}

impl Verifier for Scripted {
    fn verify_signature(&self, _app: &Path, team_id: &str) -> anyhow::Result<()> {
        self.calls.lock().unwrap().push(format!("sign:{team_id}"));
        self.signature.clone().map_err(|e| anyhow!(e))
    }

    fn run_version(&self, _binary: &Path) -> anyhow::Result<String> {
        self.calls.lock().unwrap().push("version".to_string());
        Ok(self.version_output.clone())
    }
}

/// A `RustyKrab.app` layout with a placeholder binary, as a directory.
fn bundle_in(dir: &Path) -> PathBuf {
    let app = dir.join(APP_NAME);
    let macos = app.join("Contents").join("MacOS");
    std::fs::create_dir_all(&macos).unwrap();
    std::fs::write(macos.join(BINARY_NAME), "#!/bin/sh\necho placeholder\n").unwrap();
    app
}

/// `rustykrab-<target>.tar.gz` holding a bundle, built with the system tar.
fn archive() -> Vec<u8> {
    let dir = tempfile::tempdir().unwrap();
    bundle_in(dir.path());
    let out = dir.path().join("out.tar.gz");
    let status = Command::new("tar")
        .arg("-czf")
        .arg(&out)
        .arg("-C")
        .arg(dir.path())
        .arg(APP_NAME)
        .status()
        .unwrap();
    assert!(status.success());
    std::fs::read(out).unwrap()
}

enum Digest {
    Right,
    Wrong,
    Missing,
}

/// Serve `/repos/gcbh/rustykrab/releases/latest` and the asset; returns
/// the base URL and a count of asset downloads.
async fn stand_in(tag: &str, digest: Digest, bytes: Vec<u8>) -> (String, Arc<Mutex<usize>>) {
    let served = stand_in_with(tag, digest, bytes, None, "127.0.0.1").await;
    (served.base, served.downloads)
}

/// What a stand-in saw.
struct Served {
    base: String,
    downloads: Arc<Mutex<usize>>,
    /// The `Authorization` header of each request, as `"<route>:<header>"`.
    auth: Arc<Mutex<Vec<String>>>,
}

/// [`stand_in`], with the asset's declared `size` (the real length when
/// `None`) and the host its `browser_download_url` names. The listener is
/// on 127.0.0.1, so `localhost` reaches the same server under another host.
async fn stand_in_with(
    tag: &str,
    digest: Digest,
    bytes: Vec<u8>,
    size: Option<u64>,
    asset_host: &str,
) -> Served {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base = format!("http://{addr}");
    let name = format!("rustykrab-{TARGET}.tar.gz");
    let digest = match digest {
        Digest::Right => serde_json::json!(format!("sha256:{}", sha256_hex(&bytes))),
        Digest::Wrong => serde_json::json!(format!("sha256:{}", "0".repeat(64))),
        Digest::Missing => serde_json::Value::Null,
    };
    let release = serde_json::json!({
        "tag_name": tag,
        "assets": [{
            "name": name,
            "browser_download_url":
                format!("http://{asset_host}:{}/download/{name}", addr.port()),
            "digest": digest,
            "size": size.unwrap_or(bytes.len() as u64),
        }],
    });
    let downloads = Arc::new(Mutex::new(0usize));
    let counter = downloads.clone();
    let auth = Arc::new(Mutex::new(Vec::new()));
    let (auth_release, auth_download) = (auth.clone(), auth.clone());
    let header = |headers: &axum::http::HeaderMap| {
        headers
            .get("authorization")
            .map(|v| v.to_str().unwrap_or("?").to_string())
            .unwrap_or_else(|| "none".to_string())
    };
    let app = Router::new()
        .route(
            "/repos/gcbh/rustykrab/releases/latest",
            get(move |headers: axum::http::HeaderMap| {
                auth_release
                    .lock()
                    .unwrap()
                    .push(format!("release:{}", header(&headers)));
                let release = release.clone();
                async move { axum::Json(release) }
            }),
        )
        .route(
            &format!("/download/{name}"),
            get(move |headers: axum::http::HeaderMap| {
                *counter.lock().unwrap() += 1;
                auth_download
                    .lock()
                    .unwrap()
                    .push(format!("download:{}", header(&headers)));
                let bytes = bytes.clone();
                async move { bytes }
            }),
        );
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Served {
        base,
        downloads,
        auth,
    }
}

fn config(data_dir: &Path, api_base: &str) -> Config {
    Config {
        data_dir: data_dir.to_path_buf(),
        repo: DEFAULT_REPO.to_string(),
        api_base: api_base.to_string(),
        token: None,
        target: TARGET.to_string(),
        running_version: RUNNING.to_string(),
        team_id: DEFAULT_TEAM_ID.to_string(),
    }
}

/// Everything under `updates/`, so a refusal can be shown to leave nothing.
fn updates_entries(data_dir: &Path) -> Vec<String> {
    match std::fs::read_dir(data_dir.join("updates")) {
        Ok(entries) => entries
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect(),
        Err(_) => Vec::new(),
    }
}

#[test]
fn sha256_matches_the_standard_vector() {
    assert_eq!(
        sha256_hex(b"abc"),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
}

#[test]
fn versions_compare_as_numbers() {
    assert!(is_newer("v5.3.10", "5.3.9").unwrap());
    assert!(!is_newer("v5.3.6", "5.3.6").unwrap());
    assert!(!is_newer("5.2.99", "5.3.0").unwrap());
    assert!(is_newer("v5.3", "5.3.0").is_err());
}

#[test]
fn version_output_gives_version_and_commit() {
    assert_eq!(
        parse_version_output("rustykrab 5.4.0 (abc1234-dirty, 2026-09-28)\n"),
        Some(("5.4.0".to_string(), Some("abc1234-dirty".to_string())))
    );
    assert_eq!(
        parse_version_output("rustykrab 5.4.0 (unknown, unknown)"),
        Some(("5.4.0".to_string(), None))
    );
    assert_eq!(parse_version_output("something else"), None);
}

#[test]
fn signing_details_pin_identifier_and_team() {
    let good = "Executable=/x\nIdentifier=com.gcbh.rustykrab\nFormat=app bundle\nTeamIdentifier=3RRX845C4X\n";
    assert!(check_signing_details(good, DEFAULT_TEAM_ID).is_ok());
    let other_team = good.replace("3RRX845C4X", "ABCDE12345");
    assert!(check_signing_details(&other_team, DEFAULT_TEAM_ID).is_err());
    assert!(check_signing_details(&other_team, "ABCDE12345").is_ok());
    let other_id = good.replace("com.gcbh.rustykrab", "com.example.krab");
    assert!(check_signing_details(&other_id, DEFAULT_TEAM_ID).is_err());
    let unsigned = "Identifier=com.gcbh.rustykrab\nTeamIdentifier=not set\n";
    assert!(check_signing_details(unsigned, DEFAULT_TEAM_ID).is_err());
}

#[test]
fn parses_the_two_forms() {
    let args = |s: &str| s.split_whitespace().map(String::from).collect::<Vec<_>>();
    assert_eq!(parse(&args("check")), Ok(Cmd::Check));
    assert_eq!(
        parse(&args("stage --from /tmp/RustyKrab.app --force")),
        Ok(Cmd::Stage {
            from: Some(PathBuf::from("/tmp/RustyKrab.app")),
            force: true
        })
    );
    assert!(parse(&args("stage --from")).is_err());
    assert!(parse(&args("apply --bogus")).is_err());
    assert!(parse(&args("apply --service upstart")).is_err());
    assert!(matches!(parse(&args("apply --yes")), Ok(Cmd::Apply(_))));
}

#[tokio::test]
async fn check_reports_a_newer_release() {
    let (base, _) = stand_in("v5.4.0", Digest::Right, archive()).await;
    let data = tempfile::tempdir().unwrap();
    let latest = check(&config(data.path(), &base)).await.unwrap();
    assert_eq!(latest.version, "5.4.0");
    assert!(latest.newer);
}

#[tokio::test]
async fn a_digest_mismatch_is_refused_and_nothing_is_extracted() {
    let (base, downloads) = stand_in("v5.4.0", Digest::Wrong, archive()).await;
    let data = tempfile::tempdir().unwrap();
    let verifier = Scripted::passing("5.4.0");
    let err = stage_release(&config(data.path(), &base), &verifier, false)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("sha256"), "{err:#}");
    assert_eq!(*downloads.lock().unwrap(), 1);
    assert!(updates_entries(data.path()).is_empty());
    assert!(
        verifier.calls().is_empty(),
        "nothing may run before the digest"
    );
}

#[tokio::test]
async fn a_missing_digest_is_refused() {
    let (base, downloads) = stand_in("v5.4.0", Digest::Missing, archive()).await;
    let data = tempfile::tempdir().unwrap();
    let verifier = Scripted::passing("5.4.0");
    let err = stage_release(&config(data.path(), &base), &verifier, false)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("no sha256 digest"), "{err:#}");
    assert_eq!(*downloads.lock().unwrap(), 0);
    assert!(updates_entries(data.path()).is_empty());
    assert!(verifier.calls().is_empty());
}

#[tokio::test]
async fn a_release_that_is_not_newer_stages_nothing() {
    let (base, downloads) = stand_in("v5.3.6", Digest::Right, archive()).await;
    let data = tempfile::tempdir().unwrap();
    let verifier = Scripted::passing("5.3.6");
    let outcome = stage_release(&config(data.path(), &base), &verifier, false)
        .await
        .unwrap();
    assert!(matches!(outcome, StageOutcome::NotNewer(ref l) if l.version == "5.3.6"));
    assert_eq!(*downloads.lock().unwrap(), 0);
    assert!(updates_entries(data.path()).is_empty());
}

#[tokio::test]
async fn a_good_release_is_staged_with_its_record() {
    let bytes = archive();
    let digest = format!("sha256:{}", sha256_hex(&bytes));
    let (base, _) = stand_in("v5.4.0", Digest::Right, bytes).await;
    let data = tempfile::tempdir().unwrap();
    let verifier = Scripted::passing("5.4.0");
    let StageOutcome::Staged(staged) = stage_release(&config(data.path(), &base), &verifier, false)
        .await
        .unwrap()
    else {
        panic!("a newer release with the right digest stages");
    };

    // Signature first, then the one --version run.
    assert_eq!(
        verifier.calls(),
        vec![format!("sign:{DEFAULT_TEAM_ID}"), "version".to_string()]
    );
    let dir = data.path().join("updates").join("5.4.0");
    assert_eq!(staged.path, dir.join(APP_NAME));
    assert!(dir
        .join(APP_NAME)
        .join("Contents/MacOS")
        .join(BINARY_NAME)
        .is_file());
    assert_eq!(updates_entries(data.path()), vec!["5.4.0".to_string()]);

    let record: Staged =
        serde_json::from_str(&std::fs::read_to_string(dir.join(STAGED_FILE)).unwrap()).unwrap();
    assert_eq!(record, staged);
    assert_eq!(record.version, "5.4.0");
    assert_eq!(record.tag.as_deref(), Some("v5.4.0"));
    assert_eq!(record.commit.as_deref(), Some("abc1234"));
    assert_eq!(record.digest.as_deref(), Some(digest.as_str()));
    assert!(record
        .source
        .ends_with(&format!("rustykrab-{TARGET}.tar.gz")));
}

#[tokio::test]
async fn a_refused_signature_runs_nothing_and_stages_nothing() {
    let (base, _) = stand_in("v5.4.0", Digest::Right, archive()).await;
    let data = tempfile::tempdir().unwrap();
    let verifier = Scripted {
        signature: Err("signed by team ABCDE12345".to_string()),
        ..Scripted::passing("5.4.0")
    };
    assert!(stage_release(&config(data.path(), &base), &verifier, false)
        .await
        .is_err());
    assert_eq!(verifier.calls(), vec![format!("sign:{DEFAULT_TEAM_ID}")]);
    assert!(updates_entries(data.path()).is_empty());
}

#[tokio::test]
async fn a_binary_reporting_another_version_is_refused() {
    let (base, _) = stand_in("v5.4.0", Digest::Right, archive()).await;
    let data = tempfile::tempdir().unwrap();
    let verifier = Scripted::passing("5.3.9");
    let err = stage_release(&config(data.path(), &base), &verifier, false)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("reports 5.3.9"), "{err:#}");
    assert!(updates_entries(data.path()).is_empty());
}

#[tokio::test]
async fn a_bad_version_needs_force() {
    let (base, _) = stand_in("v5.4.0", Digest::Right, archive()).await;
    let data = tempfile::tempdir().unwrap();
    let updates = data.path().join("updates");
    std::fs::create_dir_all(&updates).unwrap();
    std::fs::write(updates.join(BAD_FILE), r#"[{"version": "5.4.0"}]"#).unwrap();
    let cfg = config(data.path(), &base);
    let verifier = Scripted::passing("5.4.0");

    let err = stage_release(&cfg, &verifier, false).await.unwrap_err();
    assert!(err.to_string().contains("recorded as bad"), "{err:#}");
    assert!(!updates.join("5.4.0").exists());

    let outcome = stage_release(&cfg, &verifier, true).await.unwrap();
    assert!(matches!(outcome, StageOutcome::Staged(_)));
    assert!(updates.join("5.4.0").join(STAGED_FILE).is_file());
}

#[test]
fn from_a_local_bundle_checks_the_signature_and_has_no_digest() {
    let src = tempfile::tempdir().unwrap();
    let app = bundle_in(src.path());
    let data = tempfile::tempdir().unwrap();
    let cfg = config(data.path(), "http://127.0.0.1:9");
    let verifier = Scripted::passing("5.3.6");
    let staged = stage_from(&cfg, &verifier, &app, false).unwrap();
    assert_eq!(
        verifier.calls(),
        vec![format!("sign:{DEFAULT_TEAM_ID}"), "version".to_string()]
    );
    assert_eq!(staged.version, "5.3.6");
    assert_eq!(staged.digest, None);
    assert_eq!(staged.tag, None);
    assert_eq!(
        staged.path,
        data.path().join("updates").join("5.3.6").join(APP_NAME)
    );
    assert!(staged
        .path
        .join("Contents/MacOS")
        .join(BINARY_NAME)
        .is_file());
}

#[test]
fn from_a_bare_binary_skips_the_signature() {
    let src = tempfile::tempdir().unwrap();
    let binary = src.path().join("some-build");
    std::fs::write(&binary, "#!/bin/sh\n").unwrap();
    let data = tempfile::tempdir().unwrap();
    let cfg = config(data.path(), "http://127.0.0.1:9");
    let verifier = Scripted::passing("5.4.1");
    let staged = stage_from(&cfg, &verifier, &binary, false).unwrap();
    assert_eq!(verifier.calls(), vec!["version".to_string()]);
    assert_eq!(
        staged.path,
        data.path().join("updates").join("5.4.1").join(BINARY_NAME)
    );
    assert!(data
        .path()
        .join("updates/5.4.1")
        .join(STAGED_FILE)
        .is_file());
}

/// The real `codesign` refuses a bundle nobody signed, and the release is
/// not staged.
#[cfg(target_os = "macos")]
#[tokio::test]
async fn real_codesign_refuses_an_unsigned_bundle() {
    let src = tempfile::tempdir().unwrap();
    let app = bundle_in(src.path());
    assert!(SystemVerifier
        .verify_signature(&app, DEFAULT_TEAM_ID)
        .is_err());

    let (base, _) = stand_in("v5.4.0", Digest::Right, archive()).await;
    let data = tempfile::tempdir().unwrap();
    let err = stage_release(&config(data.path(), &base), &SystemVerifier, false)
        .await
        .unwrap_err();
    assert!(
        format!("{err:#}").contains("signature check refused"),
        "{err:#}"
    );
    assert!(updates_entries(data.path()).is_empty());
}

/// Anyone can make an ad-hoc signature that names the release's identifier
/// and team: `codesign -dv` then prints both. The Developer ID requirement
/// refuses it, since no certificate chains to Apple.
#[cfg(target_os = "macos")]
#[test]
fn real_codesign_refuses_an_ad_hoc_signature_that_claims_the_team() {
    let src = tempfile::tempdir().unwrap();
    let app = bundle_in(src.path());
    std::fs::write(
        app.join("Contents/Info.plist"),
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?><plist version=\"1.0\"><dict>\
             <key>CFBundleIdentifier</key><string>{BUNDLE_ID}</string>\
             <key>CFBundleExecutable</key><string>{BINARY_NAME}</string></dict></plist>"
        ),
    )
    .unwrap();
    let signed = std::process::Command::new("codesign")
        .args(["-s", "-", "--force", "--identifier", BUNDLE_ID])
        .args(["--team-id", DEFAULT_TEAM_ID])
        .arg(&app)
        .output()
        .unwrap();
    assert!(signed.status.success(), "{signed:?}");
    // What the old check read says yes.
    let details = std::process::Command::new("codesign")
        .arg("-dv")
        .arg(&app)
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&details.stderr).into_owned();
    assert!(
        check_signing_details(&text, DEFAULT_TEAM_ID).is_ok(),
        "{text}"
    );
    // The requirement says no.
    let err = SystemVerifier
        .verify_signature(&app, DEFAULT_TEAM_ID)
        .unwrap_err();
    assert!(
        format!("{err:#}").contains("not signed by Developer ID team"),
        "{err:#}"
    );
}

#[test]
fn versions_are_digits_only() {
    assert_eq!(parse_semver("+5.4.0"), None);
    assert_eq!(parse_semver("5.4.0-rc.1"), None);
    assert_eq!(parse_semver("5..0"), None);
    assert!(is_plain_version("5.4.0"));
    for bad in ["..", ".", "v5.4.0", "5.4", "5.4.0/..", "5.4.0 "] {
        assert!(!is_plain_version(bad), "{bad:?}");
    }
}

/// A local build that reports a version like `..` must not name, and so
/// replace, a directory outside `updates/<version>/`.
#[test]
fn a_local_build_reporting_a_path_as_its_version_is_refused_and_the_data_dir_kept() {
    let src = tempfile::tempdir().unwrap();
    let binary = src.path().join("odd-build");
    std::fs::write(&binary, "#!/bin/sh\n").unwrap();
    let data = tempfile::tempdir().unwrap();
    std::fs::write(data.path().join("keep.db"), "precious").unwrap();
    let cfg = config(data.path(), "http://127.0.0.1:9");
    for version in ["..", "."] {
        let verifier = Scripted {
            version_output: format!("rustykrab {version} (abc1234, 2026-09-28)\n"),
            ..Scripted::passing("5.4.1")
        };
        let err = stage_from(&cfg, &verifier, &binary, false).unwrap_err();
        assert!(format!("{err:#}").contains("not X.Y.Z"), "{err:#}");
        assert_eq!(
            std::fs::read_to_string(data.path().join("keep.db")).unwrap(),
            "precious"
        );
    }
    assert!(updates_entries(data.path()).is_empty());
}

#[cfg(unix)]
#[test]
fn a_payload_that_is_a_symbolic_link_is_refused() {
    let elsewhere = tempfile::tempdir().unwrap();
    let real = bundle_in(elsewhere.path());
    let dir = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(&real, dir.path().join(APP_NAME)).unwrap();
    let err = payload(dir.path()).unwrap_err();
    assert!(format!("{err:#}").contains("symbolic link"), "{err:#}");
}

#[cfg(unix)]
#[test]
fn a_version_run_that_hangs_is_killed() {
    let mut cmd = Command::new("sh");
    cmd.args(["-c", "sleep 30"]);
    let started = std::time::Instant::now();
    let err = output_within(cmd, std::time::Duration::from_millis(300)).unwrap_err();
    assert!(err.to_string().contains("killed"), "{err}");
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
}

#[test]
fn the_record_says_what_was_staged_and_whether_its_signature_was_checked() {
    let src = tempfile::tempdir().unwrap();
    let binary = src.path().join("some-build");
    std::fs::write(&binary, "#!/bin/sh\n").unwrap();
    let data = tempfile::tempdir().unwrap();
    let cfg = config(data.path(), "http://127.0.0.1:9");
    let staged = stage_from(&cfg, &Scripted::passing("5.4.1"), &binary, false).unwrap();
    assert_eq!(staged.kind, "binary");
    assert!(!staged.signature_verified);

    let app = bundle_in(src.path());
    let staged = stage_from(&cfg, &Scripted::passing("5.4.2"), &app, false).unwrap();
    assert_eq!(staged.kind, "app");
    assert_eq!(staged.signature_verified, cfg!(target_os = "macos"));
    let record: Staged = serde_json::from_str(
        &std::fs::read_to_string(data.path().join("updates/5.4.2").join(STAGED_FILE)).unwrap(),
    )
    .unwrap();
    assert_eq!(record, staged);
}

#[test]
fn a_stage_killed_midway_leaves_no_scratch_behind_the_next() {
    let data = tempfile::tempdir().unwrap();
    let updates = data.path().join("updates");
    std::fs::create_dir_all(updates.join(".staging-left-by-a-kill/x")).unwrap();
    let scratch = Scratch::new(&updates).unwrap();
    assert!(!updates.join(".staging-left-by-a-kill").exists());
    drop(scratch);
}

#[tokio::test]
async fn an_asset_declared_over_the_cap_is_refused_without_downloading() {
    let served = stand_in_with(
        "v5.4.0",
        Digest::Right,
        archive(),
        Some(MAX_ASSET_BYTES + 1),
        "127.0.0.1",
    )
    .await;
    let data = tempfile::tempdir().unwrap();
    let verifier = Scripted::passing("5.4.0");
    let err = stage_release(&config(data.path(), &served.base), &verifier, false)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("cap"), "{err:#}");
    assert_eq!(*served.downloads.lock().unwrap(), 0);
    assert!(updates_entries(data.path()).is_empty());
    assert!(verifier.calls().is_empty());
}

#[tokio::test]
async fn a_body_longer_than_its_declared_size_is_refused() {
    let bytes = archive();
    let short = bytes.len() as u64 - 1;
    let served = stand_in_with("v5.4.0", Digest::Right, bytes, Some(short), "127.0.0.1").await;
    let data = tempfile::tempdir().unwrap();
    let verifier = Scripted::passing("5.4.0");
    let err = stage_release(&config(data.path(), &served.base), &verifier, false)
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("longer than its declared"),
        "{err:#}"
    );
    assert!(updates_entries(data.path()).is_empty());
    assert!(verifier.calls().is_empty());
}

#[tokio::test]
async fn a_body_shorter_than_its_declared_size_is_refused() {
    let bytes = archive();
    let long = bytes.len() as u64 + 1;
    let served = stand_in_with("v5.4.0", Digest::Right, bytes, Some(long), "127.0.0.1").await;
    let data = tempfile::tempdir().unwrap();
    let err = stage_release(
        &config(data.path(), &served.base),
        &Scripted::passing("5.4.0"),
        false,
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("the release declares"), "{err:#}");
    assert!(updates_entries(data.path()).is_empty());
}

#[tokio::test]
async fn the_token_goes_to_the_api_host_only() {
    // The asset names `localhost`, another host than the API's 127.0.0.1.
    let served = stand_in_with("v5.4.0", Digest::Right, archive(), None, "localhost").await;
    let data = tempfile::tempdir().unwrap();
    let cfg = Config {
        token: Some("ghp_secret".to_string()),
        ..config(data.path(), &served.base)
    };
    let outcome = stage_release(&cfg, &Scripted::passing("5.4.0"), false)
        .await
        .unwrap();
    assert!(matches!(outcome, StageOutcome::Staged(_)));
    assert_eq!(
        *served.auth.lock().unwrap(),
        vec![
            "release:Bearer ghp_secret".to_string(),
            "download:none".to_string()
        ]
    );

    // On the API's own host it goes with the download too.
    let served = stand_in_with("v5.4.0", Digest::Right, archive(), None, "127.0.0.1").await;
    let data = tempfile::tempdir().unwrap();
    let cfg = Config {
        token: Some("ghp_secret".to_string()),
        ..config(data.path(), &served.base)
    };
    stage_release(&cfg, &Scripted::passing("5.4.0"), false)
        .await
        .unwrap();
    assert_eq!(
        *served.auth.lock().unwrap(),
        vec![
            "release:Bearer ghp_secret".to_string(),
            "download:Bearer ghp_secret".to_string()
        ]
    );
}

/// Where [`redirected_download_auth`] sends the asset URL's redirect.
enum RedirectTo {
    /// `localhost` on the API's port: another host.
    OtherHost,
    /// `127.0.0.1` on a second listener: the same host, another port.
    OtherPort,
}

/// Stage a release whose asset URL names the API's own host and port, so
/// the token goes with it, and which redirects as `to` says. Returns the
/// `route:authorization` each request arrived with.
async fn redirected_download_auth(to: RedirectTo) -> Vec<String> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base = format!("http://{addr}");
    let bytes = archive();
    let name = format!("rustykrab-{TARGET}.tar.gz");
    let release = serde_json::json!({
        "tag_name": "v5.4.0",
        "assets": [{
            "name": name,
            "browser_download_url": format!("{base}/redirect/{name}"),
            "digest": format!("sha256:{}", sha256_hex(&bytes)),
            "size": bytes.len() as u64,
        }],
    });
    let other = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let elsewhere = match to {
        // `localhost` reaches the same listener under another host.
        RedirectTo::OtherHost => format!("http://localhost:{}/download/{name}", addr.port()),
        RedirectTo::OtherPort => format!("http://{}/download/{name}", other.local_addr().unwrap()),
    };
    let auth = Arc::new(Mutex::new(Vec::<String>::new()));
    let seen = |auth: &Arc<Mutex<Vec<String>>>, route: &str, headers: &axum::http::HeaderMap| {
        let header = headers
            .get("authorization")
            .map(|v| v.to_str().unwrap_or("?").to_string())
            .unwrap_or_else(|| "none".to_string());
        auth.lock().unwrap().push(format!("{route}:{header}"));
    };
    let (a1, a2, a3) = (auth.clone(), auth.clone(), auth.clone());
    let app = Router::new()
        .route(
            "/repos/gcbh/rustykrab/releases/latest",
            get(move |headers: axum::http::HeaderMap| {
                seen(&a1, "release", &headers);
                let release = release.clone();
                async move { axum::Json(release) }
            }),
        )
        .route(
            &format!("/redirect/{name}"),
            get(move |headers: axum::http::HeaderMap| {
                seen(&a2, "redirect", &headers);
                let to = elsewhere.clone();
                async move { axum::response::Redirect::temporary(&to) }
            }),
        )
        .route(
            &format!("/download/{name}"),
            get(move |headers: axum::http::HeaderMap| {
                seen(&a3, "download", &headers);
                let bytes = bytes.clone();
                async move { bytes }
            }),
        );
    let other_app = app.clone();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    tokio::spawn(async move { axum::serve(other, other_app).await.unwrap() });

    let data = tempfile::tempdir().unwrap();
    let cfg = Config {
        token: Some("ghp_secret".to_string()),
        ..config(data.path(), &base)
    };
    let outcome = stage_release(&cfg, &Scripted::passing("5.4.0"), false)
        .await
        .unwrap();
    assert!(matches!(outcome, StageOutcome::Staged(_)));
    let seen = auth.lock().unwrap().clone();
    seen
}

/// The asset URL's redirect goes to another host: the redirected request
/// carries no token.
#[tokio::test]
async fn a_redirect_to_another_host_drops_the_token() {
    assert_eq!(
        redirected_download_auth(RedirectTo::OtherHost).await,
        vec![
            "release:Bearer ghp_secret".to_string(),
            "redirect:Bearer ghp_secret".to_string(),
            "download:none".to_string(),
        ]
    );
}

/// The asset URL's redirect stays on the API's host but goes to another
/// port, which the host-and-port rule of [`asset_request_sends_token`]
/// treats as elsewhere: the redirected request carries no token.
#[tokio::test]
async fn a_redirect_to_another_port_on_the_same_host_drops_the_token() {
    assert_eq!(
        redirected_download_auth(RedirectTo::OtherPort).await,
        vec![
            "release:Bearer ghp_secret".to_string(),
            "redirect:Bearer ghp_secret".to_string(),
            "download:none".to_string(),
        ]
    );
}

#[test]
fn an_asset_url_must_be_https_unless_the_api_is_a_local_http_stand_in() {
    let api = "https://api.github.com";
    assert!(!asset_request_sends_token(api, "https://github.com/x/y.tar.gz").unwrap());
    assert!(asset_request_sends_token(api, "https://api.github.com/x/y.tar.gz").unwrap());
    assert!(!asset_request_sends_token(api, "https://api.github.com:8443/x").unwrap());
    let err = asset_request_sends_token(api, "http://api.github.com/x").unwrap_err();
    assert!(err.to_string().contains("not https"), "{err:#}");
    assert!(asset_request_sends_token(api, "file:///etc/passwd").is_err());
    let local = "http://127.0.0.1:4000";
    assert!(asset_request_sends_token(local, "http://127.0.0.1:4000/x").unwrap());
    assert!(!asset_request_sends_token(local, "http://127.0.0.1:4001/x").unwrap());
    assert!(!asset_request_sends_token(local, "http://localhost:4000/x").unwrap());
}

#[tokio::test]
async fn a_pre_release_tag_stages_nothing_and_check_says_so() {
    let (base, downloads) = stand_in("v5.4.0-rc.1", Digest::Right, archive()).await;
    let data = tempfile::tempdir().unwrap();
    let cfg = config(data.path(), &base);

    let latest = check(&cfg).await.unwrap();
    assert!(!latest.plain);
    assert!(!latest.newer);
    let said = latest.describe(&cfg);
    assert!(said.contains("nothing newer"), "{said}");
    assert!(said.contains("v5.4.0-rc.1"), "{said}");
    assert!(said.contains("not a plain vX.Y.Z"), "{said}");

    let verifier = Scripted::passing("5.4.0");
    let outcome = stage_release(&cfg, &verifier, false).await.unwrap();
    assert!(matches!(outcome, StageOutcome::NotNewer(ref l) if !l.plain));
    assert_eq!(*downloads.lock().unwrap(), 0);
    assert!(updates_entries(data.path()).is_empty());
    assert!(verifier.calls().is_empty());
}

#[test]
fn the_script_service_runs_without_the_gate_variable_and_launchd_does_not() {
    let script = apply::ServiceSpec::Script("./start.sh".to_string());
    assert_eq!(apply_gate(&script, false), Ok(()));
    assert_eq!(apply_gate(&script, true), Ok(()));

    let err = apply_gate(&apply::ServiceSpec::Launchd, false).unwrap_err();
    for needed in [
        "without authentication",
        "com.gcbh.rustykrab.updater",
        "isolation",
    ] {
        assert!(err.contains(needed), "{err}");
    }
    assert_eq!(apply_gate(&apply::ServiceSpec::Launchd, true), Ok(()));
}

#[test]
fn config_debug_redacts_the_token() {
    let cfg = Config {
        token: Some("ghp_secret".to_string()),
        ..config(Path::new("/tmp/data"), DEFAULT_API_BASE)
    };
    let shown = format!("{cfg:?}");
    assert!(!shown.contains("ghp_secret"), "{shown}");
    assert!(shown.contains("<redacted>"), "{shown}");
    assert!(shown.contains(DEFAULT_REPO), "{shown}");
}
