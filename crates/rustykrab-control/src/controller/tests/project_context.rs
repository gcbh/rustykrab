//! Real git + durable SQLite: runtime changes carry project intent and verified files.
use super::{draft, Harness};
use crate::{
    controller::{Clock, Controller, ControllerConfig},
    handoff::PROJECT_CONTEXT,
    worker::{Brief, Worker, WorkerCapabilities},
    workspace::{
        head,
        tests::{commit_all, Repo},
    },
};
use async_trait::async_trait;
use chrono::{TimeDelta, Utc};
use rustykrab_core::{
    work::{ArtifactRef, BlockedReason, ResultReport, Status, WorkItemDraft, WorkerKind},
    Error,
};
use rustykrab_projects::*;
use std::sync::{Arc, Mutex};

#[tokio::test]
async fn repaired_handoffs_resume_old_size_holds_and_integrate_verified_history() {
    let repo = Repo::new();
    let root = tempfile::tempdir().unwrap();
    let config = ControllerConfig {
        worktree_root: Some(root.path().to_owned()),
        ..Default::default()
    };
    let claude = Coder::new(WorkerKind::ClaudeCode, &repo);
    let mut h = Harness::with_fleet(config.clone(), vec![claude]);
    let project = project(&h, &repo).await;
    let first = h
        .file_one(code(&repo, WorkerKind::ClaudeCode, "first", None))
        .await;
    h.drain_settled().await;
    assert_eq!(h.status(&first).await, Status::Done);
    std::fs::write(repo.path().join("upstream.txt"), "new repository work").unwrap();
    let repository_head = commit_all(repo.path(), "upstream change");
    let codex = Coder::new(WorkerKind::Codex, &repo);
    h.ctl =
        Controller::new(h.store().clone(), vec![codex.clone()], config).with_clock(h.clock.clone());
    let next = h
        .file_one(code(
            &repo,
            WorkerKind::Codex,
            "continue",
            Some(project.project.id),
        ))
        .await;
    h.ctl.hold_context(&h.store().work_get(&next).await.unwrap().unwrap(),
        "project context exceeds the handoff limit; narrow the execution slice before continuing".into()).await.unwrap();
    // Old releases could insert the same technical question more than once.
    let previous = h
        .store()
        .questions_list(&Default::default())
        .await
        .unwrap()
        .into_iter()
        .find(|q| q.item == next)
        .unwrap();
    let mut batch = super::super::batch::Batch::new(h.ctl.load().await.unwrap(), h.clock.now());
    h.ctl.record_question_sync(
        &mut batch,
        super::super::questions::NewQuestion::open(
            &next,
            previous.kind,
            previous.text,
            "project_context",
        ),
    );
    h.ctl.commit(batch, &mut Default::default()).await.unwrap();
    h.drain_settled().await;
    assert_eq!(h.status(&next).await, Status::Done);
    assert_eq!(head(repo.path()).unwrap(), repository_head);
    let brief = {
        let briefs = codex.received.lock().unwrap();
        assert_eq!(briefs.len(), 1);
        briefs[0].clone()
    };
    let workspace = brief.workspace.as_ref().unwrap();
    assert!(crate::workspace::is_ancestor(repo.path(), &repository_head, &workspace.base).unwrap());
    let questions = h.store().questions_list(&Default::default()).await.unwrap();
    let resolved: Vec<_> = questions.iter().filter(|q| q.item == next).collect();
    assert_eq!(resolved.len(), 2);
    for question in resolved {
        assert_eq!(
            question.status,
            rustykrab_core::questions::QuestionStatus::Obsolete
        );
        assert!(
            question.answer.is_none(),
            "recovery must not invent a user decision"
        );
    }
}

