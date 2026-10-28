//! Actual Git regression: a verified resolution must be considered before the
//! historical branches it reconciles, regardless of work-item ID ordering.
use std::{fs, path::Path, process::Command};

use crate::workspace::{self, CodeClaim, CodeVerdict, Workspace};

fn git(repo: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(["-c", "core.hooksPath=/dev/null", "-C"])
        .arg(repo)
        .args(args)
        .env("GIT_AUTHOR_NAME", "Recovery fixture")
        .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
        .env("GIT_COMMITTER_NAME", "Recovery fixture")
        .env("GIT_COMMITTER_EMAIL", "fixture@example.invalid")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn commit(repo: &Path, text: &str) -> String {
    fs::write(repo.join("permissions.txt"), text).unwrap();
    git(repo, &["add", "permissions.txt"]);
    git(repo, &["commit", "-m", "fixture permissions"]);
    git(repo, &["rev-parse", "HEAD"])
}

#[test]
fn completed_resolution_prevents_replaying_a_shared_historical_conflict() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-b", "main"]);
    let ancestor = commit(&repo, "restricted: cargo, git status\n");
    git(&repo, &["checkout", "-b", "old-tool-fix"]);
    let old = commit(&repo, "restricted: cargo, node, git restore\n");
    git(&repo, &["checkout", "main"]);
    let current = commit(&repo, "default working tools: Bash, Read, Edit\n");
    assert!(workspace::is_ancestor(&repo, &ancestor, &old).unwrap());
    assert!(workspace::continuation_base(&repo, &current, std::slice::from_ref(&old)).is_err());

    // A repair can start from a known parent without first combining the
    // conflicting histories. It neither moves main nor edits its checkout.
    let ws = Workspace::plan(
        &dir.path().join("worktrees"),
        &repo,
        &current,
        "repair",
        "attempt1",
    );
    ws.create().unwrap();
    let merged = Command::new("git")
        .arg("-C")
        .arg(&ws.path)
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "merge",
            "--no-commit",
            "--no-ff",
            &old,
        ])
        .env("GIT_AUTHOR_NAME", "Recovery fixture")
        .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
        .env("GIT_COMMITTER_NAME", "Recovery fixture")
        .env("GIT_COMMITTER_EMAIL", "fixture@example.invalid")
        .output()
        .unwrap();
    assert_eq!(merged.status.code(), Some(1));
    let resolved = commit(&ws.path, "default working tools: Bash, Read, Edit\n");
    assert!(workspace::is_ancestor(&repo, &current, &resolved).unwrap());
    assert!(workspace::is_ancestor(&repo, &old, &resolved).unwrap());
    assert!(matches!(
        workspace::verify(
            &ws,
            CodeClaim {
                commit: Some(&resolved),
                changed_paths: &[]
            }
        )
        .unwrap(),
        CodeVerdict::Verified { .. }
    ));
    assert_eq!(workspace::head(&repo).unwrap(), current);
    assert_eq!(
        fs::read_to_string(repo.join("permissions.txt")).unwrap(),
        "default working tools: Bash, Read, Edit\n"
    );

    // Work rows are ordered by IDs, not ancestry. A later resolution must
    // prevent the same early historical conflict for every future waiter.
    for candidates in [
        vec![old.clone(), resolved.clone()],
        vec![resolved.clone(), old.clone()],
    ] {
        assert_eq!(
            workspace::continuation_base(&repo, &current, &candidates).unwrap(),
            resolved
        );
    }
}

use super::{draft, Harness};
use crate::controller::{Controller, ControllerConfig};
use crate::handle::ControlHandle;
use crate::worker::{Brief, Worker, WorkerCapabilities};
use async_trait::async_trait;
use chrono::{TimeDelta, Utc};
use rustykrab_core::{
    work::{ArtifactRef, BlockedReason, Evidence, ResultReport, Status, WorkerKind},
    Error,
};
use rustykrab_projects::{CreateProject, ProjectId, RevisionAuthor};
use std::sync::{Arc, Mutex};

