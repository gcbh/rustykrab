//! Real LocalWorker -> controller -> Git recovery and useful original work.
//! The model and alternate executor are deterministic fixtures; no subscription
//! or live daemon is used. Git, worktrees, tools, budgets and verification are real.
use std::{
    path::{Path, PathBuf},
    process::Command,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use async_trait::async_trait;
use chrono::Utc;
use rustykrab_agent::{local_worker::LocalRuns, sandbox::NoSandbox, LocalWorker};
use rustykrab_control::{
    controller::{Controller, ControllerConfig},
    handle::ControlHandle,
    worker::{Brief, Worker, WorkerCapabilities},
    workspace::{self, Workspace},
    Provenance,
};
use rustykrab_core::{
    model::{ModelProvider, ModelResponse, StopReason, Usage},
    types::{Message, MessageContent, Role, ToolCall, ToolSchema},
    work::{ArtifactRef, Evidence, PlanOutcome, ResultReport, Status, WorkItemDraft, WorkerKind},
    Result,
};
use rustykrab_projects::{CreateProject, ProjectId, RevisionAuthor};
use rustykrab_store::Store;
use rustykrab_tools::{work_backend::StubWorkBackend, ExecTool};
use serde_json::{json, Value};

fn git(repo: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
        ])
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

fn commit(repo: &Path, text: &str) -> String {
    std::fs::write(repo.join("policy.txt"), text).unwrap();
    git(repo, &["add", "policy.txt"]);
    git(repo, &["commit", "-m", "fixture"]);
    git(repo, &["rev-parse", "HEAD"])
}

fn call(tool: &str, args: Value) -> ModelResponse {
    ModelResponse {
        message: Message::stamped(
            Role::Assistant,
            MessageContent::ToolCall(ToolCall {
                id: uuid::Uuid::new_v4().to_string(),
                name: tool.into(),
                arguments: args,
            }),
        ),
        usage: Usage {
            prompt_tokens: 90,
            completion_tokens: 10,
            ..Default::default()
        },
        stop_reason: StopReason::ToolUse,
        text: None,
    }
}

struct FixtureModel {
    other_parent: String,
    self_correct: bool,
    repair_calls: AtomicUsize,
    original_calls: AtomicUsize,
}
#[async_trait]
impl ModelProvider for FixtureModel {
    fn name(&self) -> &str {
        "worker-autonomy-fixture"
    }
    async fn chat(&self, messages: &[Message], _: &[ToolSchema]) -> Result<ModelResponse> {
        let repair = messages.iter().any(|m| matches!(&m.content, MessageContent::Text(t) if t.contains("title: Resolve shared Git conflict")));
        let n = if repair {
            self.repair_calls.fetch_add(1, Ordering::SeqCst)
        } else {
            self.original_calls.fetch_add(1, Ordering::SeqCst)
        };
        if repair && !self.self_correct && n == 0 {
            return Ok(call(
                "exec",
                json!({"command": "printf 'unverified partial repair\\n' > partial.txt"}),
            ));
        }
        if repair && (!self.self_correct || n < 4) {
            return Ok(call("exec", json!({"command": "git diff -- policy.txt"})));
        }
        if (repair && n == 4) || (!repair && n == 0) {
            let command = if repair {
                format!("git -c user.name=Fixture -c user.email=fixture@example.invalid merge --no-commit --no-ff {} ; printf 'resolved policy\\n' > policy.txt && git diff --check && git add policy.txt && git -c user.name=Fixture -c user.email=fixture@example.invalid commit -m resolve && git rev-parse HEAD", self.other_parent)
            } else {
                "test \"$(cat policy.txt)\" = 'resolved policy' && printf 'useful result\\n' > result.txt && git add result.txt && git -c user.name=Fixture -c user.email=fixture@example.invalid commit -m original && git rev-parse HEAD".into()
            };
            return Ok(call("exec", json!({"command": command})));
        }
        let commit = messages
            .iter()
            .rev()
            .find_map(|m| match &m.content {
                MessageContent::ToolResult(r) => r
                    .output
                    .get("stdout")
                    .and_then(Value::as_str)
                    .and_then(|s| s.lines().last())
                    .filter(|s| s.len() == 40)
                    .map(str::to_owned),
                _ => None,
            })
            .unwrap_or_else(|| panic!("actual tool result must supply commit; repair={repair}, call={n}, results={:?}", messages.iter().filter(|m| matches!(m.content, MessageContent::ToolResult(_))).collect::<Vec<_>>()));
        Ok(call(
            "result_report",
            json!({
                "summary": if repair {"Resolved both histories in the assigned workspace"} else {"Original useful work completed"},
                "commit": commit,
                "changed_paths": [if repair {"policy.txt"} else {"result.txt"}],
                "checks_run": if repair {vec!["git diff --check"]} else {vec!["test \"$(cat policy.txt)\" = 'resolved policy'"]},
            }),
        ))
    }
}