#[tokio::test]
async fn handoff_recovery_preserves_an_independent_user_decision() {
    let repo = Repo::new();
    let root = tempfile::tempdir().unwrap();
    let coder = Coder::new(WorkerKind::Codex, &repo);
    let h = Harness::with_fleet(
        ControllerConfig {
            worktree_root: Some(root.path().to_owned()),
            ..Default::default()
        },
        vec![coder.clone()],
    );
    let project = project(&h, &repo).await;
    let item = h
        .file_one(code(
            &repo,
            WorkerKind::Codex,
            "small slice",
            Some(project.project.id),
        ))
        .await;
    h.ctl
        .hold_context(
            &h.store().work_get(&item).await.unwrap().unwrap(),
            "old handoff limit".into(),
        )
        .await
        .unwrap();
    let mut batch = super::super::batch::Batch::new(h.ctl.load().await.unwrap(), h.clock.now());
    let question = super::super::questions::NewQuestion::open(
        &item,
        rustykrab_core::questions::QuestionKind::Decision,
        "Which product behavior should this change implement?".into(),
        "user_decision",
    );
    h.ctl.record_question(&mut batch, question).await.unwrap();
    h.ctl.commit(batch, &mut Default::default()).await.unwrap();
    h.drain_settled().await;
    assert_eq!(
        h.status(&item).await,
        Status::Blocked(BlockedReason::NeedsDecision)
    );
    assert!(coder.received.lock().unwrap().is_empty());
    assert!(h
        .store()
        .questions_list(&Default::default())
        .await
        .unwrap()
        .iter()
        .all(|q| q.answer.is_none()));
}

#[tokio::test]
async fn handoff_recovery_does_not_discard_oversized_current_requirements() {
    let repo = Repo::new();
    let root = tempfile::tempdir().unwrap();
    let coder = Coder::new(WorkerKind::Codex, &repo);
    let h = Harness::with_fleet(
        ControllerConfig {
            worktree_root: Some(root.path().to_owned()),
            ..Default::default()
        },
        vec![coder.clone()],
    );
    let project = project(&h, &repo).await;
    let node = *project.revision.nodes.keys().next().unwrap();
    h.store()
        .projects()
        .apply(
            &project.project.id,
            &project.revision.id,
            PlanChangeSet::new(
                "large current requirement",
                project.project.id,
                project.revision.id.clone(),
                "Keep every requirement",
                RevisionAuthor::User,
                Utc::now(),
                vec![PlanChange::UpdateNode {
                    node_id: node,
                    patch: PlanNodePatch {
                        body: Some("required ".repeat(20_000)),
                        ..Default::default()
                    },
                    provenance: vec![provenance()],
                }],
            ),
        )
        .await
        .unwrap();
    let item = h
        .file_one(code(
            &repo,
            WorkerKind::Codex,
            "small slice",
            Some(project.project.id),
        ))
        .await;
    h.ctl
        .hold_context(
            &h.store().work_get(&item).await.unwrap().unwrap(),
            "old handoff limit".into(),
        )
        .await
        .unwrap();
    h.drain_settled().await;
    assert_eq!(
        h.status(&item).await,
        Status::Blocked(BlockedReason::NeedsDecision)
    );
    assert!(coder.received.lock().unwrap().is_empty());
    let questions = h.store().questions_list(&Default::default()).await.unwrap();
    assert_eq!(
        questions[0].status,
        rustykrab_core::questions::QuestionStatus::Open
    );
}

