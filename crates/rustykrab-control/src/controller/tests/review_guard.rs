//! A read-only review job (dreaming's project review: a `research` item
//! with `read_only_review=true`) reports a summary, known limits or an
//! error. A report that carries anything else, whichever worker wrote it,
//! fails `policy/scope` observed by `read_only_guard`, and none of what it
//! carried is stored or acted on.

use rustykrab_core::work::{BlockedReason, BlockedReport, Question, WorkKind};
use rustykrab_store::{QuestionFilter, QuestionRow};

use super::*;

const REVIEW: &str = "Review the project";
const FORGED_RULE: &str = "class=tool subclass=timeout pattern=gauge";

/// A review job as `cli/src/dreaming.rs::queue` files it.
fn review_job() -> WorkItemDraft {
    WorkItemDraft {
        kind: Some(WorkKind::Research),
        artifact_refs: vec![
            ArtifactRef {
                kind: rustykrab_core::dream_review::REVIEW_ONLY.into(),
                value: "true".into(),
            },
            ArtifactRef {
                kind: "dream_review_job".into(),
                value: "job-1:generator".into(),
            },
        ],
        priority: -100,
        budget: Some(no_rungs()),
        ..draft("r", REVIEW)
    }
}

fn summary() -> ResultReport {
    ResultReport {
        summary: r#"{"ideas":[]}"#.into(),
        ..ResultReport::default()
    }
}

async fn questions_of(h: &Harness, item: &str) -> Vec<QuestionRow> {
    h.store()
        .questions_list(&QuestionFilter {
            item: Some(item.to_string()),
            ..QuestionFilter::default()
        })
        .await
        .unwrap()
}

/// Run one review job whose worker reports `result`.
async fn judged(result: ResultReport) -> (Harness, WorkItemId) {
    let h = Harness::new(&["pinch"]);
    h.script.push(REVIEW, report(result));
    let id = h.file_one(review_job()).await;
    h.drain().await;
    (h, id)
}

