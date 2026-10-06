//! Phase 6 in the controller: proposals wait for review, the scope limit
//! refuses a protected subject below the highest tier on any path, facets
//! land with the item, and review decisions apply as one transaction
//! (plan sections 10 and 11, scenarios 7 and 12).

use rustykrab_core::proposal::ReviewDecision;
use rustykrab_core::work::{
    BlockedReason, CancelReason, CapabilityMode, EdgeKind, EventKind, PlanOutcome, RejectionReason,
    ReviewTier, Status, WorkItemDraft, WorkKind,
};
use rustykrab_tools::work_backend::Provenance;

use super::*;

fn rest() -> Provenance {
    Provenance {
        conversation_id: None,
        filed_by_item: None,
        actor: "user:master".to_string(),
    }
}

fn proposal(title: &str, subject: Option<&str>, tier: Option<ReviewTier>) -> WorkItemDraft {
    WorkItemDraft {
        kind: Some(WorkKind::Proposal),
        title: title.to_string(),
        objective: format!("Change what {title} says"),
        done_when: "the metric moves on replay".to_string(),
        constraints: vec!["Roll back if the metric drops".to_string()],
        subject: subject.map(str::to_string),
        review_tier: tier,
        ..WorkItemDraft::default()
    }
}

async fn file_rest(h: &Harness, draft: WorkItemDraft) -> PlanOutcome {
    ControlHandle::file_draft(&h.ctl, draft, rest())
        .await
        .expect("filing")
}

fn accepted_root(outcome: PlanOutcome) -> String {
    match outcome {
        PlanOutcome::Accepted(a) => a.root,
        PlanOutcome::Rejected(r) => panic!("rejected: {r:?}"),
    }
}

#[tokio::test]
async fn a_protected_proposal_is_refused_below_the_highest_tier() {
    let h = Harness::new(&["pinch"]);
    for subject in ["controller", "policy", "measurement", "credentials:gmail"] {
        let refused = file_rest(
            &h,
            proposal("Change it", Some(subject), Some(ReviewTier::Standard)),
        )
        .await;
        let PlanOutcome::Rejected(r) = refused else {
            panic!("{subject} was accepted below the highest tier");
        };
        assert!(
            r.failed
                .iter()
                .any(|f| f.reason == RejectionReason::OutOfScope),
            "{subject}: {r:?}"
        );
        let id = accepted_root(
            file_rest(
                &h,
                proposal("Change it", Some(subject), Some(ReviewTier::Highest)),
            )
            .await,
        );
        let facets = h.store().work_facets_get(&id).await.unwrap().unwrap();
        assert_eq!(facets.subject.as_deref(), Some(subject));
        assert_eq!(facets.review_tier, Some(ReviewTier::Highest));
    }
    // An unprotected subject needs no particular tier, and one that names
    // no tier is filed at the tier its subject requires.
    let plain =
        accepted_root(file_rest(&h, proposal("Tune a skill", Some("skill:cal"), None)).await);
    assert_eq!(
        h.store()
            .work_facets_get(&plain)
            .await
            .unwrap()
            .unwrap()
            .review_tier,
        Some(ReviewTier::Standard)
    );
    let bare = accepted_root(file_rest(&h, proposal("Watch it", Some("controller"), None)).await);
    assert_eq!(
        h.store()
            .work_facets_get(&bare)
            .await
            .unwrap()
            .unwrap()
            .review_tier,
        Some(ReviewTier::Highest)
    );
}

#[tokio::test]
async fn a_proposal_waits_for_review_and_is_never_leased() {
    let h = Harness::new(&["pinch"]);
    let id = accepted_root(file_rest(&h, proposal("Pre-load caldav", None, None)).await);
    h.drain().await;
    assert_eq!(
        h.status(&id).await,
        Status::Blocked(BlockedReason::NeedsConsent)
    );
    assert_eq!(h.leases(&id).await, 0);
    assert!(h.script.briefs_for("Pre-load caldav").is_empty());
    // The user is told it was filed.
    assert!(h
        .outbox()
        .await
        .iter()
        .any(|n| n.body.contains("Pre-load caldav") && n.body.contains("review")));
}

