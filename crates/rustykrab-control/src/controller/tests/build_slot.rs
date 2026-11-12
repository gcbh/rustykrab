//! One building run at a time on a machine a local model shares
//! ([`ControllerConfig::serialise_builds`]): an item whose run gets a
//! writable repository workspace waits, `ready`, while another such run is
//! live, whichever worker would take it and whatever the item's kind; other
//! work leases alongside. On 12 November 2026 a local and an external
//! worker built the same workspace at once beside a resident model: swap
//! reached 27 GB and neither committed in two hours. The same day two items
//! filed through `POST /api/work`, stored as `task`, built side by side
//! because the first version of the gate required `code`.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use rustykrab_core::work::{ResultReport, Status, WorkItemDraft, WorkKind, WorkerKind};
use rustykrab_core::Error;

use super::{draft, Harness};
use crate::controller::ControllerConfig;
use crate::worker::{Brief, Worker, WorkerCapabilities};
use crate::workspace::tests::Repo;
use crate::workspace::REPO_PREFIX;

/// A worker that takes any item and holds each run until the test opens
/// the gate once for it. A run with a workspace creates it, as the
/// adapters do, and reports a check it ran without changing anything.
struct Held {
    name: &'static str,
    kind: WorkerKind,
    gate: Arc<tokio::sync::Semaphore>,
    /// Runs with a workspace in flight across the fleet, and the most ever.
    building: Arc<AtomicUsize>,
    peak: Arc<AtomicUsize>,
    seen: Mutex<Vec<String>>,
}

#[async_trait]
impl Worker for Held {
    fn name(&self) -> &str {
        self.name
    }
    fn kind(&self) -> WorkerKind {
        self.kind
    }
    fn capabilities(&self) -> WorkerCapabilities {
        WorkerCapabilities {
            models: vec![format!("model-of-{}", self.name)],
            writable_resources: vec!["*".to_string()],
            ..WorkerCapabilities::default()
        }
    }
    fn isolates_worktree(&self) -> bool {
        true
    }
    async fn run(&self, brief: Brief) -> Result<ResultReport, Error> {
        self.seen.lock().unwrap().push(brief.title.clone());
        let ws = brief.workspace.clone();
        if let Some(ws) = &ws {
            let ws = ws.clone();
            tokio::task::spawn_blocking(move || ws.create())
                .await
                .map_err(|e| Error::Internal(e.to_string()))?
                .map_err(Error::Internal)?;
            let now = self.building.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(now, Ordering::SeqCst);
        }
        self.gate.acquire().await.expect("gate open").forget();
        if let Some(ws) = ws {
            self.building.fetch_sub(1, Ordering::SeqCst);
            tokio::task::spawn_blocking(move || ws.remove())
                .await
                .map_err(|e| Error::Internal(e.to_string()))?
                .map_err(Error::Internal)?;
        }
        Ok(ResultReport {
            summary: "Already correct; no change needed".into(),
            checks_run: vec!["cargo test".into()],
            ..ResultReport::default()
        })
    }
}

struct Fleet {
    h: Harness,
    gate: Arc<tokio::sync::Semaphore>,
    building: Arc<AtomicUsize>,
    peak: Arc<AtomicUsize>,
    repo: Repo,
    _root: tempfile::TempDir,
}

/// A local and an external worker, both free, over one repository.
fn fleet(serialise_builds: bool) -> Fleet {
    let repo = Repo::new();
    let root = tempfile::tempdir().unwrap();
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let building = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let worker = |name, kind| {
        Arc::new(Held {
            name,
            kind,
            gate: gate.clone(),
            building: building.clone(),
            peak: peak.clone(),
            seen: Mutex::new(Vec::new()),
        }) as Arc<dyn Worker>
    };
    let h = Harness::with_fleet(
        ControllerConfig {
            worktree_root: Some(root.path().to_path_buf()),
            serialise_builds,
            ..ControllerConfig::default()
        },
        vec![
            worker("planner", WorkerKind::Local),
            worker("codex-primary", WorkerKind::Codex),
        ],
    );
    Fleet {
        h,
        gate,
        building,
        peak,
        repo,
        _root: root,
    }
}

/// An item that changes `repo`, filed as the API and chat file it: the
/// draft's kind is ignored on the wire, so it is stored as `task`.
fn change(title: &str, repo: &Repo) -> WorkItemDraft {
    let wire = serde_json::json!({
        "kind": "code",
        "title": title,
        "objective": format!("objective of {title}"),
        "done_when": format!("{title} is done"),
        "writable_resources": [format!("{REPO_PREFIX}{}", repo.path().display())],
    });
    let mut d: WorkItemDraft = serde_json::from_value(wire).unwrap();
    assert_eq!(d.kind, None, "the wire draft's kind is not taken");
    d.tmp = Some(title.to_string());
    d
}

/// The same change filed in process as a `code` item.
fn code(title: &str, repo: &Repo) -> WorkItemDraft {
    let mut d = change(title, repo);
    d.kind = Some(WorkKind::Code);
    d
}