#[derive(Clone, Copy)]
enum RepairMode {
    Resolve,
    OneParent,
    Noop,
}
struct RepairWorker {
    repo: String,
    parents: [String; 2],
    mode: RepairMode,
    seen: Mutex<Vec<Brief>>,
}
#[async_trait]
impl Worker for RepairWorker {
    fn name(&self) -> &str {
        "git-worker"
    }
    fn kind(&self) -> WorkerKind {
        WorkerKind::Codex
    }
    fn capabilities(&self) -> WorkerCapabilities {
        WorkerCapabilities {
            repos: vec![self.repo.clone()],
            writable_resources: vec![format!("repo:{}", self.repo)],
            ..Default::default()
        }
    }
    async fn run(&self, brief: Brief) -> Result<ResultReport, Error> {
        self.seen.lock().unwrap().push(brief.clone());
        let parents = self.parents.clone();
        let mode = self.mode;
        tokio::task::spawn_blocking(move || {
            let ws = brief
                .workspace
                .as_ref()
                .expect("repair and originals need isolated worktrees");
            ws.create().unwrap();
            let report = if brief.title.starts_with("Resolve shared Git conflict") {
                assert!(parents.contains(&ws.base));
                match mode {
                    RepairMode::Noop => ResultReport {
                        summary: "Claimed resolution without doing it".into(),
                        ..Default::default()
                    },
                    RepairMode::OneParent => {
                        let commit = commit(&ws.path, "one-parent edit\n");
                        ResultReport {
                            summary: "Claimed resolution missing the other history".into(),
                            commit: Some(commit),
                            changed_paths: vec!["permissions.txt".into()],
                            ..Default::default()
                        }
                    }
                    RepairMode::Resolve => {
                        let other = parents.iter().find(|p| **p != ws.base).unwrap();
                        let out = Command::new("git")
                            .arg("-C")
                            .arg(&ws.path)
                            .args([
                                "-c",
                                "core.hooksPath=/dev/null",
                                "merge",
                                "--no-commit",
                                "--no-ff",
                                other,
                            ])
                            .env("GIT_AUTHOR_NAME", "Recovery fixture")
                            .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
                            .env("GIT_COMMITTER_NAME", "Recovery fixture")
                            .env("GIT_COMMITTER_EMAIL", "fixture@example.invalid")
                            .output()
                            .unwrap();
                        assert_eq!(out.status.code(), Some(1));
                        let commit = commit(&ws.path, "resolved project policy\n");
                        ResultReport {
                            summary: "Resolved the actual conflict retaining both parents".into(),
                            commit: Some(commit),
                            changed_paths: vec!["permissions.txt".into()],
                            ..Default::default()
                        }
                    }
                }
            } else {
                assert_eq!(
                    fs::read_to_string(ws.path.join("permissions.txt")).unwrap(),
                    "resolved project policy\n"
                );
                let path = format!("{}.txt", brief.title);
                fs::write(ws.path.join(&path), "original useful work completed").unwrap();
                git(&ws.path, &["add", &path]);
                git(&ws.path, &["commit", "-m", "complete original work"]);
                ResultReport {
                    summary: "Original work resumed on the verified repair".into(),
                    commit: Some(git(&ws.path, &["rev-parse", "HEAD"])),
                    changed_paths: vec![path],
                    ..Default::default()
                }
            };
            ws.remove().unwrap();
            Ok(report)
        })
        .await
        .unwrap()
    }
}
struct Fixture {
    _dir: tempfile::TempDir,
    repo: std::path::PathBuf,
    current: String,
    h: Harness,
    worker: Arc<RepairWorker>,
    project: ProjectId,
}
impl Fixture {
    async fn new(mode: RepairMode) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "-b", "main"]);
        commit(&repo, "original policy\n");
        git(&repo, &["checkout", "-b", "old-change"]);
        let old = commit(&repo, "old tool change\n");
        git(&repo, &["checkout", "main"]);
        let current = commit(&repo, "new tool policy\n");
        let worker = Arc::new(RepairWorker {
            repo: repo.display().to_string(),
            parents: [old.clone(), current.clone()],
            mode,
            seen: Mutex::new(vec![]),
        });
        let h = Harness::with_fleet(
            ControllerConfig {
                worktree_root: Some(dir.path().join("worktrees")),
                default_budget: super::no_rungs(),
                ..Default::default()
            },
            vec![worker.clone()],
        );
        let mut create = CreateProject::new(
            "fixture",
            ProjectId::new(),
            "Git recovery project",
            RevisionAuthor::User,
            Utc::now(),
        );
        create.repository_id = Some(repo.display().to_string());
        let project = h
            .store()
            .projects()
            .create(create)
            .await
            .unwrap()
            .snapshot
            .project
            .id;
        let mut f = Self {
            _dir: dir,
            repo,
            current,
            h,
            worker,
            project,
        };
        let history = f.original("verified-history").await;
        // Host-owned fixture setup: an earlier, real Git-verified branch is
        // complete. All allocation, dispatch, repair and resumption below run
        // through the actual production controller and ordinary Git verifier.
        f.h.store()
            .work_transition(
                &history,
                None,
                Status::Done,
                "controller",
                Some("fixture verified historical commit"),
                None,
                None,
                None,
            )
            .await
            .unwrap();
        f.h.store()
            .work_evidence_add(Evidence {
                item: history.clone(),
                kind: "commit".into(),
                reference: old,
                hash: None,
                verified_by: Some("git".into()),
                at: Utc::now(),
            })
            .await
            .unwrap();
        let historical_ws = Workspace {
            repo: f.repo.clone(),
            base: git(&f.repo, &["merge-base", &f.worker.parents[0], &f.current]),
            branch: "old-change".into(),
            path: f._dir.path().join("historical-worktree"),
        };
        f.h.store()
            .work_evidence_add(Evidence {
                item: history.clone(),
                kind: "workspace".into(),
                reference: serde_json::to_string(&historical_ws).unwrap(),
                hash: Some(historical_ws.base.clone()),
                verified_by: None,
                at: Utc::now(),
            })
            .await
            .unwrap();
        f
    }
    async fn original(&mut self, title: &str) -> String {
        let mut d = draft(title, title);
        d.writable_resources = vec![format!("repo:{}", self.repo.display())];
        d.artifact_refs = vec![ArtifactRef {
            kind: "project".into(),
            value: self.project.to_string(),
        }];
        self.h.file_one(d).await
    }
    async fn repairs(&self) -> Vec<String> {
        self.h
            .store()
            .work_evidence_of_kind(super::super::merge_recovery::MERGE_RECOVERY)
            .await
            .unwrap()
            .into_iter()
            .filter(|e| e.verified_by.as_deref() == Some("controller"))
            .map(|e| {
                serde_json::from_str::<super::super::merge_recovery::Recovery>(&e.reference)
                    .unwrap()
                    .repair
            })
            .collect()
    }
}

