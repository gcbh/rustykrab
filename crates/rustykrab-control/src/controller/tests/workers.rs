//! Phase 3 against a real store and real git: `code` results verified from
//! their workspace (scenario 2), the routing record and escalation of
//! scenario 17's first half, and a worker's own tracker kept out of the
//! store (scenario 31). The workers here create their workspace and commit
//! in it the way the adapters in `rustykrab-agent` do.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use rustykrab_core::work::{
    BlockedReason, EdgeKind, ResultReport, Rung, Status, WorkItemDraft, WorkKind, WorkerKind,
};
use rustykrab_core::Error;

use super::{draft, Harness};
use crate::controller::ControllerConfig;
use crate::routing::CODE;
use crate::worker::{Brief, RunUsage, Worker, WorkerCapabilities};
use crate::workspace::tests::{commit_all, run_git, Repo};
use crate::workspace::REPO_PREFIX;

/// What a coding worker claims about the change it makes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Claim {
    /// The commit it made and the path it changed.
    Honest,
    /// A path it did not change.
    WrongPath,
    /// A commit that does not exist, and no change at all.
    NoSuchCommit,
    /// An honest change, plus its own task list and Beads database in the
    /// worktree and one discovered draft.
    KeepsItsOwnTracker,
}

struct Coder {
    name: &'static str,
    kind: WorkerKind,
    repos: Vec<String>,
    claim: Claim,
    briefs: Mutex<Vec<Brief>>,
}

impl Coder {
    fn new(name: &'static str, kind: WorkerKind, repo: &Repo, claim: Claim) -> Arc<Coder> {
        Arc::new(Coder {
            name,
            kind,
            repos: vec![repo.path().display().to_string()],
            claim,
            briefs: Mutex::new(Vec::new()),
        })
    }

    fn runs(&self) -> usize {
        self.briefs.lock().unwrap().len()
    }
}

#[async_trait]
impl Worker for Coder {
    fn name(&self) -> &str {
        self.name
    }

    fn kind(&self) -> WorkerKind {
        self.kind
    }

    fn capabilities(&self) -> WorkerCapabilities {
        WorkerCapabilities {
            repos: self.repos.clone(),
            writable_resources: self
                .repos
                .iter()
                .map(|r| format!("{REPO_PREFIX}{r}"))
                .collect(),
            ..WorkerCapabilities::default()
        }
    }

    fn usage(&self, _run: &str) -> Option<RunUsage> {
        Some(RunUsage {
            tokens: 1_000,
            wall_ms: 4_000,
            iterations: 3,
            reminders: 0,
        })
    }

    async fn run(&self, brief: Brief) -> Result<ResultReport, Error> {
        self.briefs.lock().unwrap().push(brief.clone());
        let ws = brief
            .workspace
            .clone()
            .ok_or_else(|| Error::Internal("a code brief carries a workspace".into()))?;
        let claim = self.claim;
        tokio::task::spawn_blocking(move || {
            ws.create().map_err(Error::Internal)?;
            let mut report = ResultReport {
                summary: "changed the helper".to_string(),
                ..ResultReport::default()
            };
            if claim != Claim::NoSuchCommit {
                std::fs::write(ws.path.join("src/lib.rs"), "pub fn helper() {}\n").unwrap();
                if claim == Claim::KeepsItsOwnTracker {
                    std::fs::create_dir_all(ws.path.join(".beads")).unwrap();
                    std::fs::write(
                        ws.path.join(".beads/issues.jsonl"),
                        "{\"id\":\"bd-1\",\"title\":\"beads task\"}\n",
                    )
                    .unwrap();
                    std::fs::write(ws.path.join("TODO.md"), "- [ ] claude task\n").unwrap();
                    run_git(&ws.path, &["add", "src/lib.rs"]);
                    run_git(
                        &ws.path,
                        &[
                            "-c",
                            "user.name=t",
                            "-c",
                            "user.email=t@example.invalid",
                            "commit",
                            "-q",
                            "--no-gpg-sign",
                            "-m",
                            "change",
                        ],
                    );
                    report.commit = Some(run_git(&ws.path, &["rev-parse", "HEAD"]));
                    report.discovered = vec![WorkItemDraft {
                        kind: Some(WorkKind::Code),
                        title: "Add a changelog entry".to_string(),
                        objective: "Record the change in CHANGELOG.md".to_string(),
                        done_when: "CHANGELOG.md names the change".to_string(),
                        ..WorkItemDraft::default()
                    }];
                } else {
                    report.commit = Some(commit_all(&ws.path, "change"));
                }
            }
            report.changed_paths = vec![match claim {
                Claim::WrongPath => "src/elsewhere.rs".to_string(),
                _ => "src/lib.rs".to_string(),
            }];
            if claim == Claim::NoSuchCommit {
                report.commit = Some("0000000000000000000000000000000000000000".into());
            }
            ws.remove().map_err(Error::Internal)?;
            Ok(report)
        })
        .await
        .map_err(|e| Error::Internal(e.to_string()))?
    }
}