#[tokio::test]
async fn beside_a_local_model_two_builds_lease_one_at_a_time_and_other_work_alongside() {
    let f = fleet(true);
    // Two one-test changes filed together through the API, as on
    // 12 November: stored as `task`, each with a writable repository.
    let a = f.h.file_one(change("Fix the progress test", &f.repo)).await;
    let b = f.h.file_one(change("Fix the cancel reply", &f.repo)).await;
    assert_eq!(f.h.item(&a).await.kind, WorkKind::Task);
    assert_eq!(f.h.item(&b).await.kind, WorkKind::Task);
    let first = f.h.tick().await.leased;
    assert_eq!(first.len(), 1, "one build at a time: {first:?}");
    let waiting = if first[0] == a { b.clone() } else { a.clone() };
    for _ in 0..3 {
        assert!(f.h.step().await.leased.is_empty());
    }
    // It waits as an item waits for a busy worker: ready, nothing spent,
    // nothing stalled or failed, although a worker is free.
    assert_eq!(f.h.status(&waiting).await, Status::Ready);
    assert_eq!(f.h.leases(&waiting).await, 0);
    assert!(f.h.rungs(&waiting).await.is_empty());
    assert!(crate::ladder::last_error(&f.h.events(&waiting).await).is_none());
    assert_eq!(f.h.ctl.running().len(), 1);

    // Work with no repository does not build: a `task` like them, it
    // leases on the free worker meanwhile.
    let note = f.h.file_one(draft("note", "Summarise the run")).await;
    assert_eq!(f.h.item(&note).await.kind, WorkKind::Task);
    assert_eq!(f.h.step().await.leased, vec![note.clone()]);
    assert_eq!(f.h.ctl.running().len(), 2);

    // The first build ends; only then is the second leased.
    f.gate.add_permits(1);
    let mut leased = Vec::new();
    for _ in 0..20 {
        leased.extend(f.h.step().await.leased);
        if leased.contains(&waiting) {
            break;
        }
    }
    assert!(leased.contains(&waiting), "{leased:?}");
    f.gate.add_permits(4);
    f.h.drain_settled().await;
    for id in [&a, &b, &note] {
        assert_eq!(f.h.status(id).await, Status::Done, "{id}");
    }
    assert_eq!(f.peak.load(Ordering::SeqCst), 1, "never two builds at once");
}

#[tokio::test]
async fn without_a_local_model_two_builds_lease_together() {
    let f = fleet(false);
    let a = f.h.file_one(change("Fix the progress test", &f.repo)).await;
    let b = f.h.file_one(change("Fix the cancel reply", &f.repo)).await;
    let mut leased = f.h.tick().await.leased;
    leased.sort();
    let mut both = vec![a.clone(), b.clone()];
    both.sort();
    assert_eq!(leased, both);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while f.building.load(Ordering::SeqCst) < 2 {
        assert!(
            std::time::Instant::now() < deadline,
            "both builds run at once"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    f.gate.add_permits(2);
    f.h.drain_settled().await;
    assert_eq!(f.h.status(&a).await, Status::Done);
    assert_eq!(f.h.status(&b).await, Status::Done);
    assert_eq!(f.peak.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn a_read_only_review_of_a_repository_does_not_wait_for_the_build_slot() {
    let f = fleet(true);
    let build = f.h.file_one(change("Fix the progress test", &f.repo)).await;
    assert_eq!(f.h.tick().await.leased, vec![build.clone()]);
    let mut review = change("Review the change", &f.repo);
    review.artifact_refs = vec![rustykrab_core::work::ArtifactRef {
        kind: rustykrab_core::dream_review::REVIEW_ONLY.into(),
        value: "true".into(),
    }];
    let review = f.h.file_one(review).await;
    let item = f.h.item(&review).await;
    let (_, occupied) = f.h.ctl.load_by_worker();
    assert!(occupied.build);
    assert!(!f.h.ctl.waits_for_build(&item, &occupied));
    let waiting = f.h.item(&build).await;
    assert!(
        f.h.ctl.waits_for_build(&waiting, &occupied),
        "another build would wait"
    );
    f.gate.add_permits(4);
    f.h.drain_settled().await;
}

#[tokio::test]
async fn a_read_only_review_leases_alongside_a_build() {
    let f = fleet(true);
    let build = f.h.file_one(change("Fix the progress test", &f.repo)).await;
    assert_eq!(f.h.tick().await.leased, vec![build.clone()]);
    let mut review = change("Review the change", &f.repo);
    review.artifact_refs = vec![rustykrab_core::work::ArtifactRef {
        kind: rustykrab_core::dream_review::REVIEW_ONLY.into(),
        value: "true".into(),
    }];
    let review = f.h.file_one(review).await;
    assert_eq!(f.h.step().await.leased, vec![review.clone()]);
    assert_eq!(f.h.ctl.running().len(), 2);
    let (_, occupied) = f.h.ctl.load_by_worker();
    assert!(occupied.build, "the build holds the slot");
    f.gate.add_permits(4);
    f.h.drain_settled().await;
    assert_eq!(f.h.status(&build).await, Status::Done);
    // (The held worker's report names a check, which the review guard
    // stops; how a review closes is `review_guard.rs`'s, not this test's.)
}

#[tokio::test]
async fn a_code_item_filed_in_process_still_takes_the_slot() {
    let f = fleet(true);
    let a = f.h.file_one(code("Fix the progress test", &f.repo)).await;
    let b = f.h.file_one(change("Fix the cancel reply", &f.repo)).await;
    assert_eq!(f.h.item(&a).await.kind, WorkKind::Code);
    assert_eq!(
        f.h.tick().await.leased.len(),
        1,
        "a `code` and a `task` build"
    );
    f.gate.add_permits(4);
    f.h.drain_settled().await;
    assert_eq!(f.h.status(&a).await, Status::Done);
    assert_eq!(f.h.status(&b).await, Status::Done);
    assert_eq!(f.peak.load(Ordering::SeqCst), 1, "never two builds at once");
}