#[tokio::test]
async fn one_shared_repair_receives_a_real_worktree_and_originals_resume_across_restart() {
    let mut f = Fixture::new(RepairMode::Resolve).await;
    let first = f.original("first-original").await;
    let second = f.original("second-original").await;
    f.h.tick().await;
    let repairs = f.repairs().await;
    assert_eq!(repairs.len(), 1);
    assert!(
        f.worker.seen.lock().unwrap().is_empty(),
        "preflight must not send an original into the conflict"
    );
    f.h.ctl = Controller::new(
        f.h.store().clone(),
        vec![f.worker.clone()],
        f.h.config.clone(),
    )
    .with_clock(f.h.clock.clone());
    f.h.drain_settled().await;
    assert_eq!(f.h.status(&repairs[0]).await, Status::Done);
    assert_eq!(f.h.status(&first).await, Status::Done);
    assert_eq!(f.h.status(&second).await, Status::Done);
    assert_eq!(f.repairs().await, repairs);
    assert_eq!(workspace::head(&f.repo).unwrap(), f.current);
    assert_eq!(
        fs::read_to_string(f.repo.join("permissions.txt")).unwrap(),
        "new tool policy\n"
    );
    assert!(f
        .h
        .store()
        .questions_list(&Default::default())
        .await
        .unwrap()
        .is_empty());
    {
        let briefs = f.worker.seen.lock().unwrap();
        assert_eq!(briefs.len(), 3);
        assert!(briefs[0].title.starts_with("Resolve shared Git conflict"));
    }
    let evidence = f.h.store().work_evidence_list(&repairs[0]).await.unwrap();
    let resolved = &evidence
        .iter()
        .find(|e| e.kind == "commit" && e.verified_by.as_deref() == Some("git"))
        .unwrap()
        .reference;
    for parent in &f.worker.parents {
        assert!(workspace::is_ancestor(&f.repo, parent, resolved).unwrap());
    }
}