#[tokio::test]
async fn acceptance_files_a_code_item_that_names_the_proposal() {
    let h = Harness::new(&["pinch"]);
    let id =
        accepted_root(file_rest(&h, proposal("Pre-load caldav", Some("tool:caldav"), None)).await);
    let amended = ControlHandle::review_decision(
        &h.ctl,
        &id,
        ReviewDecision::Amend {
            text: "only for calendar errands".to_string(),
        },
        "reviewer:github:ada",
    )
    .await
    .unwrap();
    assert_eq!(amended.status, Status::Blocked(BlockedReason::NeedsConsent));
    let accepted =
        ControlHandle::review_decision(&h.ctl, &id, ReviewDecision::Accept, "reviewer:github:ada")
            .await
            .unwrap();
    assert_eq!(accepted.status, Status::Done);
    let code_id = accepted.code_item.expect("a code item");
    let code = h.item(&code_id).await;
    assert_eq!(code.kind, WorkKind::Code);
    assert!(code.objective.contains("Change what Pre-load caldav says"));
    assert!(code
        .constraints
        .iter()
        .any(|c| c.contains("only for calendar errands")));
    assert!(code.artifact_refs.iter().any(|r| r.value == id));
    let edges = h.store().work_edges_of(&code_id).await.unwrap();
    assert!(edges
        .iter()
        .any(|e| e.kind == EdgeKind::DiscoveredFrom && e.depends_on == id));
    assert_eq!(h.status(&id).await, Status::Done);
    let reviews: Vec<_> = h
        .events(&id)
        .await
        .into_iter()
        .filter(|e| e.kind == EventKind::Review)
        .collect();
    assert_eq!(reviews.len(), 2, "{reviews:?}");
    assert!(reviews.iter().all(|e| e.actor == "reviewer:github:ada"));
    // A second decision on a closed proposal changes nothing.
    let again = ControlHandle::review_decision(
        &h.ctl,
        &id,
        ReviewDecision::Decline { reason: None },
        "reviewer:github:bob",
    )
    .await
    .unwrap();
    assert_eq!(again.status, Status::Done);
    assert!(again.note.is_some());
    assert_eq!(h.of_kind(WorkKind::Code).await.len(), 1);
}

#[tokio::test]
async fn a_decline_cancels_the_proposal_and_files_nothing() {
    let h = Harness::new(&["pinch"]);
    let id = accepted_root(file_rest(&h, proposal("Pre-load caldav", None, None)).await);
    let out = ControlHandle::review_decision(
        &h.ctl,
        &id,
        ReviewDecision::Decline {
            reason: Some("not worth it".to_string()),
        },
        "reviewer:github:ada",
    )
    .await
    .unwrap();
    assert_eq!(out.status, Status::Cancelled(CancelReason::Requested));
    assert_eq!(
        h.status(&id).await,
        Status::Cancelled(CancelReason::Requested)
    );
    assert!(h.of_kind(WorkKind::Code).await.is_empty());
}

#[tokio::test]
async fn only_a_proposal_takes_a_review_decision() {
    let h = Harness::new(&["pinch"]);
    let errand = h.file_one(draft("e", "Book the dentist")).await;
    assert!(
        ControlHandle::review_decision(&h.ctl, &errand, ReviewDecision::Accept, "user")
            .await
            .is_err()
    );
    assert!(
        ControlHandle::review_decision(&h.ctl, "ghost", ReviewDecision::Accept, "user")
            .await
            .is_err()
    );
}

#[tokio::test]
async fn a_capability_items_mode_lands_with_it() {
    let h = Harness::new(&[]);
    let mut build = draft("b", "Build a tide table tool");
    build.kind = Some(WorkKind::Capability);
    build.capability = Some(CapabilityMode::Build);
    let id = h.file_one(build).await;
    assert_eq!(
        h.store()
            .work_facets_get(&id)
            .await
            .unwrap()
            .and_then(|f| f.capability),
        Some(CapabilityMode::Build)
    );
    // A personal item carries no facets whatever its draft says.
    let mut errand = draft("p", "Book the dentist");
    errand.subject = Some("controller".to_string());
    let plain = h.file_one(errand).await;
    assert_eq!(h.store().work_facets_get(&plain).await.unwrap(), None);
}

// ── the review surface, end to end over a fake surface ─────────────────

use std::collections::BTreeMap;

use crate::review::{self, Comment, Issue, Projection, ReviewSurface};

