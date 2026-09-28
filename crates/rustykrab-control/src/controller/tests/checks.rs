//! A report's `checks_run` against the commands its adapter recorded the
//! agent running (`command_run` artifacts), judged through the controller.

use rustykrab_core::work::{ArtifactRef, ErrorSubclass, Evidence, ResultReport, Status};

use super::*;
use crate::worker::COMMAND_RUN;

/// A done report naming `checks` as run, with `commands` as the adapter's
/// record of what ran.
fn checked(commands: &[&str], checks: &[&str]) -> Step {
    report(ResultReport {
        summary: "Ran the checks.".to_string(),
        artifacts: commands
            .iter()
            .map(|c| ArtifactRef {
                kind: COMMAND_RUN.to_string(),
                value: c.to_string(),
            })
            .collect(),
        checks_run: checks.iter().map(|c| c.to_string()).collect(),
        ..ResultReport::default()
    })
}

async fn check_evidence(h: &Harness, id: &str) -> Vec<Evidence> {
    h.store()
        .work_evidence_list(id)
        .await
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == "check_run")
        .collect()
}

#[tokio::test]
async fn checks_run_inside_a_compound_command_are_verified_by_the_command_record() {
    let h = Harness::new(&["pinch"]);
    h.script.push(
        "Tidy the crate",
        checked(
            &["bash -lc 'cd /repo && cargo fmt --all -- --check && cargo test -p rustykrab-control'"],
            &[
                "cargo fmt --all -- --check",
                "cargo test -p rustykrab-control (passed)",
            ],
        ),
    );
    let id = h.file_one(draft("x", "Tidy the crate")).await;
    h.drain().await;

    assert_eq!(h.status(&id).await, Status::Done);
    let checks = check_evidence(&h, &id).await;
    let verified: Vec<&str> = checks
        .iter()
        .filter(|e| e.verified_by.as_deref() == Some(COMMAND_RUN))
        .map(|e| e.reference.as_str())
        .collect();
    assert_eq!(
        verified,
        [
            "cargo fmt --all -- --check",
            "cargo test -p rustykrab-control (passed)"
        ],
        "{checks:?}"
    );
    assert_eq!(checks.len(), 2, "nothing unverified besides: {checks:?}");
}

#[tokio::test]
async fn a_check_that_never_ran_fails_the_report_as_a_claim_mismatch() {
    let h = Harness::new(&["pinch"]);
    h.script.push(
        "Lint the crate",
        checked(
            &["cargo fmt --all -- --check"],
            &["cargo fmt --all -- --check", "cargo clippy --workspace"],
        ),
    );
    let id = h.file_one(draft("x", "Lint the crate")).await;
    h.step().await;
    h.step().await;

    assert_ne!(h.status(&id).await, Status::Done);
    let error = h
        .events(&id)
        .await
        .iter()
        .filter_map(decode_rung)
        .find_map(|r| r.error)
        .expect("the refused report is on the ladder with its error");
    assert_eq!(error.subclass, ErrorSubclass::ClaimMismatch);
    assert!(
        error.detail.contains("cargo clippy --workspace"),
        "{}",
        error.detail
    );
    assert!(
        check_evidence(&h, &id)
            .await
            .iter()
            .all(|e| e.verified_by.is_none()),
        "no check of the refused report is verified"
    );

    // The repair reports honestly (no checks claimed) and is verified.
    h.drain().await;
    assert_eq!(h.status(&id).await, Status::Done);
    let briefs = h.script.briefs_for("Lint the crate");
    assert_eq!(briefs.len(), 2);
    assert!(briefs[1].last_error.is_some());
}