#[tokio::test]
async fn one_parent_and_noop_claims_fail_and_do_not_allocate_a_fresh_shared_repair() {
    for mode in [RepairMode::OneParent, RepairMode::Noop] {
        let mut f = Fixture::new(mode).await;
        let original = f.original("original").await;
        f.h.drain_settled().await;
        let repairs = f.repairs().await;
        assert_eq!(repairs.len(), 1);
        assert_ne!(f.h.status(&repairs[0]).await, Status::Done);
        assert_ne!(f.h.status(&original).await, Status::Done);
        f.h.ctl = Controller::new(
            f.h.store().clone(),
            vec![f.worker.clone()],
            f.h.config.clone(),
        )
        .with_clock(f.h.clock.clone());
        let later = f.original("later-original").await;
        for _ in 0..5 {
            f.h.tick().await;
        }
        assert_eq!(f.repairs().await, repairs);
        assert_ne!(f.h.status(&later).await, Status::Done);
        assert!(f
            .worker
            .seen
            .lock()
            .unwrap()
            .iter()
            .all(|b| b.title.starts_with("Resolve shared Git conflict")));
    }
}

#[tokio::test]
async fn legacy_technical_questions_become_owned_recovery_without_an_invented_answer() {
    let mut f = Fixture::new(RepairMode::Resolve).await;
    let original = f.original("legacy-original").await;
    let row = f.h.store().work_get(&original).await.unwrap().unwrap();
    f.h.ctl
        .hold_context(&row, "legacy shared project conflict".into())
        .await
        .unwrap();
    f.h.drain_settled().await;
    assert_eq!(f.h.status(&original).await, Status::Done);
    assert_eq!(f.repairs().await.len(), 1);
    let questions =
        f.h.store()
            .questions_list(&Default::default())
            .await
            .unwrap();
    let q = questions.iter().find(|q| q.item == original).unwrap();
    assert_eq!(
        q.status,
        rustykrab_core::questions::QuestionStatus::Obsolete
    );
    assert!(q.answer.is_none());
}