/// A deterministic alternate Worker, not a claim to have exercised Codex's CLI
/// or reasoning. It receives the normal brief and performs the real Git repair.
struct Alternate {
    repo: PathBuf,
    other: String,
    seen: Mutex<Vec<Brief>>,
}
#[async_trait]
impl Worker for Alternate {
    fn name(&self) -> &str {
        "alternate-codex-fixture"
    }
    fn kind(&self) -> WorkerKind {
        WorkerKind::Codex
    }
    fn capabilities(&self) -> WorkerCapabilities {
        WorkerCapabilities {
            repos: vec![self.repo.display().to_string()],
            writable_resources: vec![format!("repo:{}", self.repo.display())],
            tools: vec!["exec".into(), "result_report".into()],
            ..Default::default()
        }
    }
    async fn run(&self, brief: Brief) -> Result<ResultReport> {
        assert!(brief.title.starts_with("Resolve shared Git conflict"));
        assert!(brief
            .last_error
            .as_ref()
            .is_some_and(|e| e.contains("progress ledger")));
        self.seen.lock().unwrap().push(brief.clone());
        let previous = brief
            .project_context
            .as_ref()
            .unwrap()
            .work
            .iter()
            .find(|w| w.item == brief.item)
            .and_then(|w| w.unfinished_attempt.as_ref())
            .and_then(|attempt| attempt.workspace.as_ref())
            .expect("the successor receives the failed attempt's workspace pointer");
        assert_eq!(
            std::fs::read_to_string(previous.path.join("partial.txt"))
                .expect("failed attempt's uncommitted work must survive handoff"),
            "unverified partial repair\n"
        );
        let other = self.other.clone();
        tokio::task::spawn_blocking(move || {
            let ws = brief.workspace.unwrap();
            ws.create().unwrap();
            let merge = Command::new("git")
                .arg("-C")
                .arg(&ws.path)
                .args([
                    "-c",
                    "user.name=Fixture",
                    "-c",
                    "user.email=fixture@example.invalid",
                    "merge",
                    "--no-commit",
                    "--no-ff",
                    &other,
                ])
                .output()
                .unwrap();
            assert_eq!(merge.status.code(), Some(1));
            std::fs::write(ws.path.join("policy.txt"), "resolved policy\n").unwrap();
            git(&ws.path, &["diff", "--check"]);
            let resolution = commit(&ws.path, "resolved policy\n");
            ws.remove().unwrap();
            Ok(ResultReport {
                summary: "Both parents reconciled".into(),
                commit: Some(resolution),
                changed_paths: vec!["policy.txt".into()],
                checks_run: vec!["git diff --check".into()],
                artifacts: vec![ArtifactRef {
                    kind: rustykrab_control::worker::COMMAND_RUN.into(),
                    value: "git diff --check".into(),
                }],
                ..Default::default()
            })
        })
        .await
        .unwrap()
    }
}

