//! The controller's side of an investigation (see [`crate::investigate`]).
//!
//! Four things, all in code:
//!
//! - **The history.** [`Controller::failure_history`] is
//!   [`crate::investigate::failure_history`] with what this daemon knows
//!   about itself: its build and, when one of its workers' repositories is
//!   the repository it was built from, its own source checkout.
//! - **The kit.** When a fix item is leased, [`Controller::investigation_kit`]
//!   writes that history as the run's investigation bundle and returns the
//!   kit its brief carries.
//! - **The code.** [`Controller::fix_base`] is the commit a fix item's
//!   worktree is pinned to: the running build's commit in the daemon's own
//!   source, otherwise the one the failing run started from.
//! - **The check.** A fix item is `done` only when it landed something the
//!   controller checked. [`Controller::park_unlanded_fix`] is what happens
//!   otherwise: its findings are kept as evidence, it parks for the owner
//!   with its conclusion as the question, and the items that waited on it
//!   are released to go on with their own ladders.

use std::path::{Path, PathBuf};

use chrono::Utc;
use rustykrab_core::questions::QuestionKind;
use rustykrab_core::work::{
    BlockedReason, EdgeKind, EventKind, Evidence, ResultReport, Rung, RungEvent, Status, WorkEvent,
    WorkItem, WorkItemId,
};
use rustykrab_core::Error;
use rustykrab_store::WorkOp;

use crate::investigate::bundle::{self, Kit};
use crate::investigate::{
    self, daemon_log, fix_fingerprint, same_repo, BuildId, FailureHistory, HistoryContext,
    SourceCheckout, DAEMON_BUILD, DAEMON_SOURCE, INVESTIGATION_BUNDLE,
};
use crate::workspace::{self, Workspace, REPO_PREFIX};

use super::batch::Batch;
use super::brief::SUMMARY;
use super::notice::label;
use super::questions::NewQuestion;
use super::Controller;

/// Characters of a fix item's conclusion carried into its question.
const CONCLUSION_MAX: usize = 1_500;

/// One evidence row a verified claim adds: kind, reference, verifier.
type Proof = (String, String, Option<String>);

fn clip(text: &str, max: usize) -> String {
    let text = text.trim();
    match text.char_indices().nth(max) {
        Some((cut, _)) => format!("{}...", &text[..cut]),
        None => text.to_string(),
    }
}

impl Controller {
    /// This daemon's build as every run records it.
    pub(super) fn build_label(&self) -> String {
        BuildId::label(self.config.build.as_ref())
    }

    /// The `daemon_build` evidence a lease writes for `run`.
    pub(super) fn build_evidence(&self, item: &str, run: &str) -> Evidence {
        Evidence {
            item: item.to_string(),
            kind: DAEMON_BUILD.to_string(),
            reference: self.build_label(),
            hash: Some(run.to_string()),
            verified_by: None,
            at: Utc::now(),
        }
    }

    /// The daemon's own source checkout, when one of the repositories its
    /// workers were added for (or one of `also`) is the repository this
    /// build was made from. Looked for again only when that list changes.
    pub(super) async fn source_checkout(&self, also: &[PathBuf]) -> Option<SourceCheckout> {
        let build = self.config.build.clone()?;
        let mut candidates: Vec<PathBuf> = also.to_vec();
        for worker in self.registry.workers() {
            let caps = worker.capabilities();
            let named = caps.repos.iter().map(String::as_str).chain(
                caps.writable_resources
                    .iter()
                    .filter_map(|r| r.strip_prefix(REPO_PREFIX)),
            );
            for repo in named.map(str::trim).filter(|r| !r.is_empty() && *r != "*") {
                let repo = PathBuf::from(repo);
                if !candidates.contains(&repo) {
                    candidates.push(repo);
                }
            }
        }
        if let Some((known, found)) = &self.state().source {
            if *known == candidates {
                return found.clone();
            }
        }
        let looked = candidates.clone();
        let found =
            tokio::task::spawn_blocking(move || investigate::detect_source(&looked, &build))
                .await
                .ok()
                .flatten();
        self.state().source = Some((candidates, found.clone()));
        found
    }

    /// Everything on record about `target` (a fingerprint or an item id).
    pub(super) async fn failure_history(
        &self,
        target: &str,
    ) -> Result<Option<FailureHistory>, Error> {
        let ctx = HistoryContext {
            build: Some(self.build_label()),
            source: self.source_checkout(&[]).await,
            limit: 0,
        };
        investigate::failure_history(&self.store, target, &ctx).await
    }