struct Coder {
    kind: WorkerKind,
    repo: String,
    received: Mutex<Vec<Brief>>,
}
impl Coder {
    fn new(kind: WorkerKind, repo: &Repo) -> Arc<Self> {
        Arc::new(Self {
            kind,
            repo: repo.path().display().to_string(),
            received: Mutex::new(Vec::new()),
        })
    }
}
#[async_trait]
impl Worker for Coder {
    fn name(&self) -> &str {
        if self.kind == WorkerKind::Codex {
            "codex"
        } else {
            "claude"
        }
    }
    fn kind(&self) -> WorkerKind {
        self.kind
    }
    fn capabilities(&self) -> WorkerCapabilities {
        WorkerCapabilities {
            repos: vec![self.repo.clone()],
            writable_resources: vec![format!("repo:{}", self.repo)],
            ..Default::default()
        }
    }
    async fn run(&self, brief: Brief) -> std::result::Result<ResultReport, Error> {
        self.received.lock().unwrap().push(brief.clone());
        tokio::task::spawn_blocking(move || {
            let ws = brief.workspace.unwrap();
            ws.create().unwrap();
            if brief.title == "continue" {
                assert_eq!(
                    std::fs::read_to_string(ws.path.join("first.txt")).unwrap(),
                    "verified first change"
                );
                let ctx = brief.project_context.as_ref().unwrap();
                assert!(ctx
                    .snapshot
                    .revision
                    .nodes
                    .values()
                    .any(|n| n.body.starts_with("Use CLI subscriptions")));
                assert!(ctx
                    .work
                    .iter()
                    .any(|w| w.status == Status::Done && w.worker.as_deref() == Some("claude")));
            }
            let path = if brief.title == "continue" {
                "second.txt"
            } else {
                "first.txt"
            };
            std::fs::write(ws.path.join(path), "verified first change").unwrap();
            let commit = commit_all(&ws.path, "worker change");
            ws.remove().unwrap();
            Ok(ResultReport {
                summary: "Implemented and verified".into(),
                commit: Some(commit),
                changed_paths: vec![path.into()],
                ..Default::default()
            })
        })
        .await
        .unwrap()
    }
}
fn provenance() -> Provenance {
    Provenance {
        classification: ProvenanceClassification::UserStated,
        source: ProvenanceSource::Manual {
            reference: "user instruction".into(),
        },
        recorded_at: Utc::now(),
        confidence: None,
        freshness: None,
    }
}
async fn project(h: &Harness, repo: &Repo) -> ProjectSnapshot {
    let mut command = CreateProject::new(
        "project",
        ProjectId::new(),
        "Shared project",
        RevisionAuthor::User,
        Utc::now(),
    );
    command.repository_id = Some(repo.path().display().to_string());
    command.initial_changes = vec![PlanChange::AddNode {
        node: PlanNodeDraft::new(
            NodeId::new(),
            NodeKind::Constraint,
            "Runtime choice",
            "Use CLI subscriptions",
            NodeData::generic("active"),
            vec![provenance()],
        ),
    }];
    h.store().projects().create(command).await.unwrap().snapshot
}
fn code(repo: &Repo, kind: WorkerKind, title: &str, project: Option<ProjectId>) -> WorkItemDraft {
    let mut d = draft("code", title);
    d.kind = Some(rustykrab_core::work::WorkKind::Code);
    d.worker_kind = kind;
    d.writable_resources = vec![format!("repo:{}", repo.path().display())];
    if let Some(project) = project {
        d.artifact_refs.push(ArtifactRef {
            kind: "project".into(),
            value: project.to_string(),
        });
    }
    d
}
#[tokio::test]
async fn claude_to_codex_rehydrates_archived_progress_after_a_fresh_controller() {
    let repo = Repo::new();
    let root = tempfile::tempdir().unwrap();
    let original = head(repo.path()).unwrap();
    let claude = Coder::new(WorkerKind::ClaudeCode, &repo);
    let mut h = Harness::with_fleet(
        ControllerConfig {
            worktree_root: Some(root.path().to_owned()),
            ..Default::default()
        },
        vec![claude.clone()],
    );
    let project = project(&h, &repo).await;
    let first = h
        .file_one(code(&repo, WorkerKind::ClaudeCode, "first", None))
        .await;
    h.drain_settled().await;
    assert_eq!(h.status(&first).await, Status::Done);
    let first_commit = h
        .store()
        .work_evidence_list(&first)
        .await
        .unwrap()
        .into_iter()
        .find(|e| e.kind == "commit")
        .unwrap()
        .reference;
    h.store()
        .work_archive_compact(
            std::slice::from_ref(&first),
            Utc::now() + TimeDelta::days(8),
        )
        .await
        .unwrap();
    assert!(h.store().work_get(&first).await.unwrap().is_none());
    // A corrected project revision reaches the next runtime; the previous receipt stays immutable.
    let node = *project.revision.nodes.keys().next().unwrap();
    let updated = h
        .store()
        .projects()
        .apply(
            &project.project.id,
            &project.revision.id,
            PlanChangeSet::new(
                "correction",
                project.project.id,
                project.revision.id.clone(),
                "Carry verified progress",
                RevisionAuthor::User,
                Utc::now() + TimeDelta::seconds(1),
                vec![PlanChange::UpdateNode {
                    node_id: node,
                    patch: PlanNodePatch {
                        body: Some("Use CLI subscriptions; carry verified work".into()),
                        ..Default::default()
                    },
                    provenance: vec![provenance()],
                }],
            ),
        )
        .await
        .unwrap()
        .snapshot;
    let frozen = h
        .store()
        .work_evidence_list(&first)
        .await
        .unwrap()
        .into_iter()
        .find(|e| e.kind == PROJECT_CONTEXT)
        .unwrap();
    assert_eq!(
        frozen.hash.as_deref(),
        Some(project.revision.id.to_string().as_str())
    );
    // Reopen SQLite and change runtime. No chat session or in-memory project cache is reused.
    let reopened = rustykrab_store::Store::open(&h.dir.0, vec![9; 32]).unwrap();
    let codex = Coder::new(WorkerKind::Codex, &repo);
    h.ctl = Controller::new(reopened.clone(), vec![codex.clone()], h.config.clone())
        .with_clock(h.clock.clone());
    let next = h
        .file_one(code(
            &repo,
            WorkerKind::Codex,
            "continue",
            Some(project.project.id),
        ))
        .await;
    h.drain_settled().await;
    assert_eq!(h.status(&next).await, Status::Done);
    let received = codex.received.lock().unwrap()[0].clone();
    assert_eq!(received.workspace.as_ref().unwrap().base, first_commit);
    let ctx = received.project_context.unwrap();
    assert_eq!(ctx.snapshot.revision.id, updated.revision.id);
    assert_eq!(
        ctx.snapshot.revision.nodes[&node].body,
        "Use CLI subscriptions; carry verified work"
    );
    assert_eq!(ctx.base_sources, vec![first]);
    assert_eq!(head(repo.path()).unwrap(), original);
    let stored = reopened
        .work_evidence_list(&next)
        .await
        .unwrap()
        .into_iter()
        .find(|e| e.kind == PROJECT_CONTEXT)
        .unwrap();
    assert_eq!(stored.verified_by.as_deref(), Some("controller"));
    assert_eq!(
        stored.hash.as_deref(),
        Some(updated.revision.id.to_string().as_str())
    );
    let monitor = reopened.work_monitor_snapshot(200, 50).await.unwrap();
    let observed = monitor.items.iter().find(|r| r.item.id == next).unwrap();
    assert_eq!(observed.workspace.as_ref().unwrap()["base"], first_commit);
    assert_eq!(
        observed.project_context.as_ref().unwrap()["base_sources"],
        serde_json::json!(ctx.base_sources)
    );
}
#[tokio::test]
async fn missing_or_ambiguous_project_does_not_start_a_worker() {
    let repo = Repo::new();
    let root = tempfile::tempdir().unwrap();
    let coder = Coder::new(WorkerKind::Codex, &repo);
    let h = Harness::with_fleet(
        ControllerConfig {
            worktree_root: Some(root.path().to_owned()),
            ..Default::default()
        },
        vec![coder.clone()],
    );
    let item = h
        .file_one(code(
            &repo,
            WorkerKind::Codex,
            "missing",
            Some(ProjectId::new()),
        ))
        .await;
    h.drain_settled().await;
    assert_eq!(
        h.status(&item).await,
        Status::Blocked(BlockedReason::NeedsDecision)
    );
    assert!(coder.received.lock().unwrap().is_empty());
    let _ = project(&h, &repo).await;
    let mut other = CreateProject::new(
        "other",
        ProjectId::new(),
        "Other project",
        RevisionAuthor::User,
        Utc::now(),
    );
    other.repository_id = Some(repo.path().display().to_string());
    h.store().projects().create(other).await.unwrap();
    let item = h
        .file_one(code(&repo, WorkerKind::Codex, "ambiguous", None))
        .await;
    h.drain_settled().await;
    assert_eq!(
        h.status(&item).await,
        Status::Blocked(BlockedReason::NeedsDecision)
    );
    assert!(coder.received.lock().unwrap().is_empty());
}