fn config(root: &tempfile::TempDir) -> ControllerConfig {
    ControllerConfig {
        worktree_root: Some(root.path().to_path_buf()),
        ..ControllerConfig::default()
    }
}

fn code_draft(title: &str, repo: &Repo, kind: WorkerKind) -> WorkItemDraft {
    let mut d = draft("code", title);
    d.kind = Some(WorkKind::Code);
    d.worker_kind = kind;
    d.writable_resources = vec![format!("{REPO_PREFIX}{}", repo.path().display())];
    d
}

#[tokio::test]
async fn scenario_02_a_commit_and_its_paths_are_verified_and_a_false_claim_is_caught() {
    let repo = Repo::new();
    let root = tempfile::tempdir().unwrap();
    let honest = Coder::new("pinch", WorkerKind::ClaudeCode, &repo, Claim::Honest);
    let h = Harness::with_fleet(config(&root), vec![honest.clone()]);

    let task = h
        .file_one(code_draft(
            "Add a status helper",
            &repo,
            WorkerKind::ClaudeCode,
        ))
        .await;
    h.drain().await;
    assert_eq!(h.status(&task).await, Status::Done);
    let evidence = h.store().work_evidence_list(&task).await.unwrap();
    let commit = evidence
        .iter()
        .find(|e| e.kind == "commit")
        .expect("commit evidence");
    assert_eq!(commit.verified_by.as_deref(), Some("git"));
    assert_eq!(
        run_git(repo.path(), &["cat-file", "-t", &commit.reference]),
        "commit"
    );
    assert!(evidence.iter().any(|e| e.kind == "changed_path"
        && e.reference == "src/lib.rs"
        && e.verified_by.as_deref() == Some("git")));
    let ws = evidence
        .iter()
        .find(|e| e.kind == "workspace")
        .expect("the workspace is recorded at lease time");
    assert!(ws.reference.contains(&root.path().display().to_string()));
    // The user's checkout never moved, and the worktree is gone.
    assert_eq!(run_git(repo.path(), &["status", "--short"]), "");
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
    let brief = honest.briefs.lock().unwrap()[0].clone();
    let workspace = brief.workspace.expect("a workspace");
    assert_eq!(workspace.repo, repo.path());
    assert!(workspace.branch.starts_with("rustykrab/work/"));
    // The worker is named in the report, and its record counts the result.
    let outbox = h.outbox().await;
    assert!(
        outbox[0].body.contains("Result (pinch)"),
        "{}",
        outbox[0].body
    );
    let record = h.ctl.registry().record_of("pinch", CODE).unwrap();
    assert_eq!(record.verified_done, 1);
    assert_eq!(record.cost.tokens, 1_000);

    // A result whose claimed paths differ from the diff.
    let liar = Coder::new("pinch", WorkerKind::ClaudeCode, &repo, Claim::WrongPath);
    let h = Harness::with_fleet(config(&root), vec![liar.clone()]);
    let claimed = h
        .file_one(code_draft(
            "Add a second helper",
            &repo,
            WorkerKind::ClaudeCode,
        ))
        .await;
    h.drain().await;
    assert_eq!(
        h.status(&claimed).await,
        Status::Blocked(BlockedReason::VerificationFailed)
    );
    let events = h.events(&claimed).await;
    let failed = events
        .iter()
        .filter(|e| e.to == Some(Status::Blocked(BlockedReason::VerificationFailed)))
        .count();
    assert!(failed >= 2, "each false claim is recorded: {failed}");
    let error = crate::ladder::last_error(&events).expect("an error");
    assert!(
        error.detail.contains("src/elsewhere.rs"),
        "{}",
        error.detail
    );
    assert!(h.rungs(&claimed).await.contains(&Rung::SwitchWorker));
    let record = h.ctl.registry().record_of("pinch", CODE).unwrap();
    assert!(record.claimed_not_verified >= 2);
    assert!(record.probation);
}