/// A GitHub-shaped surface in memory: numbered issues, labels a person
/// can add, a hand-editable title, and every call counted.
#[derive(Default)]
struct FakeSurface {
    issues: Mutex<BTreeMap<u64, Issue>>,
    comments: Mutex<HashMap<String, Vec<Comment>>>,
    calls: Mutex<Vec<String>>,
}

impl FakeSurface {
    fn all(&self) -> Vec<Issue> {
        self.issues.lock().unwrap().values().cloned().collect()
    }

    fn about(&self, needle: &str) -> Vec<Issue> {
        self.all()
            .into_iter()
            .filter(|i| i.title.contains(needle) || i.body.contains(needle))
            .collect()
    }

    fn edit(&self, number: &str, f: impl FnOnce(&mut Issue)) {
        let n: u64 = number.parse().unwrap();
        f(self.issues.lock().unwrap().get_mut(&n).unwrap());
    }
}

#[async_trait]
impl ReviewSurface for FakeSurface {
    fn name(&self) -> &str {
        "github"
    }

    async fn issues(&self) -> Result<Vec<Issue>, Error> {
        self.calls.lock().unwrap().push("list".into());
        Ok(self.all())
    }

    async fn issue(&self, number: &str) -> Result<Option<Issue>, Error> {
        let n: u64 = number.parse().unwrap_or_default();
        Ok(self.issues.lock().unwrap().get(&n).cloned())
    }

    async fn create(&self, p: &Projection) -> Result<Issue, Error> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("create {}", p.title));
        let mut issues = self.issues.lock().unwrap();
        let n = issues.len() as u64 + 1;
        let issue = Issue {
            number: n.to_string(),
            url: Some(format!("https://github.com/o/r/issues/{n}")),
            title: p.title.clone(),
            body: p.body.clone(),
            labels: p.labels.clone(),
            open: p.open,
        };
        issues.insert(n, issue.clone());
        Ok(issue)
    }

    async fn update(&self, number: &str, p: &Projection) -> Result<Issue, Error> {
        self.calls.lock().unwrap().push(format!("update {number}"));
        let n: u64 = number.parse().unwrap();
        let mut issues = self.issues.lock().unwrap();
        let issue = issues.get_mut(&n).unwrap();
        issue.title = p.title.clone();
        issue.body = p.body.clone();
        issue.open = p.open;
        for l in &p.labels {
            if !issue.labels.contains(l) {
                issue.labels.push(l.clone());
            }
        }
        Ok(issue.clone())
    }

    async fn comments(&self, number: &str) -> Result<Vec<Comment>, Error> {
        Ok(self
            .comments
            .lock()
            .unwrap()
            .get(number)
            .cloned()
            .unwrap_or_default())
    }
}

async fn sync(h: &Harness, surface: &FakeSurface) -> review::Synced {
    review::sync(h.store(), surface, &h.ctl, Utc::now())
        .await
        .expect("sync")
}