struct Overclaims(Arc<Coder>);
#[async_trait]
impl Worker for Overclaims {
    fn name(&self) -> &str {
        "overclaims"
    }
    fn kind(&self) -> WorkerKind {
        self.0.kind()
    }
    fn capabilities(&self) -> WorkerCapabilities {
        self.0.capabilities()
    }
    async fn run(&self, brief: Brief) -> std::result::Result<ResultReport, Error> {
        let mut report = self.0.run(brief).await?;
        report.changed_paths = vec!["never-changed.txt".into()];
        Ok(report)
    }
}
#[tokio::test]
async fn unfinished_attempts_have_inspection_pointers_and_never_supply_the_code_base() {
    let repo = Repo::new();
    let original = head(repo.path()).unwrap();
    let root = tempfile::tempdir().unwrap();
    let bad = Arc::new(Overclaims(Coder::new(WorkerKind::ClaudeCode, &repo)));
    let mut h = Harness::with_fleet(
        ControllerConfig {
            worktree_root: Some(root.path().to_owned()),
            ..Default::default()
        },
        vec![bad],
    );
    let project = project(&h, &repo).await;
    let first = h
        .file_one(code(
            &repo,
            WorkerKind::ClaudeCode,
            "unverified",
            Some(project.project.id),
        ))
        .await;
    h.drain_settled().await;
    assert_ne!(h.status(&first).await, Status::Done);
    let codex = Coder::new(WorkerKind::Codex, &repo);
    h.ctl = Controller::new(h.store().clone(), vec![codex.clone()], h.config.clone())
        .with_clock(h.clock.clone());
    let next = h
        .file_one(code(
            &repo,
            WorkerKind::Codex,
            "inspect prior attempt",
            Some(project.project.id),
        ))
        .await;
    h.drain_settled().await;
    assert_eq!(h.status(&next).await, Status::Done);
    let received = codex.received.lock().unwrap()[0].clone();
    assert_eq!(received.workspace.unwrap().base, original);
    let ctx = received.project_context.unwrap();
    assert!(ctx.base_sources.is_empty());
    let previous = ctx.work.iter().find(|w| w.item == first).unwrap();
    let attempt = previous.unfinished_attempt.as_ref().unwrap();
    assert!(attempt.run.is_some() && attempt.workspace.is_some() && attempt.error.is_some());
    assert!(!previous.evidence.iter().any(|e| e.kind == "commit"));
}