#[tokio::test]
async fn scenario_17_a_local_code_result_counts_and_a_failed_verification_escalates() {
    let repo = Repo::new();
    let root = tempfile::tempdir().unwrap();
    let local_good = Coder::new("krabby", WorkerKind::Local, &repo, Claim::Honest);
    let claude = Coder::new("pinch", WorkerKind::ClaudeCode, &repo, Claim::Honest);
    let h = Harness::with_fleet(config(&root), vec![local_good.clone(), claude.clone()]);

    // A code item with no worker constraint starts on the cheapest worker
    // the record qualifies: the local one, on probation.
    let first = h
        .file_one(code_draft("Document the helper", &repo, WorkerKind::Any))
        .await;
    h.drain().await;
    assert_eq!(h.status(&first).await, Status::Done);
    assert_eq!((local_good.runs(), claude.runs()), (1, 0));
    let record = h.ctl.registry().record_of("krabby", CODE).unwrap();
    assert_eq!(record.verified_done, 1);
    assert!(record.probation);

    // A local worker that claims an edit that never happened fails
    // verification twice, then the item escalates past its tier.
    let local_bad = Coder::new("krabby", WorkerKind::Local, &repo, Claim::NoSuchCommit);
    let claude = Coder::new("pinch", WorkerKind::ClaudeCode, &repo, Claim::Honest);
    let h = Harness::with_fleet(config(&root), vec![local_bad.clone(), claude.clone()]);
    let second = h
        .file_one(code_draft(
            "Document the helper's errors",
            &repo,
            WorkerKind::Any,
        ))
        .await;
    h.drain().await;
    assert_eq!(h.status(&second).await, Status::Done);
    assert_eq!((local_bad.runs(), claude.runs()), (2, 1));
    let events = h.events(&second).await;
    let failed = events
        .iter()
        .filter(|e| e.to == Some(Status::Blocked(BlockedReason::VerificationFailed)))
        .count();
    assert_eq!(failed, 2);
    assert_eq!(
        h.rungs(&second).await,
        vec![Rung::Repair, Rung::SwitchWorker]
    );
    let rung = events
        .iter()
        .filter_map(crate::controller::load::decode_rung)
        .find(|r| r.rung == Rung::SwitchWorker)
        .unwrap();
    assert_eq!(
        rung.outcome,
        "switched from krabby; escalating above tier 0"
    );
    let local = h.ctl.registry().record_of("krabby", CODE).unwrap();
    assert_eq!(local.claimed_not_verified, 2);
    let escalated = h.ctl.registry().record_of("pinch", CODE).unwrap();
    assert_eq!((escalated.verified_done, escalated.repairs), (1, 1));
    // The controller moves no default on its own: the policy is unchanged.
    let policy = crate::routing::RoutingPolicy::default();
    assert_eq!(policy.default_tiers.get(CODE), Some(&0));
}

