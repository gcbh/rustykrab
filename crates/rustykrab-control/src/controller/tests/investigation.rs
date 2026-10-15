//! The investigation slice against a real store and real git: a fix item
//! that lands nothing is not `done`, the failure's history run by run, the
//! bundle a fix item is leased with, the commit its worktree is pinned to,
//! and what a second fix item is told when the failure comes back.
//!
//! The controller here runs on the wall clock, as the daemon's does (see
//! `Harness::on_wall_clock`).

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use rustykrab_core::questions::QuestionStatus;
use rustykrab_core::work::{
    ArtifactRef, BlockedReason, EdgeKind, ErrorClass, ErrorSubclass, EventKind, ResultReport, Rung,
    Status, WorkItem, WorkItemDraft, WorkKind, WorkerKind,
};
use rustykrab_core::Error;
use rustykrab_store::QuestionFilter;

use super::{draft, Harness};
use crate::controller::ControllerConfig;
use crate::errors::{CLASSIFIER_RULE, LANDING_RULE};
use crate::handle::ControlHandle;
use crate::investigate::bundle::Bundle;
use crate::investigate::{
    BuildId, FailureHistory, Inclusion, Matched, DAEMON_BUILD, FAILURE_FINGERPRINT,
    INVESTIGATION_BUNDLE, PREVIOUS_FIX,
};
use crate::worker::{Brief, Worker, WorkerCapabilities, RUN_LOG};
use crate::workspace::tests::{commit_all, run_git, Repo};
use crate::workspace::REPO_PREFIX;

/// A fake token in the shape of a real one.
const FAKE_TOKEN: &str = "7000000001:AAFakeFakeFakeFakeFakeFakeFakeFake_0";