#[tokio::test]
async fn accumulated_project_history_does_not_overflow_a_small_execution_slice() {
    let repo = Repo::new();
    let root = tempfile::tempdir().unwrap();
    let config = ControllerConfig {
        worktree_root: Some(root.path().to_owned()),
        ..Default::default()
    };
    let mut h = Harness::with(config, super::StaticCatalog::default(), &["history"]);
    let project = project(&h, &repo).await;
    for i in 0..8 {
        let title = format!("Earlier report {i}");
        h.script.push(
            &title,
            super::report(ResultReport {
                summary: format!("Historical report {i}: {}", "detail ".repeat(4_000)),
                ..Default::default()
            }),
        );
        let mut old = draft("old", &title);
        old.artifact_refs.push(ArtifactRef {
            kind: "project".into(),
            value: project.project.id.to_string(),
        });
        h.file_one(old).await;
        h.drain_settled().await;
    }
    let coder = Coder::new(WorkerKind::Codex, &repo);
    h.ctl = Controller::new(h.store().clone(), vec![coder.clone()], h.config.clone())
        .with_clock(h.clock.clone());
    let next = h
        .file_one(code(
            &repo,
            WorkerKind::Codex,
            "small slice",
            Some(project.project.id),
        ))
        .await;
    h.drain_settled().await;
    assert_eq!(h.status(&next).await, Status::Done);
    let brief = coder.received.lock().unwrap()[0].clone();
    let context = brief.project_context.unwrap();
    assert!(serde_json::to_vec(&context).unwrap().len() > 128 * 1024);
    let view = context.execution_view();
    assert_eq!(view.work.len(), 1);
    assert_eq!(view.work[0].item, next);
    assert_eq!(view.history_items, 9);
    assert_eq!(view.snapshot, &project);
    assert!(serde_json::to_vec(&view).unwrap().len() < 16 * 1024);
    let frozen = h
        .store()
        .work_evidence_list(&next)
        .await
        .unwrap()
        .into_iter()
        .find(|e| e.kind == PROJECT_CONTEXT)
        .unwrap();
    assert_eq!(
        serde_json::from_str::<crate::handoff::ProjectContext>(&frozen.reference).unwrap(),
        context
    );
}