#[tokio::test]
async fn scenario_31_one_discovered_draft_is_filed_and_the_workers_tracker_stays_out() {
    let repo = Repo::new();
    let root = tempfile::tempdir().unwrap();
    let coder = Coder::new(
        "pinch",
        WorkerKind::ClaudeCode,
        &repo,
        Claim::KeepsItsOwnTracker,
    );
    let h = Harness::with_fleet(config(&root), vec![coder]);
    let task = h
        .file_one(code_draft(
            "Add a status helper and track it",
            &repo,
            WorkerKind::ClaudeCode,
        ))
        .await;
    for _ in 0..4 {
        h.step().await;
    }
    assert_eq!(h.status(&task).await, Status::Done);
    let everything = h
        .store()
        .work_list(&rustykrab_store::WorkFilter {
            include_closed: true,
            ..Default::default()
        })
        .await
        .unwrap();
    // The follow-up inherits the repository and worker, so it runs too and
    // this worker discovers the same draft again from it: count only what
    // `task` discovered.
    let mut filed = Vec::new();
    for i in everything
        .iter()
        .filter(|i| i.title == "Add a changelog entry")
    {
        let edges = h.store().work_edges_of(&i.id).await.unwrap();
        if edges
            .iter()
            .any(|e| e.kind == EdgeKind::DiscoveredFrom && e.depends_on == task)
        {
            filed.push(i);
        }
    }
    assert_eq!(filed.len(), 1, "exactly one item from the one draft");
    assert_eq!(filed[0].kind, WorkKind::Code);
    assert_eq!(filed[0].worker_kind, WorkerKind::ClaudeCode);
    assert_eq!(
        filed[0].writable_resources,
        vec![format!("{REPO_PREFIX}{}", repo.path().display())]
    );
    assert!(
        !everything
            .iter()
            .any(|i| i.title.contains("beads task") || i.title.contains("claude task")),
        "nothing from the worker's own tracker reached the store"
    );
    // Its untracked files are not claimed changes either.
    let evidence = h.store().work_evidence_list(&task).await.unwrap();
    let paths: Vec<&str> = evidence
        .iter()
        .filter(|e| e.kind == "changed_path")
        .map(|e| e.reference.as_str())
        .collect();
    assert_eq!(paths, ["src/lib.rs"]);
}

#[tokio::test]
async fn a_code_claim_with_no_workspace_to_check_is_not_done() {
    let repo = Repo::new();
    // No worktree root: nothing to check a claimed commit against.
    let coder = Coder::new("pinch", WorkerKind::ClaudeCode, &repo, Claim::Honest);
    let h = Harness::with_fleet(ControllerConfig::default(), vec![coder.clone()]);
    let task = h
        .file_one(code_draft("Add a helper", &repo, WorkerKind::ClaudeCode))
        .await;
    h.drain().await;
    // The worker errors without a workspace; nothing it claims is done.
    assert_ne!(h.status(&task).await, Status::Done);
    assert!(coder.runs() >= 1);
}

/// Section 17: "cheapest-qualifying worker, and escalation only on
/// failure". A busy local worker is waited for; an idle coding agent is not
/// given the item because the cheap tier is occupied.
#[tokio::test]
async fn a_busy_cheapest_tier_is_waited_for_not_escalated_past() {
    let script = Arc::new(super::Script::default());
    let gate = Arc::new(tokio::sync::Notify::new());
    script.push(
        "First errand",
        super::Step::Wait(gate.clone(), Box::new(super::done("First errand"))),
    );
    let local = Arc::new(super::Scripted {
        name: "snapper".to_string(),
        concurrency: 1,
        script: script.clone(),
    });
    let repo = Repo::new();
    let claude = Coder::new("pinch", WorkerKind::ClaudeCode, &repo, Claim::Honest);
    let h = Harness::with_fleet(ControllerConfig::default(), vec![local, claude.clone()]);
    let first = h.file_one(draft("a", "First errand")).await;
    let second = h.file_one(draft("b", "Second errand")).await;
    h.step().await;
    h.step().await;
    assert_eq!(
        h.status(&second).await,
        Status::Ready,
        "it waits for the local worker"
    );
    assert_eq!(claude.runs(), 0);

    gate.notify_one();
    h.drain().await;
    assert_eq!(h.status(&first).await, Status::Done);
    assert_eq!(h.status(&second).await, Status::Done);
    assert_eq!(claude.runs(), 0, "the coding agent never took an errand");
    assert!(script
        .briefs()
        .iter()
        .all(|(worker, _)| worker == "snapper"));
}

/// A worker of any kind that finishes whatever it gets and keeps the
/// briefs.
struct Any {
    name: &'static str,
    kind: WorkerKind,
    briefs: Mutex<Vec<Brief>>,
}

#[async_trait]
impl Worker for Any {
    fn name(&self) -> &str {
        self.name
    }
    fn kind(&self) -> WorkerKind {
        self.kind
    }
    fn capabilities(&self) -> WorkerCapabilities {
        WorkerCapabilities::default()
    }
    async fn run(&self, brief: Brief) -> Result<ResultReport, Error> {
        self.briefs.lock().unwrap().push(brief.clone());
        Ok(super::done(&brief.title))
    }
}