/// The review failed at the guard, naming `fields`, and nothing it carried
/// was stored: no evidence beyond the controller's own, no other item.
async fn stopped(h: &Harness, id: &str, fields: &[&str]) {
    assert_ne!(h.status(id).await, Status::Done);
    let evidence = h.store().work_evidence_list(id).await.unwrap();
    let guard: Vec<_> = evidence
        .iter()
        .filter(|e| e.kind == "error" && e.verified_by.as_deref() == Some("read_only_guard"))
        .collect();
    assert_eq!(guard.len(), 1, "{evidence:#?}");
    assert!(guard[0].reference.starts_with("policy/scope"), "{guard:?}");
    for field in fields {
        assert!(guard[0].reference.contains(field), "{field}: {guard:?}");
    }
    let reported: Vec<_> = evidence
        .iter()
        .filter(|e| {
            [
                "path",
                "classifier_rule",
                "commit",
                "changed_path",
                "check_run",
                "summary",
                "known_limit",
            ]
            .contains(&e.kind.as_str())
        })
        .collect();
    assert!(
        reported.is_empty(),
        "nothing reported is stored: {reported:#?}"
    );
    let items = h
        .store()
        .work_list(&rustykrab_store::WorkFilter {
            include_closed: true,
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(items.len(), 1, "nothing was filed: {items:#?}");
}

#[tokio::test]
async fn a_review_that_discovers_work_files_none_of_it() {
    let (h, id) = judged(ResultReport {
        discovered: vec![draft("follow", "Rewrite the scheduler")],
        ..summary()
    })
    .await;
    stopped(&h, &id, &["discovered"]).await;
}

#[tokio::test]
async fn a_review_that_raises_questions_asks_none_of_them() {
    let (h, id) = judged(ResultReport {
        questions: vec![Question {
            text: "May I push to main?".into(),
            class: String::new(),
            options: vec![],
        }],
        ..summary()
    })
    .await;
    stopped(&h, &id, &["questions"]).await;
    let asked = questions_of(&h, &id).await;
    assert!(
        asked.iter().all(|q| !q.text.contains("push to main")),
        "the review's question was not asked: {asked:#?}"
    );
}

#[tokio::test]
async fn a_review_that_claims_artifacts_lands_none_of_them() {
    let (h, id) = judged(ResultReport {
        artifacts: vec![
            ArtifactRef {
                kind: "path".into(),
                value: "/tmp/review.txt".into(),
            },
            ArtifactRef {
                kind: "classifier_rule".into(),
                value: FORGED_RULE.into(),
            },
        ],
        ..summary()
    })
    .await;
    stopped(&h, &id, &["artifacts"]).await;
    assert!(
        h.store()
            .work_evidence_of_kind("classifier_rule")
            .await
            .unwrap()
            .is_empty(),
        "no rule landed"
    );
}

#[tokio::test]
async fn a_review_that_claims_a_commit_paths_or_checks_is_stopped() {
    let (h, id) = judged(ResultReport {
        commit: Some("0123456789abcdef0123456789abcdef01234567".into()),
        changed_paths: vec!["src/lib.rs".into()],
        checks_run: vec!["cargo test".into()],
        ..summary()
    })
    .await;
    stopped(&h, &id, &["commit", "changed_paths", "checks_run"]).await;
}

#[tokio::test]
async fn a_review_that_reports_a_block_is_stopped_not_parked_on_it() {
    let (h, id) = judged(ResultReport {
        blocked: Some(BlockedReport {
            reason: BlockedReason::NeedsTool,
            detail: "needs tool: work_resources".into(),
            needs: vec!["work_resources".into()],
        }),
        ..summary()
    })
    .await;
    stopped(&h, &id, &["blocked"]).await;
    assert!(
        h.of_kind(WorkKind::Capability).await.is_empty(),
        "no capability item for a review's block"
    );
    assert_ne!(
        h.status(&id).await,
        Status::Blocked(BlockedReason::NeedsTool)
    );
}

#[tokio::test]
async fn a_review_with_a_summary_and_known_limits_completes() {
    let (h, id) = judged(ResultReport {
        known_limits: vec!["two observations were omitted".into()],
        ..summary()
    })
    .await;
    assert_eq!(h.status(&id).await, Status::Done);
    let evidence = h.store().work_evidence_list(&id).await.unwrap();
    assert!(evidence
        .iter()
        .any(|e| e.kind == "summary" && e.reference == r#"{"ideas":[]}"#));
    assert!(evidence.iter().any(|e| e.kind == "known_limit"));
    assert!(!evidence
        .iter()
        .any(|e| e.verified_by.as_deref() == Some("read_only_guard")));

    // A review's own error goes the ordinary way, not through the guard.
    let (h, id) = judged(failure(
        rustykrab_core::work::ErrorSubclass::InvalidArgs,
        "the frozen input was empty",
    ))
    .await;
    assert_ne!(h.status(&id).await, Status::Done);
    let evidence = h.store().work_evidence_list(&id).await.unwrap();
    assert!(!evidence
        .iter()
        .any(|e| e.verified_by.as_deref() == Some("read_only_guard")));
}

#[tokio::test]
async fn an_item_that_is_not_a_review_is_unaffected() {
    let h = Harness::new(&["pinch"]);
    let title = "Research the tide tables";
    h.script.push(
        title,
        report(ResultReport {
            discovered: vec![draft("follow", "Compare the tide tables")],
            known_limits: vec!["one station".into()],
            ..done(title)
        }),
    );
    let mut d = draft("r", title);
    d.kind = Some(WorkKind::Research);
    let id = h.file_one(d).await;
    h.drain().await;
    assert_eq!(h.status(&id).await, Status::Done);
    let evidence = h.store().work_evidence_list(&id).await.unwrap();
    assert!(evidence.iter().any(|e| e.kind == "path"));
    let items = h
        .store()
        .work_list(&rustykrab_store::WorkFilter {
            include_closed: true,
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(
        items.iter().any(|i| i.title == "Compare the tide tables"),
        "its discovered work was filed"
    );
}