#[tokio::test]
async fn background_handoff_preparation_uses_no_command_lock_and_stale_results_do_not_release_work()
{
    use crate::handle::ControlHandle;
    let repo = Repo::new();
    let root = tempfile::tempdir().unwrap();
    let h = Harness::with_fleet(
        ControllerConfig {
            worktree_root: Some(root.path().to_owned()),
            ..Default::default()
        },
        vec![],
    );
    let planning = project(&h, &repo).await;
    let item = h
        .file_one(code(
            &repo,
            WorkerKind::Codex,
            "deferred",
            Some(planning.project.id),
        ))
        .await;
    h.ctl
        .hold_context(
            &h.store().work_get(&item).await.unwrap().unwrap(),
            "historical context limit".into(),
        )
        .await
        .unwrap();
    let busy = h.ctl.loop_lock.lock().await;
    let checked = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        h.ctl.prepare_handoff_rechecks(),
    )
    .await
    .expect("history preparation must not acquire the command lock")
    .unwrap();
    drop(busy);
    h.ctl
        .comment(
            &item,
            "Owner direction changed during preparation",
            "user:owner",
        )
        .await
        .unwrap();
    let waiting = h.ctl.waiting_questions().await.unwrap();
    let mut batch = super::super::batch::Batch::new(h.ctl.load().await.unwrap(), h.clock.now());
    assert!(h
        .ctl
        .recover_project_context_holds(&mut batch, &waiting, checked)
        .await
        .unwrap()
        .is_empty());
    assert_eq!(
        h.status(&item).await,
        Status::Blocked(BlockedReason::NeedsDecision)
    );
    h.clock.advance(TimeDelta::seconds(31));
    let fresh = h.ctl.prepare_handoff_rechecks().await.unwrap();
    let mut batch = super::super::batch::Batch::new(h.ctl.load().await.unwrap(), h.clock.now());
    assert_eq!(
        h.ctl
            .recover_project_context_holds(&mut batch, &waiting, fresh)
            .await
            .unwrap(),
        vec![item.clone()]
    );
    h.ctl.commit(batch, &mut Default::default()).await.unwrap();
    assert_eq!(h.status(&item).await, Status::Queued);
}