/// Scenario 27 against the controller: personal and research work never
/// reaches the surface, a proposal and a capability build do, an
/// acquisition does not, a local-only input is opaque, and a hand edit
/// is overwritten.
#[tokio::test]
async fn the_projection_rule_by_kind_holds_end_to_end() {
    let h = Harness::new(&["pinch"]);
    let dentist = h.file_one(draft("d", "Book the dentist")).await;
    let mut research = draft("r", "Compare dentists nearby");
    research.kind = Some(WorkKind::Research);
    let research = h.file_one(research).await;
    h.drain().await;
    assert_eq!(h.status(&dentist).await, Status::Done);
    let mut p = proposal("Pre-load caldav for calendar errands", None, None);
    p.edges = vec![rustykrab_core::work::DraftEdge {
        kind: EdgeKind::WaitsFor,
        depends_on: rustykrab_core::work::ItemRef::Id(dentist.clone()),
    }];
    p.inputs_from = vec![rustykrab_core::work::ItemRef::Id(dentist.clone())];
    let proposal_id = accepted_root(file_rest(&h, p).await);
    let mut build = draft("b", "Build a tide table tool");
    build.kind = Some(WorkKind::Capability);
    build.capability = Some(CapabilityMode::Build);
    h.file_one(build).await;
    let mut acquire = draft("a", "Acquire the dentist portal password");
    acquire.kind = Some(WorkKind::Capability);
    acquire.capability = Some(CapabilityMode::Acquire);
    acquire.trigger = rustykrab_core::work::Trigger::OnCredential("dentist_portal".into());
    h.file_one(acquire).await;

    let surface = FakeSurface::default();
    let first = sync(&h, &surface).await;
    assert!(first.report.errors.is_empty(), "{:?}", first.report.errors);
    assert!(surface.about("Book the dentist").is_empty());
    assert!(surface.about("Compare dentists").is_empty());
    assert!(surface.about("Acquire the dentist").is_empty());
    assert!(surface.about(&research).is_empty());
    // The proposal waits on a personal item, so it is projected with its
    // own text withheld: it may quote what it was filed from.
    assert!(surface.about("Pre-load caldav").is_empty());
    let issue = surface
        .about(&proposal_id)
        .pop()
        .expect("the proposal is projected");
    assert!(issue.labels.contains(&"rustykrab-proposal".to_string()));
    assert!(issue.labels.contains(&"rustykrab-redacted".to_string()));
    assert!(!issue.title.contains("caldav"), "{}", issue.title);
    assert!(
        issue.body.contains(&review::local_ref(&dentist)),
        "{}",
        issue.body
    );
    assert!(!surface.about("Build a tide table tool").is_empty());

    // Nothing changed: the next pass writes nothing.
    let quiet = sync(&h, &surface).await;
    assert!(quiet.report.created.is_empty() && quiet.report.updated.is_empty());

    // A hand edit to a projected field is overwritten.
    surface.edit(&issue.number, |i| i.title = "edited by hand".into());
    let again = sync(&h, &surface).await;
    assert_eq!(again.report.updated, vec![proposal_id.clone()]);
    assert_eq!(
        surface.issue(&issue.number).await.unwrap().unwrap().title,
        issue.title,
        "the projected title comes back"
    );
}

/// Scenario 7's back half: an acceptance label on the issue becomes a
/// `code` item naming the proposal, the proposal's issue closes, the code
/// item gets its own issue, and the decision is applied once.
#[tokio::test]
async fn an_acceptance_on_the_issue_becomes_a_code_item_once() {
    let h = Harness::new(&[]);
    let id = accepted_root(
        file_rest(
            &h,
            proposal("Tune the calendar skill", Some("skill:cal"), None),
        )
        .await,
    );
    let surface = FakeSurface::default();
    sync(&h, &surface).await;
    let issue = surface.about("Tune the calendar skill").pop().unwrap();
    surface.comments.lock().unwrap().insert(
        issue.number.clone(),
        vec![Comment {
            id: "100".into(),
            author: "ada".into(),
            trusted: true,
            body: "/amend keep the old prompt as a fallback".into(),
        }],
    );
    surface.edit(&issue.number, |i| {
        i.labels.push(review::LABEL_ACCEPTED.to_string())
    });
    let synced = sync(&h, &surface).await;
    assert_eq!(synced.decisions.len(), 2, "{:?}", synced.decisions);
    let code = synced.decisions[1].code_item.clone().expect("a code item");
    let item = h.item(&code).await;
    assert_eq!(item.kind, WorkKind::Code);
    assert!(item
        .constraints
        .iter()
        .any(|c| c.contains("keep the old prompt")));
    assert_eq!(h.status(&id).await, Status::Done);
    assert!(!surface.issue(&issue.number).await.unwrap().unwrap().open);
    assert!(!surface
        .about("Carry out proposal: Tune the calendar skill")
        .is_empty());
    // The label is still there; the decision is not applied twice.
    let later = sync(&h, &surface).await;
    assert!(later.decisions.is_empty());
    assert_eq!(h.of_kind(WorkKind::Code).await.len(), 1);
}

#[tokio::test]
async fn accepting_project_proposal_preserves_its_native_runtime_and_repository() {
    let h = Harness::new(&["pinch"]);
    let mut draft = proposal(
        "Complete project intake",
        Some("project:intake"),
        Some(ReviewTier::Highest),
    );
    draft.worker_kind = WorkerKind::Codex;
    draft.writable_resources = vec!["repo:/tmp/project-review-fixture".into()];
    let id = accepted_root(file_rest(&h, draft).await);
    let result = ControlHandle::review_decision(&h.ctl, &id, ReviewDecision::Accept, "user:master")
        .await
        .unwrap();
    let code = h.item(&result.code_item.unwrap()).await;
    assert_eq!(code.worker_kind, WorkerKind::Codex);
    assert_eq!(
        code.writable_resources,
        ["repo:/tmp/project-review-fixture"]
    );
}