/// One source of truth for a capability item's mode: its review facet.
/// A build routes as `capability:build` (above the local worker's tier)
/// and its brief says build; the same need acquired is ordinary work.
#[tokio::test]
async fn a_capability_items_facet_decides_its_class_and_its_briefs_mode() {
    use rustykrab_core::work::CapabilityMode;
    let local = Arc::new(Any {
        name: "snapper",
        kind: WorkerKind::Local,
        briefs: Mutex::new(Vec::new()),
    });
    let claude = Arc::new(Any {
        name: "pinch",
        kind: WorkerKind::ClaudeCode,
        briefs: Mutex::new(Vec::new()),
    });
    let h = Harness::with_fleet(
        ControllerConfig::default(),
        vec![local.clone(), claude.clone()],
    );
    let need = crate::routing::CapabilityRef {
        gap: crate::errors::GapKind::Install,
        subject: "ffmpeg".into(),
    }
    .to_ref();
    for (title, mode) in [
        ("Build the converter", CapabilityMode::Build),
        ("Install the converter", CapabilityMode::Acquire),
    ] {
        let mut d = draft("cap", title);
        d.kind = Some(WorkKind::Capability);
        d.capability = Some(mode);
        d.artifact_refs = vec![need.clone()];
        h.file_one(d).await;
    }
    h.drain().await;
    let built = claude.briefs.lock().unwrap().clone();
    let acquired = local.briefs.lock().unwrap().clone();
    assert_eq!(built.len(), 1, "the build went up a tier");
    assert_eq!(built[0].title, "Build the converter");
    assert_eq!(built[0].capability, Some(CapabilityMode::Build));
    assert_eq!(acquired.len(), 1, "the acquisition stayed cheapest");
    assert_eq!(acquired[0].capability, Some(CapabilityMode::Acquire));
}

/// Section 10: moving a class's default tier is a routing proposal, and
/// accepting it is what moves the default. The controller routes by the
/// moved default from then on and files no code item for the move.
#[tokio::test]
async fn an_accepted_routing_proposal_moves_the_default_tier() {
    use rustykrab_core::proposal::{ReviewDecision, ROUTING_DEFAULT};
    let local = Arc::new(Any {
        name: "snapper",
        kind: WorkerKind::Local,
        briefs: Mutex::new(Vec::new()),
    });
    let claude = Arc::new(Any {
        name: "pinch",
        kind: WorkerKind::ClaudeCode,
        briefs: Mutex::new(Vec::new()),
    });
    let h = Harness::with_fleet(
        ControllerConfig::default(),
        vec![local.clone(), claude.clone()],
    );
    let store = h.store().clone();
    store.workers().seed_default_tier(CODE, 0).await.unwrap();

    let mut routing = draft("routing", "Routing: move the default for code up");
    routing.kind = Some(WorkKind::Proposal);
    routing.subject = Some(format!("routing:{CODE}"));
    routing.artifact_refs = vec![rustykrab_core::work::ArtifactRef {
        kind: ROUTING_DEFAULT.to_string(),
        value: format!("3 {CODE}"),
    }];
    let proposal = h.file_one(routing).await;
    let out = crate::handle::ControlHandle::review_decision(
        &h.ctl,
        &proposal,
        ReviewDecision::Accept,
        "reviewer:github:ada",
    )
    .await
    .unwrap();
    assert_eq!(out.status, Status::Done);
    assert!(out.code_item.is_none(), "a routing move is data, not code");
    let tiers = store.workers().default_tiers().await.unwrap();
    let code = tiers.iter().find(|d| d.class == CODE).unwrap();
    assert_eq!((code.tier, code.set_by.as_str()), (3, proposal.as_str()));

    // A code item now starts on the coding agent, not the local worker.
    let mut helper = draft("code", "Add a helper");
    helper.kind = Some(WorkKind::Code);
    let task = h.file_one(helper).await;
    h.step().await;
    h.step().await;
    assert!(local.briefs.lock().unwrap().is_empty());
    assert!(claude.briefs.lock().unwrap().iter().any(|b| b.item == task));
}