/// What a run of [`Agent`] does.
#[derive(Debug, Clone)]
enum Act {
    /// Commit a change and claim a path it did not touch: the controller's
    /// own verification fails it as `verification/claim_mismatch`.
    ClaimWrongPath,
    /// Commit a change and claim it honestly.
    Commit(&'static str),
    /// Report this summary and change nothing.
    Nothing(&'static str),
    /// Fail with an error no classifier recognises.
    Unrecognised(&'static str),
    /// Report this summary with a classifier rule and change nothing else.
    Rule(&'static str, &'static str),
}

/// A worker that runs in the worktree it is given, the way the adapters in
/// `rustykrab-agent` do, and leaves a session log for every run. A failed
/// run keeps its worktree. What it does is scripted by the start of the
/// item's title; with nothing scripted it commits honestly.
struct Agent {
    name: &'static str,
    repo: Option<String>,
    logs: PathBuf,
    script: Mutex<Vec<(String, VecDeque<Act>)>>,
    briefs: Mutex<Vec<Brief>>,
    log_of: Mutex<HashMap<String, PathBuf>>,
}

impl Agent {
    fn new(name: &'static str, repo: Option<&Repo>, logs: PathBuf) -> Arc<Agent> {
        Arc::new(Agent {
            name,
            repo: repo.map(|r| r.path().display().to_string()),
            logs,
            script: Mutex::new(Vec::new()),
            briefs: Mutex::new(Vec::new()),
            log_of: Mutex::new(HashMap::new()),
        })
    }

    /// Queue `acts` for items whose title starts with `title`.
    fn on(&self, title: &str, acts: &[Act]) {
        let mut script = self.script.lock().unwrap();
        match script.iter_mut().find(|(t, _)| t == title) {
            Some((_, queue)) => queue.extend(acts.iter().cloned()),
            None => script.push((title.to_string(), acts.iter().cloned().collect())),
        }
    }

    fn next(&self, title: &str) -> Act {
        let mut script = self.script.lock().unwrap();
        script
            .iter_mut()
            .filter(|(t, _)| title.starts_with(t.as_str()))
            .find_map(|(_, queue)| queue.pop_front())
            .unwrap_or(Act::Commit("made the change"))
    }

    fn briefs_for(&self, title: &str) -> Vec<Brief> {
        self.briefs
            .lock()
            .unwrap()
            .iter()
            .filter(|b| b.title.starts_with(title))
            .cloned()
            .collect()
    }
}

#[async_trait]
impl Worker for Agent {
    fn name(&self) -> &str {
        self.name
    }

    fn kind(&self) -> WorkerKind {
        WorkerKind::ClaudeCode
    }

    fn capabilities(&self) -> WorkerCapabilities {
        WorkerCapabilities {
            tools: vec!["*".to_string()],
            repos: self.repo.iter().cloned().collect(),
            writable_resources: match &self.repo {
                Some(repo) => vec![format!("{REPO_PREFIX}{repo}")],
                None => vec!["*".to_string()],
            },
            ..WorkerCapabilities::default()
        }
    }

    fn run_log(&self, run: &str) -> Option<PathBuf> {
        self.log_of.lock().unwrap().get(run).cloned()
    }

    async fn run(&self, brief: Brief) -> Result<ResultReport, Error> {
        self.briefs.lock().unwrap().push(brief.clone());
        let run = brief.run.clone().unwrap_or_default();
        std::fs::create_dir_all(&self.logs).unwrap();
        let log = self.logs.join(format!("{run}.jsonl"));
        std::fs::write(
            &log,
            format!("{{\"type\":\"test.session\",\"run\":\"{run}\"}}\n"),
        )
        .unwrap();
        self.log_of.lock().unwrap().insert(run, log);
        let act = self.next(&brief.title);
        let ws = brief.workspace.clone();
        let run_id = brief.run.clone().unwrap_or_default();
        tokio::task::spawn_blocking(move || {
            if let Some(ws) = &ws {
                ws.create().map_err(Error::Internal)?;
            }
            // Content unique to the run: a constant body commits nothing
            // when the worktree's base already has that file, and git
            // refuses an empty commit.
            let commit = |file: &str| -> Option<String> {
                let ws = ws.as_ref()?;
                std::fs::write(ws.path.join(file), format!("// {file} {run_id}\n")).unwrap();
                Some(commit_all(&ws.path, "change"))
            };
            match act {
                Act::ClaimWrongPath => Ok(ResultReport {
                    summary: "changed the helper".into(),
                    commit: commit("src/changed.rs"),
                    changed_paths: vec!["src/elsewhere.rs".into()],
                    ..ResultReport::default()
                }),
                Act::Commit(summary) => {
                    let sha = commit("src/fix.rs");
                    if let Some(ws) = &ws {
                        ws.remove().map_err(Error::Internal)?;
                    }
                    Ok(ResultReport {
                        summary: summary.into(),
                        changed_paths: sha.iter().map(|_| "src/fix.rs".to_string()).collect(),
                        commit: sha,
                        ..ResultReport::default()
                    })
                }
                Act::Nothing(summary) => {
                    if let Some(ws) = &ws {
                        ws.remove().map_err(Error::Internal)?;
                    }
                    Ok(ResultReport {
                        summary: summary.into(),
                        known_limits: vec!["could not see the verifier's records".into()],
                        ..ResultReport::default()
                    })
                }
                Act::Unrecognised(detail) => Err(Error::Internal(detail.into())),
                Act::Rule(summary, rule) => Ok(ResultReport {
                    summary: summary.into(),
                    artifacts: vec![ArtifactRef {
                        kind: CLASSIFIER_RULE.into(),
                        value: rule.into(),
                    }],
                    ..ResultReport::default()
                }),
            }
        })
        .await
        .map_err(|e| Error::Internal(e.to_string()))?
    }
}

/// A daemon's data directory for one test.
struct Data {
    dir: tempfile::TempDir,
}

impl Data {
    fn new() -> Data {
        Data {
            dir: tempfile::tempdir().unwrap(),
        }
    }

    fn logs(&self) -> PathBuf {
        self.dir.path().join("logs")
    }

    fn run_logs(&self) -> PathBuf {
        self.dir.path().join("run-logs")
    }

    /// The controller's configuration over this directory. Two items with
    /// the same failure are enough to file its fix item.
    fn config(&self, build: Option<BuildId>) -> ControllerConfig {
        ControllerConfig {
            worktree_root: Some(self.dir.path().join("worktrees")),
            investigation_root: Some(self.dir.path().join("investigations")),
            daemon_log_dir: Some(self.logs()),
            promote_threshold: 2,
            build,
            ..ControllerConfig::default()
        }
    }
}

fn code_draft(title: &str, repo: &Repo) -> WorkItemDraft {
    let mut d = draft("code", title);
    d.kind = Some(WorkKind::Code);
    d.writable_resources = vec![format!("{REPO_PREFIX}{}", repo.path().display())];
    d
}

/// The fix items on record, oldest first.
async fn fix_items(h: &Harness) -> Vec<WorkItem> {
    h.of_kind(WorkKind::Internal)
        .await
        .into_iter()
        .filter(|i| {
            i.artifact_refs
                .iter()
                .any(|r| r.kind == FAILURE_FINGERPRINT)
        })
        .collect()
}

fn fingerprint_of(fix: &WorkItem) -> String {
    fix.artifact_refs
        .iter()
        .find(|r| r.kind == FAILURE_FINGERPRINT)
        .map(|r| r.value.clone())
        .expect("a fix item names its failure")
}

async fn history(h: &Harness, target: &str) -> FailureHistory {
    ControlHandle::investigate(&h.ctl, target)
        .await
        .unwrap()
        .unwrap_or_else(|| panic!("nothing on record for {target}"))
}

/// Two items in `repo` that both claim a path their commit did not touch:
/// the second one's failure files the fix item. Returns their ids.
async fn two_claim_mismatches(h: &Harness, agent: &Agent, repo: &Repo) -> (String, String) {
    agent.on("First", &[Act::ClaimWrongPath, Act::ClaimWrongPath]);
    agent.on("Second", &[Act::ClaimWrongPath]);
    let first = h.file_one(code_draft("First change", repo)).await;
    h.drain_settled().await;
    let second = h.file_one(code_draft("Second change", repo)).await;
    (first, second)
}

// ── 5a: the check on the conclusion ─────────────────────────────────────

#[tokio::test]
async fn a_fix_item_that_lands_nothing_parks_with_its_conclusion_and_is_not_done() {
    let data = Data::new();
    let agent = Agent::new("pinch", None, data.run_logs());
    let h = Harness::on_wall_clock(data.config(None), vec![agent.clone()]);
    agent.on(
        "Reconcile",
        &[Act::Unrecognised("flux capacitor desynchronised")],
    );
    agent.on(
        "Classify unknown failure",
        &[Act::Nothing(
            "Not done: no fix or plan is committed and the root cause is unconfirmed.",
        )],
    );
    let ledger = h.file_one(draft("ledger", "Reconcile the ledger")).await;
    h.drain_settled().await;

    let fix = fix_items(&h).await.pop().expect("a fix item was filed");
    assert_eq!(
        h.status(&fix.id).await,
        Status::Blocked(BlockedReason::NeedsDecision),
        "a report that landed nothing is not done"
    );
    assert!(fix.constraints.iter().any(|c| c == LANDING_RULE));
    // Its findings are kept, none of them as a verified fix.
    let evidence = h.store().work_evidence_list(&fix.id).await.unwrap();
    let summary = evidence.iter().find(|e| e.kind == "summary").unwrap();
    assert!(summary.reference.starts_with("Not done:"));
    assert_eq!(summary.verified_by, None);
    assert!(evidence.iter().any(|e| e.kind == "known_limit"));
    assert!(evidence.iter().any(|e| e.kind == RUN_LOG));
    // Its conclusion is the question the owner is asked.
    let questions = h
        .store()
        .questions_list(&QuestionFilter {
            item: Some(fix.id.clone()),
            ..QuestionFilter::default()
        })
        .await
        .unwrap();
    assert_eq!(questions.len(), 1);
    assert_eq!(questions[0].status, QuestionStatus::Open);
    assert_eq!(questions[0].rule, "fix_not_landed");
    assert!(
        questions[0]
            .text
            .contains("Not done: no fix or plan is committed"),
        "{}",
        questions[0].text
    );
    assert!(questions[0].text.contains(&fingerprint_of(&fix)));
    let trail: Vec<String> = h
        .events(&fix.id)
        .await
        .iter()
        .filter(|e| matches!(e.kind, EventKind::Transition | EventKind::Lease))
        .filter_map(|e| e.to.map(|s| s.to_string()))
        .collect();
    assert!(!trail.iter().any(|s| s == "done"), "{trail:?}");
    assert_eq!(
        trail.last().map(String::as_str),
        Some("blocked(needs_decision)")
    );

    // What waited on it goes on with its own ladder instead of waiting for
    // the owner, and says why.
    assert_eq!(h.status(&ledger).await, Status::Done);
    let snap = h.ctl.load().await.unwrap();
    assert!(
        !snap
            .edges_held_by(&ledger)
            .any(|e| e.kind == EdgeKind::Blocks),
        "the edge onto the parked fix item is gone"
    );
    assert!(h.events(&ledger).await.iter().any(|e| {
        e.kind == EventKind::Resume
            && e.reason
                .as_deref()
                .is_some_and(|r| r.contains(&fix.id) && r.contains("without landing a fix"))
    }));
}

#[tokio::test]
async fn the_owners_answer_puts_a_parked_fix_item_back_to_work_with_the_answer() {
    let data = Data::new();
    let agent = Agent::new("pinch", None, data.run_logs());
    let h = Harness::on_wall_clock(data.config(None), vec![agent.clone()]);
    agent.on(
        "Reconcile",
        &[Act::Unrecognised("flux capacitor desynchronised")],
    );
    agent.on(
        "Classify unknown failure",
        &[
            Act::Nothing("Root cause found; not fixed."),
            Act::Rule("Added the rule.", "process: flux capacitor"),
        ],
    );
    h.file_one(draft("ledger", "Reconcile the ledger")).await;
    h.drain_settled().await;
    let fix = fix_items(&h).await.pop().unwrap();
    let question = h
        .store()
        .questions_list(&QuestionFilter {
            item: Some(fix.id.clone()),
            ..QuestionFilter::default()
        })
        .await
        .unwrap()
        .pop()
        .unwrap();

    ControlHandle::answer(
        &h.ctl,
        &question.id,
        "land it as a classifier rule",
        "user:test",
    )
    .await
    .unwrap();
    h.drain_settled().await;

    assert_eq!(h.status(&fix.id).await, Status::Done);
    let second = agent.briefs_for("Classify unknown failure").pop().unwrap();
    assert!(
        second
            .decisions_made
            .iter()
            .any(|d| d.contains("land it as a classifier rule")),
        "{:?}",
        second.decisions_made
    );
    let evidence = h.store().work_evidence_list(&fix.id).await.unwrap();
    assert!(evidence
        .iter()
        .any(|e| e.kind == CLASSIFIER_RULE && e.verified_by.as_deref() == Some("replay")));
}

#[tokio::test]
async fn a_fix_item_that_lands_a_commit_git_verifies_is_done() {
    let data = Data::new();
    let repo = Repo::new();
    let agent = Agent::new("pinch", Some(&repo), data.run_logs());
    let h = Harness::on_wall_clock(data.config(None), vec![agent.clone()]);
    agent.on(
        "Stop recurring",
        &[Act::Commit("Fixed the path bookkeeping.")],
    );
    let (_, second) = two_claim_mismatches(&h, &agent, &repo).await;
    h.drain_settled().await;

    let fix = fix_items(&h).await.pop().expect("a fix item was filed");
    assert_eq!(h.status(&fix.id).await, Status::Done);
    let evidence = h.store().work_evidence_list(&fix.id).await.unwrap();
    let commit = evidence.iter().find(|e| e.kind == "commit").unwrap();
    assert_eq!(commit.verified_by.as_deref(), Some("git"));
    assert_eq!(
        run_git(repo.path(), &["cat-file", "-t", &commit.reference]),
        "commit"
    );
    // The item that waited on it resumes with it as an input.
    let resumed = agent.briefs_for("Second").pop().unwrap();
    assert!(resumed.inputs.iter().any(|i| i.item == fix.id));
    assert_eq!(h.status(&second).await, Status::Done);
}

#[tokio::test]
async fn a_rule_for_a_failure_the_classifier_already_knows_is_not_a_landing() {
    let data = Data::new();
    let repo = Repo::new();
    let agent = Agent::new("pinch", Some(&repo), data.run_logs());
    let mut config = data.config(None);
    config.default_budget.rungs.repairs = 0;
    config.default_budget.rungs.worker_switches = 0;
    let h = Harness::on_wall_clock(config, vec![agent.clone()]);
    // The fix item for a known failure offers a rule: the rule did no
    // classifying, so the report claims more than the replay shows.
    agent.on(
        "Stop recurring",
        &[Act::Rule("Added a rule.", "process: changed paths")],
    );
    two_claim_mismatches(&h, &agent, &repo).await;
    h.drain_settled().await;

    let fix = fix_items(&h).await.pop().unwrap();
    assert_ne!(h.status(&fix.id).await, Status::Done);
    let evidence = h.store().work_evidence_list(&fix.id).await.unwrap();
    assert!(evidence.iter().any(|e| e.kind == "error"
        && e.reference
            .contains("does not classify the failure this item was filed for")));
    assert!(!evidence
        .iter()
        .any(|e| e.kind == CLASSIFIER_RULE && e.verified_by.as_deref() == Some("replay")));
}

#[tokio::test]
async fn a_new_failure_does_not_wait_behind_a_fix_item_parked_for_its_owner() {
    let data = Data::new();
    let agent = Agent::new("pinch", None, data.run_logs());
    let h = Harness::on_wall_clock(data.config(None), vec![agent.clone()]);
    agent.on(
        "Reconcile",
        &[Act::Unrecognised("flux capacitor desynchronised")],
    );
    agent.on(
        "Classify unknown failure",
        &[Act::Nothing("Could not find it.")],
    );
    h.file_one(draft("ledger", "Reconcile the ledger")).await;
    h.drain_settled().await;
    let fix = fix_items(&h).await.pop().unwrap();
    assert_eq!(
        h.status(&fix.id).await,
        Status::Blocked(BlockedReason::NeedsDecision)
    );

    agent.on(
        "Reconcile",
        &[Act::Unrecognised("flux capacitor desynchronised")],
    );
    let again = h
        .file_one(draft("again", "Reconcile the ledger again"))
        .await;
    h.drain_settled().await;

    assert_eq!(
        fix_items(&h).await.len(),
        1,
        "no second fix item while one is open"
    );
    assert_eq!(
        h.status(&again).await,
        Status::Done,
        "it went on with its ladder"
    );
    assert_eq!(h.rungs(&again).await, vec![Rung::Improve, Rung::Repair]);
    assert!(h.events(&again).await.iter().any(|e| e
        .reason
        .as_deref()
        .is_some_and(|r| r.contains("is waiting on its owner"))));
    // The parked item's evidence names the later failure too.
    let fix = h.item(&fix.id).await;
    assert!(fix
        .artifact_refs
        .iter()
        .any(|r| r.kind == "item" && r.value == again));
}

// ── 2: the history ──────────────────────────────────────────────────────

#[tokio::test]
async fn the_history_of_a_fingerprint_is_every_occurrence_run_by_run() {
    let data = Data::new();
    let repo = Repo::new();
    let agent = Agent::new("pinch", Some(&repo), data.run_logs());
    let build = BuildId {
        commit: "abc1234".into(),
        dirty: false,
    };
    let h = Harness::on_wall_clock(data.config(Some(build)), vec![agent.clone()]);
    agent.on(
        "Stop recurring",
        &[Act::Nothing("Not done: could not read the verifier.")],
    );
    let (first, second) = two_claim_mismatches(&h, &agent, &repo).await;
    h.drain_settled().await;
    let fix = fix_items(&h).await.pop().unwrap();
    let fp = fingerprint_of(&fix);

    let got = history(&h, &fp).await;
    assert_eq!(got.matched, Matched::Fingerprint);
    assert_eq!(got.fingerprints.len(), 1);
    let summary = &got.fingerprints[0];
    assert_eq!(summary.fingerprint, fp);
    assert_eq!(
        (summary.class.as_str(), summary.subclass.as_str()),
        ("verification", "claim_mismatch")
    );
    assert_eq!(summary.items, 2);
    assert_eq!(got.total_items, 2);
    let ids: Vec<&str> = got.occurrences.iter().map(|o| o.item.id.as_str()).collect();
    assert_eq!(ids, vec![first.as_str(), second.as_str()]);

    // The first item ran three times: the failure, a repair that failed
    // the same way, and a repair that passed. The first two hit the
    // fingerprint.
    let runs = &got.occurrences[0].runs;
    assert_eq!(runs.len(), 3, "{runs:#?}");
    assert!(!runs[2].hit);
    assert_eq!(runs[2].judged, Some(Status::Done));
    assert!(runs[2].rungs.is_empty());
    assert!(runs[2]
        .evidence
        .iter()
        .any(|e| e.kind == "commit" && e.verified_by.as_deref() == Some("git")));
    for run in &runs[..2] {
        assert!(run.hit);
        assert_eq!(run.worker.as_deref(), Some("pinch"));
        assert!(run.ended_at.is_some_and(|end| end >= run.started_at));
        assert!(run
            .ended
            .as_deref()
            .is_some_and(|e| e.starts_with("run ended")));
        assert_eq!(
            run.build.as_deref(),
            Some(format!("{}+abc1234", rustykrab_core::VERSION).as_str())
        );
        // The typed error of the rung it climbed.
        let error = run.rungs[0].error.as_ref().unwrap();
        assert_eq!(error.class, ErrorClass::Verification);
        assert_eq!(error.subclass, ErrorSubclass::ClaimMismatch);
        assert_eq!(error.fingerprint, fp);
        assert!(run
            .evidence
            .iter()
            .any(|e| e.kind == "error" && e.hash.as_deref() == Some(fp.as_str())));
        // Where it ran, and that the failed run's worktree is still there.
        let ws = run.workspace.as_ref().unwrap();
        assert_eq!(ws.repository, repo.path());
        assert!(ws.branch.starts_with("rustykrab/work/"));
        assert!(ws.worktree_exists, "a failed run keeps its worktree");
        // Its session log.
        let log = run.run_log.as_ref().unwrap();
        assert!(log.exists);
        assert_eq!(log.path, data.run_logs().join(format!("{}.jsonl", run.run)));
        // How it ended, as the controller moved the item.
        assert!(run
            .trail
            .iter()
            .any(|t| t.to == Some(Status::Blocked(BlockedReason::VerificationFailed))));
    }
    assert_eq!(runs[0].rungs[0].rung, Rung::Repair);
    assert_ne!(runs[0].run, runs[1].run);

    // The fix item filed for it: what it concluded, that it landed
    // nothing, and the question that conclusion became.
    assert_eq!(got.fixes.len(), 1);
    assert_eq!(got.fixes[0].item.id, fix.id);
    assert!(got.fixes[0].landed.is_empty());
    assert_eq!(
        got.fixes[0].conclusion.as_deref(),
        Some("Not done: could not read the verifier.")
    );
    assert_eq!(got.fixes[0].runs.len(), 1);
    assert_eq!(got.fixes[0].questions.len(), 1);
    assert_eq!(got.fixes[0].questions[0].status, "open");
    assert!(got.fixes[0].questions[0]
        .text
        .contains("Not done: could not read the verifier."));
    // Once the owner answers, the answer is on record with it.
    ControlHandle::answer(
        &h.ctl,
        &got.fixes[0].questions[0].id,
        "the verifier is in crates/rustykrab-control",
        "user:test",
    )
    .await
    .unwrap();
    let answered = history(&h, &fp).await;
    let question = &answered.fixes[0].questions[0];
    assert_eq!(question.status, "answered");
    assert_eq!(
        question.answer.as_deref(),
        Some("the verifier is in crates/rustykrab-control")
    );
    assert_eq!(question.answered_by.as_deref(), Some("user:test"));

    // By item id: that item's runs, and the fingerprints they hit.
    let by_item = history(&h, &second).await;
    assert_eq!(by_item.matched, Matched::Item);
    assert_eq!(by_item.occurrences.len(), 1);
    assert_eq!(by_item.fingerprints[0].fingerprint, fp);
    assert_eq!(by_item.fixes[0].item.id, fix.id);
    // A prefix of the fingerprint finds it; something unknown finds nothing.
    assert_eq!(history(&h, &fp[..8]).await.fingerprints[0].fingerprint, fp);
    assert!(ControlHandle::investigate(&h.ctl, "ffffffffffffffff")
        .await
        .unwrap()
        .is_none());
}

// ── 3 and 4: the code, the bundle and the daemon log ────────────────────

#[tokio::test]
async fn a_fix_item_is_leased_in_a_worktree_at_the_failing_runs_commit_with_its_bundle() {
    let data = Data::new();
    let repo = Repo::new();
    let agent = Agent::new("pinch", Some(&repo), data.run_logs());
    let h = Harness::on_wall_clock(data.config(None), vec![agent.clone()]);
    agent.on(
        "Stop recurring",
        &[Act::Nothing("Not done: nothing changed.")],
    );
    let failing_base = run_git(repo.path(), &["rev-parse", "HEAD"]);
    let (first, second) = two_claim_mismatches(&h, &agent, &repo).await;
    // Reconcile the second failure without leasing the fix item it files.
    h.tick().await;
    h.ctl.set_draining(true);
    h.step_settled().await;
    let fix = fix_items(&h).await.pop().expect("a fix item was filed");
    let fp = fingerprint_of(&fix);

    // The repository moves on after the failure.
    std::fs::write(repo.path().join("LATER.md"), "later\n").unwrap();
    let later = commit_all(repo.path(), "later work on main");
    assert_ne!(later, failing_base);
    // The daemon logged a line inside the second item's failed run, with a
    // bot token in it, the way a build before the Telegram fix did.
    let failed = &history(&h, &fp).await.occurrences[1].runs[0];
    std::fs::create_dir_all(data.logs()).unwrap();
    let stamp = failed
        .started_at
        .to_rfc3339_opts(chrono::SecondsFormat::Micros, true);
    let leaked = format!(
        "{stamp} ERROR rustykrab_channels::telegram: Telegram getUpdates network error: error \
         sending request for url (https://api.telegram.org/bot{FAKE_TOKEN}/getUpdates?timeout=30) \
         consecutive_errors=1 delay_secs=2\n"
    );
    std::fs::write(
        crate::investigate::daemon_log::log_file(&data.logs(), failed.started_at.date_naive()),
        &leaked,
    )
    .unwrap();

    h.ctl.set_draining(false);
    assert_eq!(h.tick().await.leased, vec![fix.id.clone()]);
    h.drain_settled().await;

    // The worktree it ran in is a checkout of the failing item's repository
    // at the commit the failing run started from, not at the new HEAD.
    let brief = agent.briefs_for("Stop recurring").pop().unwrap();
    let ws = brief
        .workspace
        .expect("a fix item with a repository gets a worktree");
    assert_eq!(ws.repo, repo.path());
    assert_eq!(ws.base, failing_base);
    assert_ne!(ws.base, later);

    // The bundle the controller wrote for that run.
    let kit = brief.investigation.expect("the brief carries the kit");
    assert_eq!(kit.fingerprint, fp);
    assert!(kit.dir.starts_with(data.dir.path().join("investigations")));
    let bundle: Bundle =
        serde_json::from_str(&std::fs::read_to_string(&kit.bundle).unwrap()).unwrap();
    assert_eq!(bundle.bundle.for_item, fix.id);
    assert_eq!(bundle.bundle.fingerprint, fp);
    let ids: Vec<&str> = bundle
        .history
        .occurrences
        .iter()
        .map(|o| o.item.id.as_str())
        .collect();
    assert_eq!(ids, vec![first.as_str(), second.as_str()]);
    assert_eq!((kit.items, kit.failed_runs), (2, 3));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&kit.bundle).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
    // It may read the bundle, the session logs and the failed runs' kept
    // worktrees.
    assert!(kit.read_dirs.contains(&kit.dir));
    assert!(kit.read_dirs.contains(&data.run_logs()));
    let kept = bundle.history.occurrences[1].runs[0]
        .workspace
        .as_ref()
        .unwrap();
    assert!(kept.worktree_exists);
    assert!(kit.read_dirs.contains(&kept.worktree));
    // The index points at each run's record in the bundle.
    let index = std::fs::read_to_string(&kit.index).unwrap();
    assert!(index.contains(&format!("run `{}`", failed.run)), "{index}");
    assert!(index.contains("bundle.json line "), "{index}");

    // The daemon log for the failed run's window, with the token gone.
    let log = bundle.history.occurrences[1].runs[0]
        .daemon_log
        .as_ref()
        .expect("the failed run has a daemon log slice");
    assert_eq!(log.redactions, 1);
    let slice = std::fs::read_to_string(&log.path).unwrap();
    assert!(leaked.contains(FAKE_TOKEN));
    assert!(!slice.contains("AAFake"), "{slice}");
    assert!(
        slice.contains("https://api.telegram.org/bot<redacted>/getUpdates?timeout=30"),
        "{slice}"
    );
    assert!(!std::fs::read_to_string(&kit.bundle)
        .unwrap()
        .contains("AAFake"));

    // The brief says where all of it is and how to read it in portions.
    let section = crate::investigate::bundle::brief_section(&kit);
    assert!(
        section.contains(&kit.index.display().to_string()),
        "{section}"
    );
    assert!(section.contains(&kit.bundle.display().to_string()));
    assert!(section.contains("a portion at a time"));
    assert!(section.contains("readable outside your working directory"));

    // And the lease recorded the bundle and the build that ran it.
    let evidence = h.store().work_evidence_list(&fix.id).await.unwrap();
    assert!(evidence.iter().any(|e| e.kind == INVESTIGATION_BUNDLE
        && e.reference == kit.bundle.display().to_string()
        && e.hash.as_deref() == Some(fp.as_str())));
    assert!(evidence
        .iter()
        .any(|e| e.kind == DAEMON_BUILD && e.reference == rustykrab_core::VERSION));
}

#[tokio::test]
async fn the_daemons_own_source_is_found_among_its_workers_repositories_and_named() {
    let data = Data::new();
    // A repository that is RustyKrab's as far as the check can tell: the
    // build's commit is in it and has the control crate.
    let source = Repo::new();
    std::fs::create_dir_all(source.path().join("crates/rustykrab-control")).unwrap();
    std::fs::write(
        source.path().join("crates/rustykrab-control/Cargo.toml"),
        "[package]\nname = \"rustykrab-control\"\n",
    )
    .unwrap();
    let built = commit_all(source.path(), "the running build");
    let build = BuildId {
        commit: built[..7].to_string(),
        dirty: false,
    };
    let other = Repo::new();
    assert_eq!(
        crate::investigate::detect_source(&[other.path().to_path_buf()], &build),
        None,
        "another repository is not the daemon's source"
    );

    let agent = Agent::new("pinch", Some(&source), data.run_logs());
    let h = Harness::on_wall_clock(data.config(Some(build)), vec![agent.clone()]);
    agent.on("Stop recurring", &[Act::Nothing("Not done.")]);
    two_claim_mismatches(&h, &agent, &source).await;
    h.drain_settled().await;

    let kit = agent
        .briefs_for("Stop recurring")
        .pop()
        .and_then(|b| b.investigation)
        .expect("a kit");
    let found = kit.source.as_ref().expect("the daemon's source is known");
    assert_eq!(found.repo, source.path());
    assert_eq!(found.commit, built);
    assert!(kit.read_dirs.contains(&source.path().to_path_buf()));
    let section = crate::investigate::bundle::brief_section(&kit);
    assert!(section.contains("RustyKrab's own source"), "{section}");
    assert!(section.contains(&built), "{section}");
}

// ── 5b: recurrence ──────────────────────────────────────────────────────

#[tokio::test]
async fn a_failure_that_comes_back_after_a_done_fix_says_so_and_carries_what_it_concluded() {
    let data = Data::new();
    let repo = Repo::new();
    let agent = Agent::new("pinch", Some(&repo), data.run_logs());
    let h = Harness::on_wall_clock(data.config(None), vec![agent.clone()]);
    agent.on(
        "Stop recurring",
        &[
            Act::Commit("Root cause: the path bookkeeping. Fixed it."),
            Act::Nothing("Not done: the earlier fix never reached main."),
        ],
    );
    // The item that waited on the first fix fails again once it resumes,
    // and so does a third item filed afterwards.
    agent.on(
        "Second",
        &[
            Act::ClaimWrongPath,
            Act::ClaimWrongPath,
            Act::ClaimWrongPath,
        ],
    );
    agent.on("Third", &[Act::ClaimWrongPath]);
    two_claim_mismatches(&h, &agent, &repo).await;
    h.drain_settled().await;
    let first_fix = fix_items(&h).await.pop().unwrap();
    assert_eq!(h.status(&first_fix.id).await, Status::Done);
    let fp = fingerprint_of(&first_fix);
    let landed = h
        .store()
        .work_evidence_list(&first_fix.id)
        .await
        .unwrap()
        .into_iter()
        .find(|e| e.kind == "commit")
        .unwrap()
        .reference;

    let third = h.file_one(code_draft("Third change", &repo)).await;
    h.drain_settled().await;

    let fixes = fix_items(&h).await;
    assert_eq!(fixes.len(), 2, "a second fix item for the recurrence");
    let second_fix = fixes.iter().find(|f| f.id != first_fix.id).unwrap();
    assert!(second_fix
        .artifact_refs
        .iter()
        .any(|r| r.kind == PREVIOUS_FIX && r.value == first_fix.id));
    assert!(
        second_fix.objective.contains(&format!("#{}", first_fix.id)),
        "{}",
        second_fix.objective
    );

    // Its brief carries the earlier conclusion, the commit it landed and
    // the runs that failed since, each with whether the fix was in what ran.
    let kit = agent
        .briefs_for("Stop recurring")
        .pop()
        .and_then(|b| b.investigation)
        .expect("the second fix item has a kit");
    assert_eq!(kit.earlier_fixes.len(), 1);
    let earlier = &kit.earlier_fixes[0];
    assert_eq!(earlier.item, first_fix.id);
    assert_eq!(earlier.status, "done");
    assert_eq!(
        earlier.conclusion.as_deref(),
        Some("Root cause: the path bookkeeping. Fixed it.")
    );
    assert_eq!(earlier.landed, vec![format!("commit {landed}")]);
    assert!(!earlier.recurrences.is_empty());
    let section = crate::investigate::bundle::brief_section(&kit);
    assert!(
        section.contains(&format!("earlier fix: item #{} is done", first_fix.id)),
        "{section}"
    );
    assert!(section.contains("Root cause: the path bookkeeping. Fixed it."));
    assert!(section.contains(&format!("commit {landed}")));
    assert!(
        section.contains("The failure recurred after it: run "),
        "{section}"
    );

    // The fix commit is on the fix item's branch, not on main: every run
    // since started from a commit without it, and the record says that
    // rather than counting the recurrence against the fix.
    let got = history(&h, &fp).await;
    let record = got
        .fixes
        .iter()
        .find(|f| f.item.id == first_fix.id)
        .unwrap();
    assert!(record.recurrences.iter().any(|r| r.item == third));
    for recurrence in &record.recurrences {
        assert_eq!(
            recurrence.includes_fix,
            Inclusion::NotIncluded,
            "{recurrence:#?}"
        );
        assert!(recurrence.basis.contains("does not contain the fix commit"));
        assert!(recurrence.base.is_some());
    }
    assert!(earlier.recurrences[0].contains("the fix was NOT in what ran"));

    // Once main has the fix, a run that still fails had it: that one counts.
    run_git(repo.path(), &["merge", "-q", "--ff-only", &landed]);
    agent.on("Fourth", &[Act::ClaimWrongPath, Act::ClaimWrongPath]);
    let fourth = h.file_one(code_draft("Fourth change", &repo)).await;
    h.drain_settled().await;
    let got = history(&h, &fp).await;
    let record = got
        .fixes
        .iter()
        .find(|f| f.item.id == first_fix.id)
        .unwrap();
    let after_merge: Vec<_> = record
        .recurrences
        .iter()
        .filter(|r| r.item == fourth)
        .collect();
    assert!(!after_merge.is_empty());
    for recurrence in after_merge {
        assert_eq!(
            recurrence.includes_fix,
            Inclusion::Included,
            "{recurrence:#?}"
        );
        assert!(recurrence.basis.contains("contains the fix commit"));
    }
}

// ── a fix in the daemon's own source ────────────────────────────────────

/// A repository that is RustyKrab's as far as the check can tell (the
/// build's commit is in it and has the control crate), the build made from
/// it, and a later commit on its `main`: the checkout has moved on since
/// the running daemon was built.
fn daemon_source() -> (Repo, String, BuildId, String) {
    let source = Repo::new();
    std::fs::create_dir_all(source.path().join("crates/rustykrab-control")).unwrap();
    std::fs::write(
        source.path().join("crates/rustykrab-control/Cargo.toml"),
        "[package]\nname = \"rustykrab-control\"\n",
    )
    .unwrap();
    let built = commit_all(source.path(), "the running build");
    let build = BuildId {
        commit: built[..7].to_string(),
        dirty: false,
    };
    std::fs::write(source.path().join("LATER.md"), "work after the build\n").unwrap();
    let later = commit_all(source.path(), "later work on main");
    (source, built, build, later)
}

/// The evidence rows a later slice reads to take a fix to the daemon's own
/// source further: the `workspace` row written at lease time and the
/// `commit` row git verified, exactly as a repository-scoped item has them.
async fn landed_rows(
    h: &Harness,
    item: &str,
) -> (
    rustykrab_core::work::Evidence,
    rustykrab_core::work::Evidence,
) {
    let evidence = h.store().work_evidence_list(item).await.unwrap();
    let workspace = evidence
        .iter()
        .rev()
        .find(|e| e.kind == crate::workspace::WORKSPACE_EVIDENCE)
        .cloned()
        .expect("a workspace row");
    let commit = evidence
        .iter()
        .rev()
        .find(|e| e.kind == "commit" && e.verified_by.as_deref() == Some("git"))
        .cloned()
        .expect("a commit row git verified");
    (workspace, commit)
}

#[tokio::test]
async fn a_failure_in_the_daemons_own_code_is_fixed_in_its_source_at_the_running_build() {
    let data = Data::new();
    let (source, built, build, later) = daemon_source();
    let agent = Agent::new("pinch", Some(&source), data.run_logs());
    let h = Harness::on_wall_clock(data.config(Some(build)), vec![agent.clone()]);
    // An item that names no repository fails in a way nothing classifies:
    // the cause is in the daemon, not in a repository of the user's.
    agent.on(
        "Reconcile",
        &[Act::Unrecognised("flux capacitor desynchronised")],
    );
    agent.on(
        "Classify unknown failure",
        &[Act::Commit(
            "Root cause in the verifier; fixed and committed.",
        )],
    );
    let ledger = h.file_one(draft("ledger", "Reconcile the ledger")).await;
    h.drain_settled().await;

    let fix = fix_items(&h).await.pop().expect("a fix item was filed");
    let resource = format!("{REPO_PREFIX}{}", source.path().display());
    assert_eq!(fix.writable_resources, vec![resource]);
    assert!(fix
        .artifact_refs
        .iter()
        .any(|r| r.kind == crate::investigate::DAEMON_SOURCE
            && r.value == source.path().display().to_string()));

    // It ran in a worktree of the daemon's source at the commit the running
    // build was made from, not at the checkout's later HEAD.
    let brief = agent.briefs_for("Classify unknown failure").pop().unwrap();
    let ws = brief
        .workspace
        .expect("an own-source fix item gets a worktree");
    assert_eq!(ws.repo, source.path());
    assert_eq!(ws.base, built);
    assert_ne!(ws.base, later);
    assert!(ws.branch.starts_with("rustykrab/work/"));
    let kit = brief.investigation.expect("a kit");
    assert!(kit.own_source);
    let section = crate::investigate::bundle::brief_section(&kit);
    assert!(
        section.contains("your working directory is a worktree of its source"),
        "{section}"
    );

    // It landed a commit git verified, and it is done.
    assert_eq!(h.status(&fix.id).await, Status::Done);
    let (workspace, commit) = landed_rows(&h, &fix.id).await;
    let recorded: crate::workspace::Workspace = serde_json::from_str(&workspace.reference).unwrap();
    assert_eq!(recorded.repo, source.path());
    assert_eq!(recorded.base, built);
    assert_eq!(recorded.branch, ws.branch);
    assert_eq!(workspace.hash.as_deref(), Some(built.as_str()));
    assert_eq!(workspace.verified_by, None);
    assert_eq!(commit.reference.len(), 40);
    assert_eq!(commit.hash, None);
    assert_eq!(
        run_git(source.path(), &["cat-file", "-t", &commit.reference]),
        "commit"
    );
    assert_eq!(
        run_git(
            source.path(),
            &["rev-parse", &format!("{}^", commit.reference)]
        ),
        built,
        "the fix is a commit on the running build"
    );
    // It stays on its own branch: nothing moved main or anything else.
    assert_eq!(run_git(source.path(), &["rev-parse", "main"]), later);
    assert_eq!(
        run_git(
            source.path(),
            &[
                "branch",
                "--contains",
                &commit.reference,
                "--format=%(refname:short)"
            ]
        ),
        ws.branch
    );
    assert_eq!(
        run_git(source.path(), &["remote"]),
        "",
        "nothing to push to"
    );
    // The item that waited on it goes on, with the fix as its input.
    assert_eq!(h.status(&ledger).await, Status::Done);
}

#[tokio::test]
async fn a_failure_in_a_repository_that_is_the_daemons_source_starts_from_the_build() {
    let data = Data::new();
    let (source, built, build, later) = daemon_source();
    let agent = Agent::new("pinch", Some(&source), data.run_logs());
    let h = Harness::on_wall_clock(data.config(Some(build)), vec![agent.clone()]);
    agent.on(
        "Stop recurring",
        &[Act::Commit("Fixed the path bookkeeping.")],
    );
    let (first, _) = two_claim_mismatches(&h, &agent, &source).await;
    h.drain_settled().await;

    // The failing runs started from the checkout's HEAD, which is later
    // than the build; the fix item starts from the build.
    let failed = h
        .ctl
        .workspace_of(&first)
        .await
        .unwrap()
        .expect("the failing item had a workspace");
    assert_eq!(failed.base, later);
    let fix = fix_items(&h).await.pop().unwrap();
    assert!(fix
        .artifact_refs
        .iter()
        .any(|r| r.kind == crate::investigate::DAEMON_SOURCE));
    let ws = agent
        .briefs_for("Stop recurring")
        .pop()
        .and_then(|b| b.workspace)
        .unwrap();
    assert_eq!(ws.base, built);
    assert_eq!(h.status(&fix.id).await, Status::Done);
    let (workspace, commit) = landed_rows(&h, &fix.id).await;
    assert_eq!(workspace.hash.as_deref(), Some(built.as_str()));
    assert_eq!(commit.verified_by.as_deref(), Some("git"));
}

#[tokio::test]
async fn a_failure_elsewhere_stays_in_its_repository_and_without_a_source_nothing_changes() {
    let (source, _, _, _) = daemon_source();
    let other = Repo::new();
    let found = crate::investigate::SourceCheckout {
        repo: source.path().to_path_buf(),
        commit: run_git(source.path(), &["rev-parse", "HEAD"]),
        version: rustykrab_core::VERSION.to_string(),
        dirty: false,
    };
    let h = Harness::new(&[]);
    let error = rustykrab_core::work::WorkError {
        class: ErrorClass::Unknown,
        subclass: ErrorSubclass::Unclassified,
        fingerprint: "elsewhere-fingerprint".into(),
        detail: "unclassified failure".into(),
        observed_by: "test".into(),
        artifact_refs: vec![],
    };
    let in_other = h
        .file_one(code_draft("Work in another repository", &other))
        .await;
    let nowhere = h
        .file_one(draft("nowhere", "Work with no repository"))
        .await;
    for (failing, source, want_repo, own) in [
        (&in_other, Some(&found), other.path(), false),
        (&nowhere, None, std::path::Path::new(""), false),
    ] {
        let mut batch =
            crate::controller::batch::Batch::new(h.ctl.load().await.unwrap(), chrono::Utc::now());
        let item = batch.snap.item(failing).unwrap().clone();
        let fix = h
            .ctl
            .file_internal(&mut batch, &item, &error, source)
            .unwrap();
        let fix = batch.snap.item(&fix).unwrap();
        let repos: Vec<&str> = fix
            .writable_resources
            .iter()
            .filter_map(|r| r.strip_prefix(REPO_PREFIX))
            .collect();
        if want_repo.as_os_str().is_empty() {
            assert!(repos.is_empty(), "{repos:?}");
        } else {
            assert_eq!(repos, vec![want_repo.display().to_string().as_str()]);
        }
        assert_eq!(
            fix.artifact_refs
                .iter()
                .any(|r| r.kind == crate::investigate::DAEMON_SOURCE),
            own
        );
    }
}

#[tokio::test]
async fn an_own_source_fix_falls_back_to_head_when_the_build_is_not_in_the_source() {
    let (source, _, _, later) = daemon_source();
    let config = ControllerConfig {
        build: Some(BuildId {
            commit: "0badc0de".into(),
            dirty: true,
        }),
        ..ControllerConfig::default()
    };
    let h = Harness::with(config, crate::controller::StaticCatalog::default(), &[]);
    let mut d = draft("fix", "Classify unknown failure fallback");
    d.kind = Some(WorkKind::Internal);
    d.writable_resources = vec![format!("{REPO_PREFIX}{}", source.path().display())];
    d.artifact_refs = vec![
        ArtifactRef {
            kind: FAILURE_FINGERPRINT.into(),
            value: "fallback-fingerprint".into(),
        },
        ArtifactRef {
            kind: crate::investigate::DAEMON_SOURCE.into(),
            value: source.path().display().to_string(),
        },
    ];
    let id = h.file_one(d).await;
    let item = h.item(&id).await;
    assert_eq!(h.ctl.fix_base(&item, source.path()).await, Some(later));
}