#[tokio::test]
async fn shared_recovery_preserves_independent_consent_and_original_budget_holds() {
    let mut f = Fixture::new(RepairMode::Resolve).await;
    let protected = f.original("owner-held").await;
    let allowed = f.original("allowed-original").await;
    let spent = f.original("spent-original").await;
    let row = f.h.store().work_get(&protected).await.unwrap().unwrap();
    f.h.ctl
        .hold_context(&row, "legacy conflict".into())
        .await
        .unwrap();
    let mut batch = super::super::batch::Batch::new(f.h.ctl.load().await.unwrap(), Utc::now());
    let question =
        f.h.ctl
            .record_question(
                &mut batch,
                super::super::questions::NewQuestion::open(
                    &protected,
                    rustykrab_core::questions::QuestionKind::Consent,
                    "May this publish externally?".into(),
                    "independent_consent",
                ),
            )
            .await
            .unwrap();
    f.h.ctl
        .commit(batch, &mut Default::default())
        .await
        .unwrap();
    f.h.store()
        .work_transition(
            &spent,
            None,
            Status::Blocked(BlockedReason::BudgetExhausted),
            "controller",
            Some("fixture existing spent budget"),
            None,
            None,
            None,
        )
        .await
        .unwrap();
    f.h.drain_settled().await;
    assert_eq!(f.h.status(&allowed).await, Status::Done);
    assert_eq!(
        f.h.status(&protected).await,
        Status::Blocked(BlockedReason::NeedsDecision)
    );
    assert_eq!(
        f.h.status(&spent).await,
        Status::Blocked(BlockedReason::BudgetExhausted)
    );
    let questions =
        f.h.store()
            .questions_list(&Default::default())
            .await
            .unwrap();
    let q = questions.iter().find(|q| q.id == question).unwrap();
    assert_eq!(q.status, rustykrab_core::questions::QuestionStatus::Open);
    assert!(q.answer.is_none());
    assert!(!f
        .worker
        .seen
        .lock()
        .unwrap()
        .iter()
        .any(|b| b.item == protected || b.item == spent));
    assert!(f
        .h
        .store()
        .work_evidence_of_kind("budget_grant")
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn prepared_conflict_does_not_allocate_after_owner_direction_or_a_pending_answer_changes() {
    for pending in [false, true] {
        let mut f = Fixture::new(RepairMode::Resolve).await;
        let original = f.original("stale-original").await;
        let row = f.h.store().work_get(&original).await.unwrap().unwrap();
        f.h.ctl
            .hold_context(&row, "legacy conflict".into())
            .await
            .unwrap();
        let checks = f.h.ctl.prepare_handoff_rechecks().await.unwrap();
        if pending {
            let q =
                f.h.store()
                    .questions_list(&Default::default())
                    .await
                    .unwrap()
                    .into_iter()
                    .find(|q| q.item == original)
                    .unwrap();
            f.h.ctl
                .submit_answer(
                    &q.id,
                    "Use a fresh checkout",
                    "user:owner",
                    Some(&uuid::Uuid::new_v4().to_string()),
                )
                .await
                .unwrap();
        } else {
            f.h.ctl
                .comment(
                    &original,
                    "Owner changed the requested approach",
                    "user:owner",
                )
                .await
                .unwrap();
        }
        let waiting = f.h.ctl.waiting_questions().await.unwrap();
        let mut batch = super::super::batch::Batch::new(f.h.ctl.load().await.unwrap(), Utc::now());
        assert!(f
            .h
            .ctl
            .recover_project_context_holds(&mut batch, &waiting, checks)
            .await
            .unwrap()
            .is_empty());
        assert!(f.repairs().await.is_empty());
        assert!(batch.evidence.is_empty());
        assert_eq!(
            f.h.status(&original).await,
            Status::Blocked(BlockedReason::NeedsDecision)
        );
    }
}

#[tokio::test]
async fn archived_verified_repair_continues_to_prevent_the_same_conflict() {
    let mut f = Fixture::new(RepairMode::Resolve).await;
    let original = f.original("before-archive").await;
    f.h.drain_settled().await;
    assert_eq!(f.h.status(&original).await, Status::Done);
    let repairs = f.repairs().await;
    assert_eq!(repairs.len(), 1);
    f.h.store()
        .work_archive_compact(&repairs, Utc::now() + TimeDelta::days(8))
        .await
        .unwrap();
    assert!(f.h.store().work_get(&repairs[0]).await.unwrap().is_none());
    let reopened = rustykrab_store::Store::open(&f.h.dir.0, vec![9; 32]).unwrap();
    f.h.ctl = Controller::new(reopened.clone(), vec![f.worker.clone()], f.h.config.clone())
        .with_clock(f.h.clock.clone());
    f.h.store = reopened;
    let later = f.original("after-archive").await;
    f.h.drain_settled().await;
    assert_eq!(f.h.status(&later).await, Status::Done);
    assert_eq!(f.repairs().await, repairs);
    assert_eq!(workspace::head(&f.repo).unwrap(), f.current);
}

#[tokio::test]
async fn model_authored_recovery_markers_cannot_select_a_bypass_workspace() {
    let mut f = Fixture::new(RepairMode::Resolve).await;
    let original = f.original("forged-marker").await;
    let row = f.h.store().work_get(&original).await.unwrap().unwrap();
    let mut ctx =
        f.h.ctl
            .project_context(&f.h.ctl.load().await.unwrap(), &row)
            .await
            .unwrap();
    let error =
        f.h.ctl
            .plan_workspace(&row, "check", ctx.as_mut())
            .await
            .unwrap_err();
    let workspace::ContinuationError::Conflict(conflict) = error else {
        panic!("expected typed conflict")
    };
    let fake = super::super::merge_recovery::Recovery {
        repair: original.clone(),
        project: f.project.to_string(),
        conflict,
    };
    f.h.store()
        .work_evidence_add(Evidence {
            item: original.clone(),
            kind: super::super::merge_recovery::MERGE_RECOVERY.into(),
            reference: serde_json::to_string(&fake).unwrap(),
            hash: None,
            verified_by: Some("result_report".into()),
            at: Utc::now(),
        })
        .await
        .unwrap();
    assert!(f.h.ctl.merge_recovery(&row).await.unwrap().is_none());
    assert!(matches!(
        f.h.ctl
            .plan_workspace(&row, "check-again", ctx.as_mut())
            .await,
        Err(workspace::ContinuationError::Conflict(_))
    ));
    let mut batch = super::super::batch::Batch::new(f.h.ctl.load().await.unwrap(), Utc::now());
    let artifacts = vec![ArtifactRef {
        kind: super::super::merge_recovery::MERGE_RECOVERY.into(),
        value: serde_json::to_string(&fake).unwrap(),
    }];
    assert!(super::super::breaker::reported_artifacts(
        &mut batch,
        &original,
        "malicious-worker",
        &artifacts
    )
    .is_empty());
}

#[tokio::test]
async fn cancelling_a_shared_repair_preserves_original_goals_and_does_not_buy_another_repair() {
    let mut f = Fixture::new(RepairMode::Resolve).await;
    let original = f.original("preserved-original").await;
    f.h.tick().await;
    let repairs = f.repairs().await;
    assert_eq!(repairs.len(), 1);
    f.h.ctl
        .cancel(
            &repairs[0],
            Some("Stop this repair attempt".into()),
            "user:owner",
        )
        .await
        .unwrap();
    assert!(!f.h.status(&original).await.is_closed());
    for _ in 0..3 {
        f.h.tick().await;
    }
    assert_eq!(
        f.h.status(&original).await,
        Status::Blocked(BlockedReason::NeedsDecision)
    );
    assert_eq!(f.repairs().await, repairs);
    assert!(f.worker.seen.lock().unwrap().is_empty());
    let q =
        f.h.store()
            .questions_list(&Default::default())
            .await
            .unwrap();
    assert!(q
        .iter()
        .any(|q| q.item == original && q.text.contains(&repairs[0])));
}

#[tokio::test]
async fn exhausted_shared_repair_keeps_one_budget_across_restart_and_new_waiters() {
    let mut f = Fixture::new(RepairMode::Resolve).await;
    let original = f.original("first-waiter").await;
    f.h.tick().await;
    let repairs = f.repairs().await;
    let repair = &repairs[0];
    f.h.store()
        .work_spend_record(rustykrab_store::RunSpend {
            item: repair.clone(),
            run: Some("prior-exhausted-run".into()),
            worker: "git-worker".into(),
            tokens: 10_000,
            wall_ms: 0,
            iterations: 5,
            at: Utc::now(),
        })
        .await
        .unwrap();
    // Spend is loaded at controller startup (normal completed runs also
    // update its cache); reopen after the host-owned historical setup.
    f.h.ctl = Controller::new(
        f.h.store().clone(),
        vec![f.worker.clone()],
        f.h.config.clone(),
    )
    .with_clock(f.h.clock.clone());
    f.h.tick().await;
    assert_eq!(
        f.h.status(repair).await,
        Status::Blocked(BlockedReason::BudgetExhausted)
    );
    f.h.ctl = Controller::new(
        f.h.store().clone(),
        vec![f.worker.clone()],
        f.h.config.clone(),
    )
    .with_clock(f.h.clock.clone());
    let later = f.original("second-waiter").await;
    for _ in 0..5 {
        f.h.tick().await;
    }
    assert_eq!(f.repairs().await, repairs);
    assert!(!f.h.status(&original).await.is_closed());
    assert!(!f.h.status(&later).await.is_closed());
    assert!(f.worker.seen.lock().unwrap().is_empty());
    assert_eq!(
        f.h.store().work_spend_of(repair).await.unwrap().tokens,
        10_000
    );
    assert!(f
        .h
        .store()
        .work_evidence_of_kind("budget_grant")
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn repair_allocation_honors_existing_discovered_work_approval_policy() {
    let mut f = Fixture::new(RepairMode::Resolve).await;
    f.h.config.approval.hold_discovered = true;
    f.h.ctl = Controller::new(
        f.h.store().clone(),
        vec![f.worker.clone()],
        f.h.config.clone(),
    )
    .with_clock(f.h.clock.clone());
    let original = f.original("approval-gated").await;
    f.h.tick().await;
    let repairs = f.repairs().await;
    assert_eq!(repairs.len(), 1);
    let repair = f.h.store().work_get(&repairs[0]).await.unwrap().unwrap();
    assert!(repair.held_by.is_some());
    for _ in 0..3 {
        f.h.tick().await;
    }
    assert!(f.worker.seen.lock().unwrap().is_empty());
    assert!(!f.h.status(&original).await.is_closed());
}

#[tokio::test]
async fn changed_project_revision_invalidates_a_prepared_conflict_allocation() {
    use rustykrab_projects::{
        PlanChangeSet, ProjectPatch, Provenance, ProvenanceClassification, ProvenanceSource,
    };
    let mut f = Fixture::new(RepairMode::Resolve).await;
    let original = f.original("revision-gated").await;
    let row = f.h.store().work_get(&original).await.unwrap().unwrap();
    f.h.ctl
        .hold_context(&row, "legacy conflict".into())
        .await
        .unwrap();
    let checks = f.h.ctl.prepare_handoff_rechecks().await.unwrap();
    let old =
        f.h.store()
            .projects()
            .get(&f.project)
            .await
            .unwrap()
            .unwrap();
    let change = PlanChangeSet::new(
        "owner-revision",
        f.project,
        old.revision.id.clone(),
        "Changed owner intent",
        RevisionAuthor::User,
        Utc::now(),
        vec![],
    )
    .with_project_patch(ProjectPatch {
        title: Some("Updated owner project".into()),
        provenance: vec![Provenance {
            classification: ProvenanceClassification::UserStated,
            source: ProvenanceSource::Manual {
                reference: "owner correction".into(),
            },
            recorded_at: Utc::now(),
            confidence: None,
            freshness: None,
        }],
        ..Default::default()
    });
    f.h.store()
        .projects()
        .apply(&f.project, &old.revision.id, change)
        .await
        .unwrap();
    let waiting = f.h.ctl.waiting_questions().await.unwrap();
    let mut batch = super::super::batch::Batch::new(f.h.ctl.load().await.unwrap(), Utc::now());
    assert!(f
        .h
        .ctl
        .recover_project_context_holds(&mut batch, &waiting, checks)
        .await
        .unwrap()
        .is_empty());
    assert!(f.repairs().await.is_empty());
    assert!(batch.evidence.is_empty());
}

#[tokio::test]
async fn repair_brief_carries_owner_direction_from_its_actual_waiters() {
    let mut f = Fixture::new(RepairMode::Resolve).await;
    let first = f.original("first-owner-goal").await;
    let second = f.original("second-owner-goal").await;
    for (item, answer) in [
        (&first, "Create a fresh worktree; retain the original goal"),
        (
            &second,
            "Preserve the current permission policy while resolving history",
        ),
    ] {
        let row = f.h.store().work_get(item).await.unwrap().unwrap();
        f.h.ctl
            .hold_context(&row, "legacy project conflict".into())
            .await
            .unwrap();
        let q =
            f.h.store()
                .questions_list(&Default::default())
                .await
                .unwrap()
                .into_iter()
                .find(|q| q.item == *item)
                .unwrap();
        f.h.ctl.answer(&q.id, answer, "user:owner").await.unwrap();
    }
    f.h.drain_settled().await;
    let briefs = f.worker.seen.lock().unwrap();
    let repair = briefs
        .iter()
        .find(|b| b.title.starts_with("Resolve shared Git conflict"))
        .unwrap();
    let view = repair.project_context.as_ref().unwrap().execution_view();
    for item in [&first, &second] {
        assert!(
            view.work.iter().any(|w| w.item == *item),
            "repair must receive each actual waiter's goal"
        );
        assert!(
            view.questions
                .iter()
                .any(|q| q.item == *item && q.answer.is_some()),
            "repair must receive saved owner direction"
        );
    }
}