    /// The commit a fix item's worktree in `repo` starts from.
    ///
    /// In the daemon's own source (the item is marked [`DAEMON_SOURCE`]),
    /// the commit the running build was made from, so the fix is made to the
    /// code that failed; that repository's `HEAD` when the build's commit is
    /// not in it. Anywhere else, the one the run that failed started from,
    /// read from the failing items it names (the one that filed it first).
    /// `None` for any other item, or when no failing run recorded a
    /// workspace in `repo` or its commit is gone: the caller falls back to
    /// the repository's `HEAD`.
    pub(super) async fn fix_base(&self, item: &WorkItem, repo: &Path) -> Option<String> {
        fix_fingerprint(item.kind, &item.artifact_refs)?;
        let own = item
            .artifact_refs
            .iter()
            .any(|r| r.kind == DAEMON_SOURCE && same_repo(Path::new(&r.value), repo));
        if own {
            let build = self.config.build.as_ref().map(|b| b.commit.clone());
            let at = repo.to_path_buf();
            return tokio::task::spawn_blocking(move || {
                let built = build.and_then(|c| workspace::resolve_commit(&at, &c).ok().flatten());
                built.or_else(|| workspace::head(&at).ok())
            })
            .await
            .ok()
            .flatten();
        }
        for failing in item.artifact_refs.iter().filter(|r| r.kind == "item") {
            let Ok(Some(ws)) = self.workspace_of(&failing.value).await else {
                continue;
            };
            let canon = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
            if ws.repo != repo && canon(&ws.repo) != canon(repo) {
                continue;
            }
            let (at, base) = (repo.to_path_buf(), ws.base.clone());
            let resolved =
                tokio::task::spawn_blocking(move || workspace::resolve_commit(&at, &base))
                    .await
                    .ok()
                    .and_then(Result::ok)
                    .flatten();
            if resolved.is_some() {
                return resolved;
            }
        }
        None
    }

    /// For a fix item about to run as `run`: write the failure's history as
    /// its investigation bundle and return the kit for its brief, with the
    /// evidence row that records where the bundle is. `None` for any other
    /// item, when no bundle root is configured, or when nothing is on
    /// record for the fingerprint yet. A bundle that cannot be written is
    /// a warning on the item, and the run goes ahead without one.
    pub(super) async fn investigation_kit(
        &self,
        item: &WorkItem,
        run: &str,
    ) -> Option<(Kit, Evidence)> {
        let fingerprint = fix_fingerprint(item.kind, &item.artifact_refs)?.to_string();
        let root = self.config.investigation_root.clone()?;
        let also: Vec<PathBuf> = Workspace::repo_of(&item.writable_resources)
            .into_iter()
            .collect();
        let source = self.source_checkout(&also).await;
        let own_source = item.artifact_refs.iter().any(|r| {
            r.kind == DAEMON_SOURCE
                && source
                    .as_ref()
                    .is_some_and(|s| same_repo(Path::new(&r.value), &s.repo))
        });
        let ctx = HistoryContext {
            build: Some(self.build_label()),
            source: source.clone(),
            limit: 0,
        };
        let written = match investigate::failure_history(&self.store, &fingerprint, &ctx).await {
            Ok(Some(history)) => {
                let (item_id, run_id, fp) = (item.id.clone(), run.to_string(), fingerprint.clone());
                let log_dir = self.config.daemon_log_dir.clone();
                tokio::task::spawn_blocking(move || {
                    bundle::write(
                        &bundle::Request {
                            root: &root,
                            item: &item_id,
                            run: &run_id,
                            fingerprint: &fp,
                            log_dir: log_dir.as_deref(),
                            source: source.as_ref(),
                            own_source,
                            max_log_bytes: daemon_log::DEFAULT_MAX_BYTES,
                        },
                        history,
                    )
                })
                .await
                .map_err(|e| e.to_string())
                .and_then(|r| r.map_err(|e| e.to_string()))
            }
            Ok(None) => return None,
            Err(e) => Err(e.to_string()),
        };
        match written {
            Ok(kit) => {
                let evidence = Evidence {
                    item: item.id.clone(),
                    kind: INVESTIGATION_BUNDLE.to_string(),
                    reference: kit.bundle.display().to_string(),
                    hash: Some(fingerprint),
                    verified_by: None,
                    at: Utc::now(),
                };
                Some((kit, evidence))
            }
            Err(why) => {
                tracing::warn!(item = %item.id, %why, "investigation bundle not written");
                let _ = self
                    .store
                    .work_event_append(&WorkEvent {
                        item: item.id.clone(),
                        at: Utc::now(),
                        kind: EventKind::Warning,
                        from: None,
                        to: None,
                        actor: "controller".to_string(),
                        reason: Some(format!("investigation bundle not written: {why}")),
                        upstream: None,
                        origin: None,
                        evidence_ref: None,
                    })
                    .await;
                None
            }
        }
    }