async fn file(ctl: &Controller, draft: WorkItemDraft) -> String {
    match ctl
        .file_draft(
            draft,
            Provenance {
                actor: "user:fixture".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap()
    {
        PlanOutcome::Accepted(a) => a.root,
        other => panic!("filing refused: {other:?}"),
    }
}

async fn exercise(self_correct: bool) {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-b", "main"]);
    let base = commit(&repo, "base policy\n");
    git(&repo, &["checkout", "-b", "other"]);
    let other = commit(&repo, "other policy\n");
    git(&repo, &["checkout", "main"]);
    let current = commit(&repo, "current policy\n");
    let store = Store::open(dir.path().join("store.db"), vec![7; 32]).unwrap();
    let mut project = CreateProject::new(
        "fixture",
        ProjectId::new(),
        "Worker recovery",
        RevisionAuthor::User,
        Utc::now(),
    );
    project.repository_id = Some(repo.display().to_string());
    let project = store
        .projects()
        .create(project)
        .await
        .unwrap()
        .snapshot
        .project
        .id;
    let model = Arc::new(FixtureModel {
        other_parent: other.clone(),
        self_correct,
        repair_calls: AtomicUsize::new(0),
        original_calls: AtomicUsize::new(0),
    });
    let mut definition = LocalWorker::default_definition("planner");
    definition.tools = vec!["exec".into()];
    definition.writable_resources = vec![format!("repo:{}", repo.display())];
    let local = Arc::new(
        LocalWorker::new(
            "planner",
            definition,
            model.clone(),
            vec![Arc::new(ExecTool::in_dir(repo.clone()))],
            Arc::new(NoSandbox),
            Arc::new(StubWorkBackend::new()),
        )
        .with_runs(Arc::new(LocalRuns::default()))
        .with_stall_steps(2),
    );
    let alternate = Arc::new(Alternate {
        repo: repo.clone(),
        other: other.clone(),
        seen: Mutex::new(vec![]),
    });
    let config = ControllerConfig {
        worktree_root: Some(dir.path().join("worktrees")),
        ..Default::default()
    };
    let ctl = Controller::new(store.clone(), vec![local, alternate.clone()], config);
    let draft = |title: &str| WorkItemDraft {
        title: title.into(),
        objective: "Complete the assigned operation".into(),
        done_when: "Actual work is Git verified".into(),
        writable_resources: vec![format!("repo:{}", repo.display())],
        artifact_refs: vec![ArtifactRef {
            kind: "project".into(),
            value: project.to_string(),
        }],
        ..Default::default()
    };
    let historical = file(&ctl, draft("historical-branch")).await;
    store
        .work_transition(
            &historical,
            None,
            Status::Done,
            "controller",
            Some("fixture historical commit"),
            None,
            None,
            None,
        )
        .await
        .unwrap();
    let old_ws = Workspace {
        repo: repo.clone(),
        base,
        branch: "other".into(),
        path: dir.path().join("historical"),
    };
    for (kind, reference, verifier) in [
        ("commit", other.clone(), Some("git".into())),
        ("workspace", serde_json::to_string(&old_ws).unwrap(), None),
    ] {
        store
            .work_evidence_add(Evidence {
                item: historical.clone(),
                kind: kind.into(),
                reference,
                hash: (kind == "workspace").then(|| old_ws.base.clone()),
                verified_by: verifier,
                at: Utc::now(),
            })
            .await
            .unwrap();
    }
    let original = file(&ctl, draft("original-useful-work")).await;
    tokio::time::timeout(Duration::from_secs(25), async {
        loop {
            ctl.tick().await.unwrap();
            ctl.wait_for_runs(Duration::from_millis(50)).await;
            if store.work_get(&original).await.unwrap().unwrap().status == Status::Done {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("original work must resume after real repair or handoff");
    let repairs = store.work_evidence_of_kind("merge_recovery").await.unwrap();
    assert_eq!(
        repairs.len(),
        1,
        "one durable repair, no replacement budget"
    );
    let repair: Value = serde_json::from_str(&repairs[0].reference).unwrap();
    let repair_id = repair["repair"].as_str().unwrap();
    assert_eq!(
        store.work_get(repair_id).await.unwrap().unwrap().status,
        Status::Done
    );
    let evidence = store.work_evidence_list(repair_id).await.unwrap();
    let resolved = &evidence
        .iter()
        .find(|e| e.kind == "commit" && e.verified_by.as_deref() == Some("git"))
        .unwrap()
        .reference;
    assert!(workspace::is_ancestor(&repo, &current, resolved).unwrap());
    assert!(workspace::is_ancestor(&repo, &other, resolved).unwrap());
    let outcome = store.work_evidence_list(&original).await.unwrap();
    let final_commit = &outcome
        .iter()
        .find(|e| e.kind == "commit" && e.verified_by.as_deref() == Some("git"))
        .unwrap()
        .reference;
    assert_eq!(
        git(&repo, &["show", &format!("{final_commit}:result.txt")]),
        "useful result"
    );
    assert_eq!(
        workspace::head(&repo).unwrap(),
        current,
        "main was never moved"
    );
    let switches = alternate.seen.lock().unwrap().clone();
    assert_eq!(switches.len(), usize::from(!self_correct));
    if let Some(brief) = switches.first() {
        assert_eq!(brief.budget.tokens, 0);
        assert_eq!(brief.budget.iterations, 0);
        assert_eq!(brief.budget.wall_seconds, 0);
        assert!(
            store.work_spend_of(repair_id).await.unwrap().tokens >= 600,
            "handoff retains observed usage without withholding execution"
        );
    }
    assert!(
        store
            .questions_list(&Default::default())
            .await
            .unwrap()
            .is_empty(),
        "no owner intervention or budget grant"
    );
    assert_eq!(model.repair_calls.load(Ordering::SeqCst), 6);
}

#[tokio::test]
async fn worker_autonomy_self_corrects_then_merges_and_completes_original_work() {
    exercise(true).await;
}

#[tokio::test]
async fn worker_autonomy_stall_hands_off_real_repair_then_original_worker_resumes() {
    exercise(false).await;
}