#[tokio::test]
async fn a_late_saved_answer_prevents_a_prepared_technical_recovery_from_committing() {
    use crate::handle::ControlHandle;
    use rustykrab_store::AnswerSubmissionStatus;
    let repo = Repo::new();
    let root = tempfile::tempdir().unwrap();
    let h = Harness::with_fleet(
        ControllerConfig {
            worktree_root: Some(root.path().to_owned()),
            ..Default::default()
        },
        vec![],
    );
    let planning = project(&h, &repo).await;
    let item = h
        .file_one(code(
            &repo,
            WorkerKind::Codex,
            "late owner answer",
            Some(planning.project.id),
        ))
        .await;
    h.ctl
        .hold_context(
            &h.store().work_get(&item).await.unwrap().unwrap(),
            "historical context limit".into(),
        )
        .await
        .unwrap();
    let checked = h.ctl.prepare_handoff_rechecks().await.unwrap();
    let waiting = h.ctl.waiting_questions().await.unwrap();
    let question = waiting.iter().find(|q| q.item == item).unwrap().id.clone();
    let mut batch = super::super::batch::Batch::new(h.ctl.load().await.unwrap(), h.clock.now());
    assert_eq!(
        h.ctl
            .recover_project_context_holds(&mut batch, &waiting, checked)
            .await
            .unwrap(),
        vec![item.clone()]
    );
    h.ctl
        .submit_answer(
            &question,
            "Use the current project intent",
            "user:owner",
            None,
        )
        .await
        .unwrap();
    assert!(
        h.ctl.commit(batch, &mut Default::default()).await.is_err(),
        "stale recovery must roll back atomically"
    );
    assert_eq!(
        h.status(&item).await,
        Status::Blocked(BlockedReason::NeedsDecision)
    );
    let pending = h.store().question_get(&question).await.unwrap().unwrap();
    assert_eq!(
        pending.status,
        rustykrab_core::questions::QuestionStatus::Open
    );
    assert_eq!(
        pending.answer_submission.unwrap().status,
        AnswerSubmissionStatus::Pending
    );
    {
        let _guard = h.ctl.loop_lock.lock().await;
        h.ctl.apply_pending_answers_locked().await.unwrap();
    }
    let applied = h.store().question_get(&question).await.unwrap().unwrap();
    assert_eq!(
        applied.answer.as_deref(),
        Some("Use the current project intent")
    );
    assert_eq!(
        applied.answer_submission.unwrap().status,
        AnswerSubmissionStatus::Applied
    );
    assert_eq!(h.status(&item).await, Status::Ready);
}

#[tokio::test]
async fn cached_project_history_keeps_each_tasks_own_execution_slice() {
    let repo = Repo::new();
    let h = Harness::with_fleet(ControllerConfig::default(), vec![]);
    let planning = project(&h, &repo).await;
    let first = h
        .file_one(code(
            &repo,
            WorkerKind::Codex,
            "first slice",
            Some(planning.project.id),
        ))
        .await;
    let next = h
        .file_one(code(
            &repo,
            WorkerKind::Codex,
            "second slice",
            Some(planning.project.id),
        ))
        .await;
    let snap = h.ctl.load().await.unwrap();
    let mut cache = std::collections::BTreeMap::new();
    let a = h
        .ctl
        .cached_project_context(&snap, snap.item(&first).unwrap(), &mut cache)
        .await
        .unwrap()
        .unwrap();
    let b = h
        .ctl
        .cached_project_context(&snap, snap.item(&next).unwrap(), &mut cache)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(a.work, b.work);
    assert!(a.execution_items.contains(&first));
    assert!(!a.execution_items.contains(&next));
    assert!(b.execution_items.contains(&next));
    assert!(!b.execution_items.contains(&first));
    assert_eq!(
        Some(b),
        h.ctl
            .project_context(&snap, snap.item(&next).unwrap())
            .await
            .unwrap()
    );
}