    /// A fix item reported without landing anything the controller checked
    /// (no commit verified by git, no classifier rule that replays): it is
    /// not `done`. Its report is kept as evidence, none of it verified as a
    /// fix; it parks as `blocked(needs_decision)` with its conclusion as
    /// the question, so the owner's answer goes into its next run; and the
    /// items the ladder parked behind it are released.
    pub(super) async fn park_unlanded_fix(
        &self,
        b: &mut Batch,
        item: &WorkItem,
        worker: &str,
        fingerprint: &str,
        result: &ResultReport,
        proofs: &[Proof],
    ) -> Result<Vec<WorkItemId>, Error> {
        for artifact in &result.artifacts {
            b.evidence(&item.id, &artifact.kind, &artifact.value, None);
        }
        b.evidence(&item.id, SUMMARY, result.summary.trim(), None);
        for (kind, reference, by) in proofs {
            b.evidence(&item.id, kind, reference, by.as_deref());
        }
        if !proofs.iter().any(|(kind, _, _)| kind == "check_run") {
            for check in &result.checks_run {
                b.evidence(&item.id, "check_run", check, None);
            }
        }
        for limit in &result.known_limits {
            b.evidence(&item.id, "known_limit", limit, None);
        }
        let limits = match result.known_limits.as_slice() {
            [] => String::new(),
            limits => format!(" Known limits: {}.", clip(&limits.join("; "), 600)),
        };
        let text = format!(
            "{} landed nothing the controller can check for failure {fingerprint} (no commit in \
             its worktree, no classifier rule that replays), so it is not done. It reported: \
             {}{limits} What should it do next? Your answer goes into its next run (for example \
             where the code is, what to change, or that no fix is wanted).",
            label(item),
            clip(&result.summary, CONCLUSION_MAX),
        );
        let mut state = self.ladder_of(item).await?;
        b.rung(
            &item.id,
            &mut state,
            RungEvent {
                rung: Rung::Surface,
                at: b.now,
                error: None,
                outcome: format!("parked needs_decision: no fix landed on {worker}"),
            },
        );
        b.own(
            &item.id,
            Status::Verifying,
            "controller",
            "checking the result report",
            None,
        );
        let mut changed = b.move_to(
            &item.id,
            Status::Blocked(BlockedReason::NeedsDecision),
            "controller",
            format!(
                "no fix landed on {worker}: a fix item is done only with a commit git verified \
                 or a classifier rule that replays; its conclusion goes to the owner"
            ),
        );
        self.record_question(
            b,
            NewQuestion::open(
                &item.id,
                QuestionKind::for_blocked(BlockedReason::NeedsDecision),
                text,
                "fix_not_landed",
            ),
        )
        .await?;
        changed.extend(release_waiters(b, &item.id));
        Ok(changed)
    }
}

/// Release every open item that holds a `blocks` edge on the fix item
/// `fix`, which parked without landing: the edge is dropped, a `resume`
/// event on each says why, and the fix item's outcome is left as evidence
/// for its next brief. Returns the released ids, for the caller to settle.
pub(super) fn release_waiters(b: &mut Batch, fix: &str) -> Vec<WorkItemId> {
    let edges: Vec<rustykrab_core::work::Edge> = b
        .snap
        .edges_naming(fix)
        .filter(|e| e.kind == EdgeKind::Blocks)
        .filter(|e| b.snap.item(&e.item).is_some_and(|i| !i.status.is_closed()))
        .cloned()
        .collect();
    let mut released = Vec::new();
    for edge in edges {
        b.snap.remove_edge(&edge);
        b.ops.push(WorkOp::RemoveEdge(edge.clone()));
        b.note(
            &edge.item,
            EventKind::Resume,
            "controller",
            format!(
                "resolution item {fix} parked for its owner without landing a fix; no longer \
                 waiting on it, the ladder goes on"
            ),
        );
        b.evidence(
            &edge.item,
            "resolution_outcome",
            format!("{fix}: no fix landed; its conclusion is with the owner"),
            Some("controller"),
        );
        released.push(edge.item);
    }
    released
}
