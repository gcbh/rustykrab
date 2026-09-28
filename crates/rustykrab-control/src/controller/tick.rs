//! One pass of the loop (plan section 6): sweep, reconcile, climb, select,
//! lease and run, age. See the module docs of `controller` for the order
//! and why.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use chrono::{DateTime, TimeDelta, Utc};
use rustykrab_core::work::{
    BlockedReason, ErrorClass, Rung, RungEvent, Status, WorkError, WorkItem, WorkItemId, WorkKind,
    WorkerKind,
};
use rustykrab_core::{Error, ToolErrorKind};
use rustykrab_store::{TransitionSpec, WorkOp};
use rustykrab_tools::work_backend::{ToolState, WorkRunContext, WORK_RUN_CONTEXT};

use crate::errors::{
    apply_learned, classify, fingerprint, gap_of, Context, FailureInput, GapKind, LearnedRule,
    VerifierVerdict, CLASSIFIER_RULE,
};
use crate::graph::{self, Effects};
use crate::handle::TickReport;
use crate::ladder::{self, Decision, LadderContext, LadderState, SurfaceReason};
use crate::routing::{built_tool, work_class, Judged, Verdict};
use crate::worker::{run_failure_input, Brief, Worker, COMMAND_RUN};
use crate::workspace::{self, CodeClaim, CodeVerdict, Workspace, WORKSPACE_EVIDENCE};

use super::batch::{has_open_plan_b, Batch};
use super::brief::{brief_for, first_line, ERROR, RESULT_REPORT, RUN, SUMMARY};
use super::commit::Written;
use super::filing::{
    parked_on_landed_capability, parked_reason, unneeded_capabilities, waiting_on_mcp,
};
use super::load::{history, ladder_from, ESCALATING_ABOVE, REPLAYED, SWITCHED};
use super::notice::{label, Cause};
use super::{Controller, Finished, Run, RunResult};

/// Moves the ladder may make on one failure before it must surface: the
/// improve and skipped-switch rungs continue the climb, the rest end it.
const MAX_CLIMB: usize = 16;

/// Who verified a claim: git for a commit and its paths, the host's
/// catalog for a built tool, the adapter's command record for a check.
const BY_GIT: &str = "git";
const BY_CATALOG: &str = "catalog";

/// One evidence row a verified claim adds: kind, reference, verifier.
type Proof = (String, String, Option<String>);

/// How a failure counts in the routing record: a capability gap or a
/// policy stop is not the worker's result, so it is not recorded.
fn verdict_of(error: &WorkError) -> Option<Verdict> {
    match error.class {
        ErrorClass::Verification => Some(Verdict::NotVerified),
        ErrorClass::CapabilityGap | ErrorClass::Policy => None,
        _ => Some(Verdict::Failed),
    }
}

fn absorb(report: &mut TickReport, written: Written) {
    report.transitions += written.transitions;
    report.notices += written.notices;
    for id in written.made_ready {
        if !report.made_ready.contains(&id) {
            report.made_ready.push(id);
        }
    }
}

/// Whether `worker` can take `item`: kind, tools, MCP servers and
/// writable resources all covered (`*` covers everything).
pub(super) fn covers(worker: &dyn Worker, item: &WorkItem) -> bool {
    let caps = worker.capabilities();
    let has = |list: &[String], want: &String| list.iter().any(|x| x == want || x == "*");
    (item.worker_kind == WorkerKind::Any || item.worker_kind == worker.kind())
        && item.required_tools.iter().all(|t| has(&caps.tools, t))
        && item
            .required_mcp_servers
            .iter()
            .all(|s| has(&caps.mcp_servers, s))
        && item
            .writable_resources
            .iter()
            .all(|r| has(&caps.writable_resources, r))
}

/// The blocked state a surfaced failure parks in (sections 7 and 8).
fn surface_reason(error: &WorkError, why: SurfaceReason) -> BlockedReason {
    if why == SurfaceReason::PolicyStop {
        return BlockedReason::NeedsDecision;
    }
    if let Some(gap) = gap_of(error) {
        return match gap.kind {
            GapKind::Knowledge => BlockedReason::NeedsDecision,
            other => parked_reason(other),
        };
    }
    match error.class {
        ErrorClass::Budget => BlockedReason::BudgetExhausted,
        ErrorClass::Verification => BlockedReason::VerificationFailed,
        _ => BlockedReason::NeedsDecision,
    }
}

/// A worker's own error, made fit for the ladder: an `unknown` one, or one
/// whose subclass does not belong to its class, is classified again from
/// its detail; a missing fingerprint or observer is filled in.
fn normalise_reported(error: &WorkError, kind: WorkerKind) -> WorkError {
    let ctx = Context {
        tool: None,
        worker_kind: Some(kind),
    };
    if error.class == ErrorClass::Unknown || error.subclass.class() != error.class {
        let mut again = classify(
            &FailureInput::Raw {
                message: error.detail.clone(),
            },
            &ctx,
        );
        again.artifact_refs = error.artifact_refs.clone();
        return again;
    }
    let mut out = error.clone();
    if out.fingerprint.is_empty() {
        out.fingerprint = fingerprint(out.class, out.subclass, None, Some(kind), &out.detail);
    }
    if out.observed_by.is_empty() {
        out.observed_by = "worker_report".to_string();
    }
    out
}

/// A claimed check as the verifier compares it: lower case, whitespace
/// collapsed, with a trailing parenthetical annotation such as
/// `(86 passed)` removed.
fn claimed_check(check: &str) -> String {
    let mut claim = collapse(check);
    while claim.ends_with(')') {
        let mut depth = 0usize;
        let mut open = None;
        for (i, ch) in claim.char_indices().rev() {
            match ch {
                ')' => depth += 1,
                '(' => {
                    depth -= 1;
                    if depth == 0 {
                        open = Some(i);
                        break;
                    }
                }
                _ => {}
            }
        }
        match open {
            // A claim that is nothing but a parenthetical is not stripped.
            Some(i) if i > 0 => claim = claim[..i].trim_end().to_string(),
            _ => break,
        }
    }
    claim
}

/// Lower case, with every run of whitespace made one space.
fn collapse(s: &str) -> String {
    s.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// Whether a normalised claimed check ran in `command`: it equals one of
/// the command's segments (split on `&&`, `||`, `;` and `|`, so a leading
/// `cd <dir> &&` is its own segment), or it appears in the command as a
/// whole, which covers a shell wrapper such as `bash -lc '...'` and a claim
/// that is itself compound.
fn check_ran_in(wanted: &str, command: &str) -> bool {
    let command = collapse(command);
    if command.contains(wanted) {
        return true;
    }
    command
        .replace("&&", ";")
        .replace("||", ";")
        .replace('|', ";")
        .split(';')
        .map(|s| s.trim().trim_matches(|c| c == '\'' || c == '"').trim())
        .any(|s| s == wanted)
}

/// The class a failed tick reports: the variant of the error it returned.
/// Exhaustive, so a new variant has to be named here.
pub(super) fn failure_class(err: &Error) -> &'static str {
    match err {
        Error::ModelProvider(_) => "model_provider",
        Error::ModelEmptyResponse(_) => "model_empty_response",
        Error::ModelRateLimit(_) => "model_rate_limit",
        Error::ModelAuthError(_) => "model_auth",
        Error::ModelBadRequest(_) => "model_bad_request",
        Error::ContextBudgetExceeded { .. } => "context_budget_exceeded",
        Error::ModelOverloaded(_) => "model_overloaded",
        Error::ContentPolicy => "content_policy",
        Error::ToolExecution(_) => "tool_execution",
        Error::Config(_) => "config",
        Error::Storage(_) => "storage",
        Error::Serialization(_) => "serialization",
        Error::Channel(_) => "channel",
        Error::Auth(_) => "auth",
        Error::NotFound(_) => "not_found",
        Error::AlreadyExists(_) => "already_exists",
        Error::PendingApproval { .. } => "pending_approval",
        Error::Internal(_) => "internal",
    }
}

impl Controller {
    /// One pass, and its outcome recorded for [`LoopStatus`]: a completed
    /// pass sets `last_tick` and clears the failure count, a failed one
    /// records its time and class and counts up.
    ///
    /// [`LoopStatus`]: crate::handle::LoopStatus
    pub(super) async fn tick_locked(&self) -> Result<TickReport, Error> {
        #[cfg(test)]
        let injected = self.state().fail_next_tick.take();
        #[cfg(not(test))]
        let injected: Option<Error> = None;
        let result = match injected {
            Some(err) => Err(err),
            None => self.tick_pass().await,
        };
        let now = self.clock.now();
        let mut state = self.state();
        match &result {
            Ok(_) => {
                state.last_tick = Some(now);
                state.consecutive_failures = 0;
            }
            Err(err) => {
                state.last_failure = Some((now, failure_class(err)));
                state.consecutive_failures = state.consecutive_failures.saturating_add(1);
            }
        }
        result
    }

    async fn tick_pass(&self) -> Result<TickReport, Error> {
        let now = self.clock.now();
        let first = !self.state().resumed;
        if first {
            self.load_recurrence(now).await?;
            self.load_learned().await?;
        }
        let mut report = TickReport::default();
        let mut noticed: HashSet<WorkItemId> = HashSet::new();
        self.sweep(now, first, &mut report, &mut noticed).await?;
        self.state().resumed = true;
        self.reconcile_all(now, &mut report, &mut noticed).await?;
        self.select_and_lease(&mut report).await?;
        self.age(now, &mut report).await?;
        Ok(report)
    }

    // ── 1. resume and sweep ─────────────────────────────────────────────

    /// Plan 6.7 on every tick, in one transaction.
    async fn sweep(
        &self,
        now: DateTime<Utc>,
        first: bool,
        report: &mut TickReport,
        noticed: &mut HashSet<WorkItemId>,
    ) -> Result<(), Error> {
        let mut b = Batch::new(self.load().await?, now);
        let all: Vec<WorkItemId> = b.snap.items().iter().map(|i| i.id.clone()).collect();

        // Stored state that disagrees with its derivation is a controller
        // defect: cascade holds on leaves always (nothing about them
        // depends on time; a parent's status is its roll-up), and roll-ups
        // on the first tick, from the rows as stored.
        let mut probe = b.snap.clone();
        let mut fixes = Effects::default();
        let leaves: Vec<WorkItemId> = all
            .iter()
            .filter(|id| !probe.has_children(id))
            .cloned()
            .collect();
        for _ in 0..=leaves.len() {
            let mut moved = false;
            for id in &leaves {
                if let Some(t) = graph::hold_recompute(&probe, id) {
                    probe.apply(
                        &Effects {
                            transitions: vec![t.clone()],
                            ..Effects::default()
                        },
                        now,
                    );
                    fixes.transitions.push(t);
                    moved = true;
                }
            }
            if !moved {
                break;
            }
        }
        if first {
            let timed = |id: &str| {
                probe
                    .item(id)
                    .is_some_and(|i| matches!(i.trigger, rustykrab_core::work::Trigger::At(_)))
            };
            let rollups: Vec<_> = graph::rollup_all(&probe, &all, now)
                .into_iter()
                .filter(|t| !timed(&t.item))
                .collect();
            fixes.transitions.extend(rollups);
        }
        if !fixes.transitions.is_empty() {
            let found: Vec<String> = fixes
                .transitions
                .iter()
                .map(|t| format!("{} stored {} but derives {}", t.item, t.from, t.to))
                .collect();
            b.corrections(&fixes);
            if let Err(why) = self.file_defect(&mut b, &found) {
                tracing::warn!(%why, "resume defect not filed");
            }
        }
        let mut changed: Vec<WorkItemId> = fixes.touched();

        // Expiries first, with their cascade, so an expired chain does not
        // start one more step.
        for id in graph::due_expiries(&b.snap, now) {
            if b.status(&id).is_none_or(|s| s.is_closed()) {
                continue;
            }
            changed.extend(b.close(&id, Status::Expired, "controller", "expires_at passed"));
            report.expired.push(id);
        }

        // Runs past their lease TTL on the controller's clock have stalled
        // (step 7) unless a progress ledger says otherwise: the run stops
        // and the ladder climbs.
        let ttl = i64::try_from(self.config.lease_ttl_seconds)
            .ok()
            .and_then(TimeDelta::try_seconds)
            .unwrap_or(TimeDelta::MAX);
        let past_ttl =
            |since: DateTime<Utc>| since.checked_add_signed(ttl).is_some_and(|d| d < now);
        let stalled: Vec<(WorkItemId, String, DateTime<Utc>)> = self
            .state()
            .runs
            .iter()
            .filter(|(_, r)| !r.reported && !r.handle.is_finished() && past_ttl(r.since))
            .map(|(id, r)| (id.clone(), r.worker.clone(), r.since))
            .collect();
        for (id, worker, since) in stalled {
            if !b.status(&id).is_some_and(|s| s.is_active()) {
                continue;
            }
            if self.ledger.progressed(&id, since) {
                if let Some(run) = self.state().runs.get_mut(&id) {
                    run.since = now;
                }
                self.store.work_lease_heartbeat(&id).await?;
                continue;
            }
            let Some(item) = b.snap.item(&id).cloned() else {
                continue;
            };
            let error = classify(
                &FailureInput::ToolResult {
                    tool: String::new(),
                    kind: ToolErrorKind::Timeout,
                    message: format!(
                        "stalled: no progress within the {}s lease",
                        self.config.lease_ttl_seconds
                    ),
                },
                &Context {
                    tool: None,
                    worker_kind: Some(self.worker_kind(&worker)),
                },
            );
            b.revoke.push(id.clone());
            changed.extend(self.climb(&mut b, &item, &worker, error).await?);
        }

        // A restart does not kill a run that lives outside this process
        // (a peer's task): one its worker still holds is re-attached before
        // anything below returns to ready (`reattach.rs`, Phase 5).
        if first {
            self.reattach(&b).await;
        }

        // Leases no run of this process holds, past their TTL by the
        // store's own clock (it stamped them): returned to `ready` with a
        // repair note. Nothing fails because a run was lost.
        for lease in self.store.work_leases_expired(Utc::now()).await? {
            if !b.status(&lease.item).is_some_and(|s| s.is_active()) {
                continue;
            }
            let ours = {
                let state = self.state();
                state.runs.contains_key(&lease.item) || state.finished.contains_key(&lease.item)
            };
            if ours {
                continue;
            }
            changed.extend(b.resume_to(
                &lease.item,
                Status::Ready,
                format!(
                    "the lease on {} expired without a heartbeat; returned to ready with a repair \
                     note",
                    lease.worker
                ),
            ));
        }

        // A restart: this process runs nothing yet, so every active leaf
        // is a run that did not survive. Nothing fails because of it.
        if first {
            let orphans: Vec<WorkItemId> = {
                let state = self.state();
                b.snap
                    .items()
                    .iter()
                    .filter(|i| {
                        i.status.is_active()
                            && !b.snap.has_children(&i.id)
                            && !state.runs.contains_key(&i.id)
                            && !state.finished.contains_key(&i.id)
                    })
                    .map(|i| i.id.clone())
                    .collect()
            };
            for id in orphans {
                changed.extend(
                    b.resume_to(
                        &id,
                        Status::Ready,
                        "the run did not survive a restart; returned to ready with a repair note"
                            .to_string(),
                    ),
                );
            }
        }

        // Capability items that landed release what waited on them, and a
        // configured MCP server releases what named it.
        for id in parked_on_landed_capability(&b.snap) {
            changed.extend(b.move_to(&id, Status::Queued, "controller", "capability landed"));
        }
        for id in waiting_on_mcp(&b.snap, |s| self.catalog.mcp_server_configured(s)) {
            changed.extend(b.move_to(&id, Status::Queued, "controller", "MCP server configured"));
        }
        // A capability item nothing waits on any more is cancelled before
        // anyone leases it (6.2): the chain that needed it closed.
        for (id, origin) in unneeded_capabilities(&b.snap) {
            changed.extend(b.cancel_unneeded(&id, &origin));
        }

        // Readiness (time triggers that fired), roll-ups and verification.
        changed.extend(all);
        b.settle(changed);
        if let Some(written) = self.commit(b, noticed).await? {
            absorb(report, written);
        }
        Ok(())
    }

    // ── 2. reconcile ────────────────────────────────────────────────────

    /// Every finished run and handed-in report, one transaction each.
    async fn reconcile_all(
        &self,
        now: DateTime<Utc>,
        report: &mut TickReport,
        noticed: &mut HashSet<WorkItemId>,
    ) -> Result<(), Error> {
        let (mut ready, done_runs) = {
            let mut state = self.state();
            let ready: Vec<(WorkItemId, Finished)> = state.finished.drain().collect();
            let ids: Vec<WorkItemId> = state
                .runs
                .iter()
                .filter(|(_, r)| r.handle.is_finished())
                .map(|(id, _)| id.clone())
                .collect();
            let runs: Vec<(WorkItemId, Run)> = ids
                .into_iter()
                .filter_map(|id| state.runs.remove(&id).map(|r| (id, r)))
                .collect();
            (ready, runs)
        };
        for (id, run) in done_runs {
            self.record_run(&id, &run, "ended").await;
            if run.reported {
                continue;
            }
            let outcome: RunResult = match run.handle.await {
                Ok(result) => result,
                Err(e) => Err(Error::Internal(format!("worker run ended abnormally: {e}"))),
            };
            ready.push((
                id,
                Finished {
                    worker: run.worker,
                    outcome,
                },
            ));
        }
        ready.sort_by(|a, b| a.0.cmp(&b.0));
        for (id, finished) in ready {
            let mut b = Batch::new(self.load().await?, now);
            if !b.status(&id).is_some_and(|s| s.is_active()) {
                // Cancelled, expired or returned meanwhile: the result is
                // no longer the item's.
                continue;
            }
            self.reconcile_one(&mut b, &id, &finished).await?;
            let judged = std::mem::take(&mut b.judged);
            match self.commit(b, noticed).await {
                Ok(Some(written)) => {
                    absorb(report, written);
                    report.reconciled.push(id);
                    // Only once the transition is written, so a result
                    // deferred to the next tick is not counted twice.
                    for j in &judged {
                        self.routing.record(j).await;
                    }
                }
                Ok(None) => {
                    self.state().finished.insert(id, finished);
                }
                Err(e) => {
                    tracing::warn!(item = %id, error = %e, "reconcile not written; the lease will resume it");
                }
            }
        }
        Ok(())
    }

    /// Verify one result and write its consequences (section 5, step 5).
    async fn reconcile_one(
        &self,
        b: &mut Batch,
        id: &str,
        finished: &Finished,
    ) -> Result<(), Error> {
        let Some(item) = b.snap.item(id).cloned() else {
            return Ok(());
        };
        let worker = finished.worker.as_str();
        let mut changed = match &finished.outcome {
            Err(e) => {
                // A typed `RunFailure` (a spent budget, a model that never
                // reported) comes back typed; anything else as the core
                // error reports it.
                let error = classify(
                    &run_failure_input(e),
                    &Context {
                        tool: None,
                        worker_kind: Some(self.worker_kind(worker)),
                    },
                );
                self.fail(b, &item, worker, error, &[]).await?
            }
            Ok(result) => self.judge(b, &item, worker, result).await?,
        };
        if let Ok(result) = &finished.outcome {
            changed.extend(self.file_discovered(b, &item, worker, &result.discovered));
        }
        b.settle(changed);
        Ok(())
    }

    /// Judge a report (section 5: every result is a claim): an error is a
    /// failure; `needs_tool` climbs the ladder's order 2 (a tool that
    /// exists is loaded, one that does not is built); any other typed block
    /// parks (a cascade reason is not the worker's to claim); questions
    /// park as `needs_decision`; a `done` claim with an empty summary, or
    /// with claims the evidence does not bear out ([`Self::verify_claims`]),
    /// fails verification; otherwise the item is `done`, its artifacts
    /// evidence verified by the report and its checked claims evidence
    /// verified by what checked them.
    async fn judge(
        &self,
        b: &mut Batch,
        item: &WorkItem,
        worker: &str,
        result: &rustykrab_core::work::ResultReport,
    ) -> Result<Vec<WorkItemId>, Error> {
        let kind = self.worker_kind(worker);
        let ctx = Context {
            tool: None,
            worker_kind: Some(kind),
        };
        if let Some(error) = &result.error {
            let error = normalise_reported(error, kind);
            return self.fail(b, item, worker, error, &result.artifacts).await;
        }
        if let Some(blocked) = &result.blocked {
            if blocked.reason.is_cascade() {
                let error = classify(
                    &FailureInput::Verifier {
                        verdict: VerifierVerdict::ClaimMismatch,
                        detail: format!(
                            "the worker reported {}, which only the controller's cascade sets",
                            Status::Blocked(blocked.reason)
                        ),
                    },
                    &ctx,
                );
                return self.fail(b, item, worker, error, &result.artifacts).await;
            }
            if blocked.reason == BlockedReason::NeedsTool {
                // Section 7: `needs_tool` is a capability gap for the
                // ladder, never a question. Order 2 loads a tool the host
                // has and builds one it does not (section 8).
                let tool = blocked
                    .needs
                    .first()
                    .cloned()
                    .unwrap_or_else(|| blocked.detail.clone());
                let error = classify(
                    &FailureInput::CapabilityGap {
                        gap: GapKind::Tool,
                        name: tool,
                    },
                    &ctx,
                );
                return self.fail(b, item, worker, error, &result.artifacts).await;
            }
            // No item parks with a blank question: a blocked report that
            // says nothing falls back to the report's summary, then to a
            // sentence naming the reason.
            let detail = [blocked.detail.trim(), result.summary.trim()]
                .into_iter()
                .find(|s| !s.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| {
                    format!(
                        "the worker reported {} without saying what it needs",
                        Status::Blocked(blocked.reason)
                    )
                });
            let text = if blocked.needs.is_empty() {
                detail
            } else {
                format!("{detail} (needs {})", blocked.needs.join(", "))
            };
            return self.park(b, item, blocked.reason, &text).await;
        }
        if !result.questions.is_empty() {
            let text = result
                .questions
                .iter()
                .map(|q| {
                    if q.options.is_empty() {
                        q.text.clone()
                    } else {
                        format!("{} [{}]", q.text, q.options.join(" / "))
                    }
                })
                .collect::<Vec<_>>()
                .join(" ");
            return self
                .park(b, item, BlockedReason::NeedsDecision, &text)
                .await;
        }
        if result.summary.trim().is_empty() {
            let error = classify(
                &FailureInput::Verifier {
                    verdict: VerifierVerdict::Incomplete,
                    detail: "the report claims done with an empty summary".to_string(),
                },
                &ctx,
            );
            return self.fail(b, item, worker, error, &result.artifacts).await;
        }
        let rules = match self.replay_rules(item, result).await? {
            Ok(rules) => rules,
            Err(why) => {
                let error = classify(
                    &FailureInput::Verifier {
                        verdict: VerifierVerdict::ClaimMismatch,
                        detail: why,
                    },
                    &ctx,
                );
                return self.fail(b, item, worker, error, &result.artifacts).await;
            }
        };
        let proofs = match self.verify_claims(item, result).await? {
            Ok(proofs) => proofs,
            Err((verdict, why)) => {
                let error = classify(
                    &FailureInput::Verifier {
                        verdict,
                        detail: why,
                    },
                    &ctx,
                );
                return self.fail(b, item, worker, error, &result.artifacts).await;
            }
        };
        for artifact in &result.artifacts {
            // A landed rule is verified by the replay above, not by being
            // in the report; an adapter's command record is what checks
            // were verified against.
            let verified = if item.kind == WorkKind::Internal && artifact.kind == CLASSIFIER_RULE {
                REPLAYED
            } else {
                RESULT_REPORT
            };
            b.evidence(&item.id, &artifact.kind, &artifact.value, Some(verified));
        }
        b.learned.extend(rules);
        b.evidence(&item.id, SUMMARY, result.summary.trim(), None);
        let proved = |kind: &str| proofs.iter().any(|(k, _, _)| k == kind);
        for (kind, reference, by) in &proofs {
            b.evidence(&item.id, kind, reference, by.as_deref());
        }
        // Claims nothing checked stay on the record, unverified.
        if !proved("changed_path") {
            for path in &result.changed_paths {
                b.evidence(&item.id, "changed_path", path, None);
            }
        }
        if !proved("commit") {
            if let Some(commit) = &result.commit {
                b.evidence(&item.id, "commit", commit, None);
            }
        }
        if !proved("check_run") {
            for check in &result.checks_run {
                b.evidence(&item.id, "check_run", check, None);
            }
        }
        for limit in &result.known_limits {
            b.evidence(&item.id, "known_limit", limit, None);
        }
        self.judged(b, item, worker, Verdict::Verified).await?;
        b.own(
            &item.id,
            Status::Verifying,
            "controller",
            "checking the result report",
            None,
        );
        Ok(b.close(
            &item.id,
            Status::Done,
            "controller",
            format!(
                "verified on {worker}: {} artifact(s); {}",
                result.artifacts.len(),
                first_line(&result.summary)
            ),
        ))
    }

    /// The classifier rules an `internal` item's report lands (section 9,
    /// scenario 14), each checked as the item's `done_when` says: replaying
    /// a failure the item was filed for (the `item` refs it carries) through
    /// the classifier with the rule must yield a class other than
    /// `unknown`. `Err` names the rule that fails, which fails the report.
    /// Any other item's rules are not rules, just artifacts.
    async fn replay_rules(
        &self,
        item: &WorkItem,
        result: &rustykrab_core::work::ResultReport,
    ) -> Result<Result<Vec<LearnedRule>, String>, Error> {
        if item.kind != WorkKind::Internal {
            return Ok(Ok(Vec::new()));
        }
        let raw: Vec<&str> = result
            .artifacts
            .iter()
            .filter(|a| a.kind == CLASSIFIER_RULE)
            .map(|a| a.value.as_str())
            .collect();
        if raw.is_empty() {
            return Ok(Ok(Vec::new()));
        }
        let mut failures: Vec<WorkError> = Vec::new();
        for r in item.artifact_refs.iter().filter(|r| r.kind == "item") {
            if let Some(e) = ladder::last_error(&self.store.work_events(&r.value).await?) {
                failures.push(e);
            }
        }
        let ctx = Context {
            tool: None,
            worker_kind: None,
        };
        let mut rules = Vec::new();
        for text in raw {
            let rule = match LearnedRule::parse(&item.id, text) {
                Ok(rule) => rule,
                Err(why) => return Ok(Err(format!("classifier_rule {text:?}: {why}"))),
            };
            let replays = failures.iter().any(|f| {
                apply_learned(f.clone(), std::slice::from_ref(&rule), &ctx).class
                    != ErrorClass::Unknown
            });
            if !replays {
                return Ok(Err(format!(
                    "classifier_rule {text:?} does not classify the failure this item was filed for"
                )));
            }
            rules.push(rule);
        }
        Ok(Ok(rules))
    }

    /// What the controller checks of a claimed result beyond its summary
    /// (section 5), as evidence rows to add, or the verdict and why when
    /// the result claims more than the evidence shows:
    ///
    /// - a `code` item's commit and changed paths, against the run's
    ///   workspace ([`workspace::verify`]); a `code` result that claims a
    ///   commit or paths with no workspace to check them against claims
    ///   beyond the evidence;
    /// - every check it names, against the commands its adapter recorded
    ///   the agent running ([`COMMAND_RUN`]), where any were recorded;
    /// - a capability build of a tool: that the host now has the tool.
    async fn verify_claims(
        &self,
        item: &WorkItem,
        result: &rustykrab_core::work::ResultReport,
    ) -> Result<Result<Vec<Proof>, (VerifierVerdict, String)>, Error> {
        let mut proofs: Vec<Proof> = Vec::new();
        if item.kind == WorkKind::Code {
            match self.workspace_of(&item.id).await? {
                Some(ws) => {
                    let claim_commit = result.commit.clone();
                    let claim_paths = result.changed_paths.clone();
                    let verdict = tokio::task::spawn_blocking(move || {
                        workspace::verify(
                            &ws,
                            CodeClaim {
                                commit: claim_commit.as_deref(),
                                changed_paths: &claim_paths,
                            },
                        )
                    })
                    .await
                    .map_err(|e| Error::Internal(format!("verification task failed: {e}")))?;
                    match verdict {
                        Ok(CodeVerdict::Verified {
                            commit,
                            changed_paths,
                        }) => {
                            proofs.push(("commit".into(), commit, Some(BY_GIT.into())));
                            for path in changed_paths {
                                proofs.push(("changed_path".into(), path, Some(BY_GIT.into())));
                            }
                        }
                        Ok(CodeVerdict::Mismatch(why)) => {
                            return Ok(Err((VerifierVerdict::ClaimMismatch, why)))
                        }
                        Ok(CodeVerdict::Incomplete(why)) => {
                            return Ok(Err((VerifierVerdict::Incomplete, why)))
                        }
                        Err(why) => {
                            return Ok(Err((
                                VerifierVerdict::Incomplete,
                                format!("the claim could not be checked: {why}"),
                            )))
                        }
                    }
                }
                None if result.commit.is_some() || !result.changed_paths.is_empty() => {
                    return Ok(Err((
                        VerifierVerdict::ClaimMismatch,
                        "the result claims a commit or changed paths, but the item has no \
                         workspace to check them against (name its repository as a \
                         repo:<path> writable resource)"
                            .to_string(),
                    )));
                }
                None => {}
            }
        }
        let ran: Vec<String> = result
            .artifacts
            .iter()
            .filter(|a| a.kind == COMMAND_RUN)
            .map(|a| a.value.to_lowercase())
            .collect();
        if !ran.is_empty() {
            for check in &result.checks_run {
                let wanted = claimed_check(check);
                if wanted.is_empty() {
                    continue;
                }
                if !ran.iter().any(|c| check_ran_in(&wanted, c)) {
                    return Ok(Err((
                        VerifierVerdict::ClaimMismatch,
                        format!(
                            "the result says the check `{check}` ran, but it is not among \
                             the {} command(s) the worker ran",
                            ran.len()
                        ),
                    )));
                }
                proofs.push(("check_run".into(), check.clone(), Some(COMMAND_RUN.into())));
            }
        }
        // A capability build of a tool (its facet mode `build`, its need a
        // tool): the host must now have the tool.
        if item.kind == WorkKind::Capability {
            let mode = self.capability_mode(&item.id).await?;
            if let Some(tool) = built_tool(mode, &item.artifact_refs) {
                self.catalog.refresh();
                if self.catalog.tool_state(&tool) == ToolState::Unknown {
                    return Ok(Err((
                        VerifierVerdict::ClaimMismatch,
                        format!(
                            "the result says the tool is built, but the host has no tool named \
                             `{tool}`"
                        ),
                    )));
                }
                proofs.push(("tool".into(), tool, Some(BY_CATALOG.into())));
            }
        }
        Ok(Ok(proofs))
    }

    /// A capability item's mode: its review facet, the one source of
    /// truth for build, acquisition or request. `None` for any other item
    /// and for a capability without one.
    pub(super) async fn capability_mode(
        &self,
        item: &str,
    ) -> Result<Option<rustykrab_core::work::CapabilityMode>, Error> {
        Ok(self
            .store
            .work_facets_get(item)
            .await?
            .and_then(|f| f.capability))
    }

    /// The routed class of `item` (section 10), read with its facet.
    pub(super) async fn class_of(&self, item: &WorkItem) -> Result<Option<String>, Error> {
        let mode = match item.kind {
            WorkKind::Capability => self.capability_mode(&item.id).await?,
            _ => None,
        };
        Ok(work_class(item, mode))
    }

    /// The workspace the item's latest run was given, from its lease-time
    /// evidence.
    async fn workspace_of(&self, item: &str) -> Result<Option<Workspace>, Error> {
        Ok(self
            .store
            .work_evidence_list(item)
            .await?
            .iter()
            .rev()
            .find(|e| e.kind == WORKSPACE_EVIDENCE)
            .and_then(|e| serde_json::from_str(&e.reference).ok()))
    }

    /// Queue a judged result of a routed class for its worker's routing
    /// record (section 10), written once the batch is.
    async fn judged(
        &self,
        b: &mut Batch,
        item: &WorkItem,
        worker: &str,
        verdict: Verdict,
    ) -> Result<(), Error> {
        let Some(class) = self.class_of(item).await? else {
            return Ok(());
        };
        let repairs = self.ladder_of(item).await?.used(Rung::Repair);
        // The run this result came from: the item's latest `run` pointer.
        let run = self
            .store
            .work_evidence_list(&item.id)
            .await?
            .into_iter()
            .rev()
            .find(|e| e.kind == RUN)
            .map(|e| e.reference);
        let usage = run
            .as_deref()
            .and_then(|r| self.worker(worker).and_then(|w| w.usage(r)));
        let wall_seconds = match (usage, self.store.work_lease_get(&item.id).await?) {
            (Some(u), _) if u.wall_ms > 0 => u.wall_ms / 1_000,
            (_, Some(lease)) => {
                u64::try_from((Utc::now() - lease.since).num_seconds()).unwrap_or(0)
            }
            _ => 0,
        };
        b.judged.push(Judged {
            worker: worker.to_string(),
            worker_kind: self.worker_kind(worker),
            item: item.id.clone(),
            class,
            verdict,
            repairs,
            wall_seconds,
            usage,
            at: b.now,
        });
        Ok(())
    }

    /// A failed run: its partial artifacts and its error as evidence, then
    /// the ladder.
    async fn fail(
        &self,
        b: &mut Batch,
        item: &WorkItem,
        worker: &str,
        error: WorkError,
        artifacts: &[rustykrab_core::work::ArtifactRef],
    ) -> Result<Vec<WorkItemId>, Error> {
        // What the built-in classifiers left unknown, a landed rule may
        // know (section 9, scenario 14).
        let error = apply_learned(
            error,
            &self.state().learned,
            &Context {
                tool: None,
                worker_kind: Some(self.worker_kind(worker)),
            },
        );
        for artifact in artifacts {
            b.evidence(&item.id, &artifact.kind, &artifact.value, None);
        }
        b.evidence.push(rustykrab_core::work::Evidence {
            item: item.id.clone(),
            kind: ERROR.to_string(),
            reference: format!(
                "{}/{}: {}",
                error.class.as_str(),
                error.subclass.as_str(),
                error.detail
            ),
            hash: Some(error.fingerprint.clone()),
            verified_by: Some(error.observed_by.clone()),
            at: b.now,
        });
        // A claim beyond the evidence is recorded as what it is (section
        // 5): the item passes through `verifying` into
        // `blocked(verification_failed)` before the ladder moves it on.
        if error.class == ErrorClass::Verification
            && b.status(&item.id).is_some_and(|s| s.is_active())
        {
            b.own(
                &item.id,
                Status::Verifying,
                "controller",
                "checking the result report",
                None,
            );
            b.move_to(
                &item.id,
                Status::Blocked(BlockedReason::VerificationFailed),
                "controller",
                format!("verification failed on {worker}: {}", error.detail),
            );
        }
        if let Some(verdict) = verdict_of(&error) {
            self.judged(b, item, worker, verdict).await?;
        }
        self.climb(b, item, worker, error).await
    }

    /// A block or a question only the user can answer (section 7): the
    /// item's plan B runs first when it has one (6.4); otherwise the item
    /// parks in its typed state and its root's notice asks.
    async fn park(
        &self,
        b: &mut Batch,
        item: &WorkItem,
        reason: BlockedReason,
        text: &str,
    ) -> Result<Vec<WorkItemId>, Error> {
        let mut state = self.ladder_of(item).await?;
        if reason.needs_user() && has_open_plan_b(&b.snap, &item.id) {
            b.rung(
                &item.id,
                &mut state,
                RungEvent {
                    rung: Rung::PlanB,
                    at: b.now,
                    error: None,
                    outcome: format!("{}: {text}; plan B runs before asking", reason.as_str()),
                },
            );
            return Ok(b.close(
                &item.id,
                Status::Failed,
                "controller",
                format!("{} with a plan B: the plan B runs", reason.as_str()),
            ));
        }
        b.rung(
            &item.id,
            &mut state,
            RungEvent {
                rung: Rung::Surface,
                at: b.now,
                error: None,
                outcome: format!("parked {}: {text}", reason.as_str()),
            },
        );
        let changed = b.move_to(&item.id, Status::Blocked(reason), "controller", text);
        b.notify(
            &item.id,
            Cause::Asked {
                item: item.id.clone(),
                text: format!("{} {}: {text}", label(item), reason.as_str()),
            },
        );
        Ok(changed)
    }

    // ── climb the ladder (sections 8 and 6.4) ───────────────────────────

    /// Ask the ladder for the next move on `error` and carry it out, every
    /// rung an event on the item. Returns the ids to settle.
    pub(super) async fn climb(
        &self,
        b: &mut Batch,
        item: &WorkItem,
        worker: &str,
        error: WorkError,
    ) -> Result<Vec<WorkItemId>, Error> {
        let events = self.store.work_events(&item.id).await?;
        let mut state = ladder_from(&events, item);
        let mut excluded = history(&events).excluded;
        excluded.insert(worker.to_string());

        // Recurrence counts items, not attempts: the same fingerprint on
        // the same item counts once.
        let first_here = !state.history.iter().any(|e| {
            e.error
                .as_ref()
                .is_some_and(|x| x.fingerprint == error.fingerprint)
        });
        let seen = self.state().recurrence.count(&error.fingerprint) + u32::from(first_here);
        if first_here {
            b.observe.push(error.fingerprint.clone());
        }
        let replans = match item.parent.as_deref().and_then(|p| b.snap.item(p)).cloned() {
            Some(parent) => self.ladder_of(&parent).await?.left(Rung::Replan),
            None => 0,
        };
        let has_plan_b = has_open_plan_b(&b.snap, &item.id);
        let gap = gap_of(&error);
        let user_gap = gap
            .as_ref()
            .is_some_and(|g| matches!(g.kind, GapKind::Credential | GapKind::Consent));
        let tool_exists = match &gap {
            Some(g) if g.kind == GapKind::Tool => {
                self.catalog.refresh();
                Some(self.catalog.tool_state(&g.subject) != ToolState::Unknown)
            }
            _ => None,
        };
        // A failed verification escalates: the switch rung looks above the
        // failing worker's tier, not beside it (sections 5 and 8).
        let floor = (error.class == ErrorClass::Verification)
            .then(|| {
                self.worker(worker)
                    .map(|w| self.routing.cost_tier(w.as_ref()))
            })
            .flatten()
            .into_iter()
            .chain(history(&events).floor)
            .max();
        let class = self.class_of(item).await?;
        let ctx = LadderContext {
            error: &error,
            recurrence_count: seen,
            promote_threshold: self.config.promote_threshold,
            has_plan_b,
            replans_left_on_parent: replans,
            policy_stop: error.class == ErrorClass::Policy,
            only_user_can_meet: user_gap && has_plan_b,
            tool_exists,
        };
        let id = item.id.clone();
        for _ in 0..MAX_CLIMB {
            let decision = ladder::next(&state, &ctx);
            let rung = decision.rung();
            match decision {
                Decision::Improve { .. } => {
                    let outcome = match self.file_internal(b, item, &error) {
                        Ok(filed) => format!("filed internal item {filed}"),
                        Err(why) => format!("internal item not filed: {why}"),
                    };
                    self.rung(b, &id, &mut state, rung, &error, outcome);
                }
                Decision::SwitchWorker
                    if !self.has_alternative(item, class.as_deref(), &excluded, floor) =>
                {
                    self.rung(
                        b,
                        &id,
                        &mut state,
                        rung,
                        &error,
                        "skipped: no other worker qualifies".to_string(),
                    );
                }
                Decision::SwitchWorker => {
                    let outcome = match floor {
                        Some(tier) => format!("{SWITCHED}{worker}{ESCALATING_ABOVE}{tier}"),
                        None => format!("{SWITCHED}{worker}"),
                    };
                    self.rung(b, &id, &mut state, rung, &error, outcome);
                    return Ok(b.move_to(&id, Status::Queued, "controller", "switch worker"));
                }
                Decision::Retry => {
                    self.rung(
                        b,
                        &id,
                        &mut state,
                        rung,
                        &error,
                        format!("retry on {worker}"),
                    );
                    return Ok(b.move_to(&id, Status::Queued, "controller", "retry"));
                }
                Decision::Repair { with_diagnosis } => {
                    let outcome = if with_diagnosis {
                        "repair with the failure evidence and a diagnosis step"
                    } else {
                        "repair with the failure evidence"
                    };
                    self.rung(b, &id, &mut state, rung, &error, outcome.to_string());
                    return Ok(b.move_to(&id, Status::Queued, "controller", "repair"));
                }
                Decision::Acquire(need) | Decision::Build(need) | Decision::Request(need) => {
                    let reason = parked_reason(need.gap);
                    let mut changed = b.move_to(
                        &id,
                        Status::Blocked(reason),
                        "controller",
                        format!(
                            "waiting on a capability: {} {}",
                            need.gap.as_str(),
                            need.subject
                        ),
                    );
                    match self.file_capability(b, item, rung, &need) {
                        Ok(capability) => {
                            self.rung(
                                b,
                                &id,
                                &mut state,
                                rung,
                                &error,
                                format!("filed capability item {capability}"),
                            );
                            if reason.needs_user() {
                                b.notify(
                                    &id,
                                    Cause::Asked {
                                        item: id.clone(),
                                        text: format!(
                                            "{} needs the {} {}",
                                            label(item),
                                            need.gap.as_str(),
                                            need.subject
                                        ),
                                    },
                                );
                            }
                            changed.push(capability);
                            return Ok(changed);
                        }
                        Err(why) => {
                            self.rung(
                                b,
                                &id,
                                &mut state,
                                rung,
                                &error,
                                format!("capability item not filed: {why}"),
                            );
                        }
                    }
                }
                Decision::PlanB => {
                    self.rung(
                        b,
                        &id,
                        &mut state,
                        rung,
                        &error,
                        "failed; plan B released".into(),
                    );
                    return Ok(b.close(
                        &id,
                        Status::Failed,
                        "controller",
                        "ladder spent; plan B runs",
                    ));
                }
                Decision::Replan => {
                    // The parent's re-plan is Phase 4: the item fails, the
                    // cascade holds what is behind it, and the parent
                    // surfaces once (6.4, step 4).
                    self.rung(
                        b,
                        &id,
                        &mut state,
                        rung,
                        &error,
                        "failed; no re-plan yet, surfaced at the parent".into(),
                    );
                    let changed = b.close(
                        &id,
                        Status::Failed,
                        "controller",
                        "ladder spent; surfaced at the parent",
                    );
                    b.notify(
                        &id,
                        Cause::Asked {
                            item: id.clone(),
                            text: format!(
                                "{} failed after its ladder ({}/{}). How should the rest go on?",
                                label(item),
                                error.class.as_str(),
                                error.subclass.as_str()
                            ),
                        },
                    );
                    return Ok(changed);
                }
                Decision::Surface(surfacing) => {
                    let reason = surface_reason(&error, surfacing.reason);
                    self.rung(
                        b,
                        &id,
                        &mut state,
                        Rung::Surface,
                        &error,
                        surfacing.ask.clone(),
                    );
                    let changed = b.move_to(
                        &id,
                        Status::Blocked(reason),
                        "controller",
                        surfacing.ask.clone(),
                    );
                    b.notify(
                        &id,
                        Cause::Asked {
                            item: id.clone(),
                            text: surfacing.ask,
                        },
                    );
                    return Ok(changed);
                }
                Decision::Park(reason) => {
                    self.rung(
                        b,
                        &id,
                        &mut state,
                        Rung::Surface,
                        &error,
                        format!("parked: {}", reason.as_str()),
                    );
                    let changed = b.move_to(
                        &id,
                        Status::Blocked(reason),
                        "controller",
                        error.detail.clone(),
                    );
                    b.notify(
                        &id,
                        Cause::Asked {
                            item: id.clone(),
                            text: format!("{} {}: {}", label(item), reason.as_str(), error.detail),
                        },
                    );
                    return Ok(changed);
                }
            }
        }
        // The climb did not settle within its bound: surface rather than loop.
        let reason = surface_reason(&error, SurfaceReason::LadderSpent);
        self.rung(
            b,
            &id,
            &mut state,
            Rung::Surface,
            &error,
            "the ladder did not settle".into(),
        );
        let changed = b.move_to(
            &id,
            Status::Blocked(reason),
            "controller",
            "the ladder did not settle",
        );
        b.notify(
            &id,
            Cause::Asked {
                item: id.clone(),
                text: format!("{} could not be resolved: {}", label(item), error.detail),
            },
        );
        Ok(changed)
    }

    fn rung(
        &self,
        b: &mut Batch,
        item: &str,
        state: &mut LadderState,
        rung: Rung,
        error: &WorkError,
        outcome: String,
    ) {
        b.rung(
            item,
            state,
            RungEvent {
                rung,
                at: b.now,
                error: Some(error.clone()),
                outcome,
            },
        );
    }

    /// Whether a healthy worker other than `excluded`, and above `floor`'s
    /// cost tier when one is set, could take `item`.
    fn has_alternative(
        &self,
        item: &WorkItem,
        class: Option<&str>,
        excluded: &HashSet<String>,
        floor: Option<u32>,
    ) -> bool {
        self.registry.workers().iter().any(|w| {
            !excluded.contains(w.name())
                && w.healthy()
                && covers(w.as_ref(), item)
                && self.routing.qualifies(w.as_ref(), item, class)
                && floor.is_none_or(|f| self.routing.cost_tier(w.as_ref()) > f)
        })
    }

    // ── 3. select, match, lease and run ─────────────────────────────────

    /// How many items a worker may run at once: one per local worker under
    /// the 12.1 rule, else what it advertises.
    fn capacity(&self, worker: &dyn Worker) -> usize {
        if self.config.serialise_local && worker.kind() == WorkerKind::Local {
            1
        } else {
            worker.concurrency().max(1)
        }
    }

    async fn select_and_lease(&self, report: &mut TickReport) -> Result<(), Error> {
        let snap = self.load().await?;
        let mut ready: Vec<WorkItem> = snap
            .items()
            .iter()
            .filter(|i| i.status == Status::Ready && !snap.has_children(&i.id))
            .cloned()
            .collect();
        ready.sort_by(|a, b| {
            b.priority
                .cmp(&a.priority)
                .then_with(|| a.created_at.cmp(&b.created_at))
                .then_with(|| a.id.cmp(&b.id))
        });
        // The single-writer rule, over every active item whatever its tree.
        let mut busy: HashSet<String> = snap
            .items()
            .iter()
            .filter(|i| i.status.is_active() && !snap.has_children(&i.id))
            .flat_map(|i| i.writable_resources.iter().cloned())
            .collect();
        let (mut load, mut models) = self.load_by_worker();
        for item in ready {
            if item.writable_resources.iter().any(|r| busy.contains(r)) {
                continue;
            }
            if !item
                .preconditions
                .iter()
                .all(|p| self.catalog.precondition_holds(p))
            {
                continue;
            }
            let events = self.store.work_events(&item.id).await?;
            let hist = history(&events);
            let mode = match item.kind {
                WorkKind::Capability => self.capability_mode(&item.id).await?,
                _ => None,
            };
            let class = work_class(&item, mode);
            let Some(worker) = self.pick(
                &item,
                class.as_deref(),
                &hist.excluded,
                hist.floor,
                &load,
                &models,
            ) else {
                continue;
            };
            let (inputs, more) = self.build_inputs(&snap, &item).await?;
            let prior = if hist.repair.is_some() {
                self.store.work_evidence_list(&item.id).await?
            } else {
                Vec::new()
            };
            let mut brief = brief_for(&item, inputs.clone(), more, prior, &hist);
            let run_id = match self.continued_conversation(&item.id).await {
                Some(conversation) => conversation,
                None => uuid::Uuid::new_v4().to_string(),
            };
            brief.run = Some(run_id.clone());
            brief.workspace = self.plan_workspace(&item, &run_id).await;
            brief.capability = mode;
            if let Err(e) = self
                .store
                .work_lease_acquire(
                    &item.id,
                    worker.name(),
                    self.config.lease_ttl_seconds,
                    inputs,
                )
                .await
            {
                tracing::warn!(item = %item.id, error = %e, "lease refused");
                continue;
            }
            // The run's pointer, before it starts: whatever happens to the
            // run (a cancel, a restart), the item keeps where its partial
            // work is.
            if let Err(e) = self
                .store
                .work_evidence_add(rustykrab_core::work::Evidence {
                    item: item.id.clone(),
                    kind: RUN.to_string(),
                    reference: run_id,
                    hash: None,
                    verified_by: None,
                    at: Utc::now(),
                })
                .await
            {
                tracing::warn!(item = %item.id, error = %e, "run pointer not recorded");
            }
            // The workspace the controller pinned, which verification reads
            // back: the parent commit is the controller's, not the worker's.
            if let Some(ws) = &brief.workspace {
                let recorded = self
                    .store
                    .work_evidence_add(rustykrab_core::work::Evidence {
                        item: item.id.clone(),
                        kind: WORKSPACE_EVIDENCE.to_string(),
                        reference: serde_json::to_string(ws).unwrap_or_default(),
                        hash: Some(ws.base.clone()),
                        verified_by: None,
                        at: Utc::now(),
                    })
                    .await;
                if let Err(e) = recorded {
                    tracing::warn!(item = %item.id, error = %e, "workspace not recorded");
                }
            }
            let name = worker.name().to_string();
            let run = spawn(worker.clone(), brief, self.clock.now());
            self.state().runs.insert(item.id.clone(), run);
            let mut spec = TransitionSpec::new(&item.id, Status::Running, format!("worker:{name}"));
            spec.expected_from = Some(Status::Leased);
            spec.reason = Some("run started".to_string());
            match self.store.work_apply(vec![WorkOp::Transition(spec)]).await {
                Ok(_) => report.transitions += 2,
                Err(e) => {
                    report.transitions += 1;
                    tracing::warn!(item = %item.id, error = %e, "running not recorded");
                }
            }
            busy.extend(item.writable_resources.iter().cloned());
            *load.entry(name).or_default() += 1;
            if self.config.serialise_local && worker.kind() == WorkerKind::Local {
                models.extend(worker.capabilities().models);
            }
            report.leased.push(item.id);
        }
        Ok(())
    }

    /// The workspace a `code` run of `item` gets (section 5): its
    /// repository's `HEAD` pinned as the parent commit, a new branch and a
    /// worktree under the configured root. `None` for other items, without
    /// a root, or when the repository has no commit to start from.
    async fn plan_workspace(&self, item: &WorkItem, run: &str) -> Option<Workspace> {
        if item.kind != WorkKind::Code {
            return None;
        }
        let root = self.config.worktree_root.clone()?;
        let repo = Workspace::repo_of(&item.writable_resources)?;
        let at = repo.clone();
        let base = tokio::task::spawn_blocking(move || workspace::head(&at))
            .await
            .ok()?;
        match base {
            Ok(base) => Some(Workspace::plan(&root, &repo, &base, &item.id, run)),
            Err(why) => {
                tracing::warn!(item = %item.id, %why, "no workspace: the repository has no commit");
                None
            }
        }
    }

    /// Live runs per worker, and the models local runs occupy.
    fn load_by_worker(&self) -> (HashMap<String, usize>, HashSet<String>) {
        let state = self.state();
        let mut load: HashMap<String, usize> = HashMap::new();
        let mut models: HashSet<String> = HashSet::new();
        for run in state.runs.values() {
            *load.entry(run.worker.clone()).or_default() += 1;
            if let Some(w) = self.worker(&run.worker) {
                if self.config.serialise_local && w.kind() == WorkerKind::Local {
                    models.extend(w.capabilities().models);
                }
            }
        }
        (load, models)
    }

    /// Step 2: the cheapest tier of healthy workers that cover the item,
    /// whose routing record qualifies them for its class and, after a
    /// failed verification escalated the item, above `floor`'s tier; then
    /// a worker of that tier with room and (for a local worker) whose model
    /// is not busy. When every worker of the cheapest tier is busy the item
    /// waits for one: a tier is escalated on failure, never on load (plan
    /// sections 6 step 2 and 17, "cheapest-qualifying worker, and escalation
    /// only on failure").
    fn pick(
        &self,
        item: &WorkItem,
        class: Option<&str>,
        excluded: &HashSet<String>,
        floor: Option<u32>,
        load: &HashMap<String, usize>,
        models: &HashSet<String>,
    ) -> Option<Arc<dyn Worker>> {
        let workers = self.registry.workers();
        let qualified: Vec<(u32, usize, &Arc<dyn Worker>)> = workers
            .iter()
            .enumerate()
            .filter(|(_, w)| {
                w.healthy()
                    && !excluded.contains(w.name())
                    && covers(w.as_ref(), item)
                    && self.routing.qualifies(w.as_ref(), item, class)
                    && floor.is_none_or(|f| self.routing.cost_tier(w.as_ref()) > f)
            })
            .map(|(i, w)| (self.routing.cost_tier(w.as_ref()), i, w))
            .collect();
        let cheapest = qualified.iter().map(|(tier, _, _)| *tier).min()?;
        qualified
            .into_iter()
            .filter(|(tier, _, w)| {
                let local = self.config.serialise_local && w.kind() == WorkerKind::Local;
                *tier == cheapest
                    && load.get(w.name()).copied().unwrap_or(0) < self.capacity(w.as_ref())
                    && !(local
                        && w.capabilities()
                            .models
                            .iter()
                            .any(|m| models.contains(m) || self.activity.busy(m)))
            })
            .min_by_key(|(_, i, _)| *i)
            .map(|(_, _, w)| Arc::clone(w))
    }

    // ── 4. aging ────────────────────────────────────────────────────────

    /// Compact what 4.6 lets age: at idle, when nothing runs, and otherwise
    /// at least every `aging_max_gap`, so steady load cannot starve it.
    async fn age(&self, now: DateTime<Utc>, report: &mut TickReport) -> Result<(), Error> {
        {
            let mut state = self.state();
            let idle = state.runs.is_empty();
            let overdue = state
                .last_aged
                .is_none_or(|at| now - at >= self.config.aging_max_gap);
            if !idle && !overdue {
                return Ok(());
            }
            state.last_aged = Some(now);
        }
        let snap = self.load().await?;
        let ids = graph::aging_candidates(&snap, now, &self.config.aging);
        if ids.is_empty() {
            return Ok(());
        }
        self.store.work_archive_compact(&ids, now).await?;
        report.archived.extend(ids);
        Ok(())
    }
}

/// Start `worker` on `brief` under tokio, with the run binding the work
/// tools read their provenance from.
pub(super) fn spawn(worker: Arc<dyn Worker>, brief: Brief, since: DateTime<Utc>) -> Run {
    let name = worker.name().to_string();
    let run_id = brief.run.clone().unwrap_or_default();
    let binding = WorkRunContext {
        item: brief.item.clone(),
        actor: format!("worker:{name}"),
    };
    let handle =
        tokio::spawn(WORK_RUN_CONTEXT.scope(binding, async move { worker.run(brief).await }));
    Run {
        worker: name,
        run_id,
        started: std::time::Instant::now(),
        handle,
        since,
        reported: false,
    }
}

#[cfg(test)]
mod tests {
    use super::{check_ran_in, claimed_check};

    fn verifies(claim: &str, commands: &[&str]) -> bool {
        let wanted = claimed_check(claim);
        commands.iter().any(|c| check_ran_in(&wanted, c))
    }

    #[test]
    fn an_annotated_claim_verifies() {
        assert_eq!(
            claimed_check("  cargo test -p rustykrab-e2e --no-default-features (86 passed) "),
            "cargo test -p rustykrab-e2e --no-default-features"
        );
        assert!(verifies(
            "cargo test -p rustykrab-e2e --no-default-features (86 passed)",
            &["cargo test -p rustykrab-e2e --no-default-features"],
        ));
    }

    #[test]
    fn a_check_inside_a_compound_command_verifies() {
        let ran = [
            "cd /tmp/wt && cargo fmt --all -- --check && cargo clippy --workspace --all-targets \
             && cargo test -p rustykrab-e2e --no-default-features",
        ];
        assert!(verifies(
            "cargo test -p rustykrab-e2e --no-default-features (86 passed)",
            &ran,
        ));
        assert!(verifies("cargo fmt --all -- --check", &ran));
        assert!(verifies("cargo clippy --workspace --all-targets", &ran));
        assert!(verifies("echo done", &["cargo check || true; echo done"]));
    }

    #[test]
    fn a_check_inside_a_piped_command_verifies() {
        let ran = ["cargo test -p rustykrab-control --no-default-features 2>&1 | tail -20"];
        assert!(verifies(
            "cargo test -p rustykrab-control --no-default-features (312 passed)",
            &ran,
        ));
        assert!(verifies("tail -20", &ran));
    }

    #[test]
    fn a_check_that_never_ran_does_not_verify() {
        let ran = [
            "cd /tmp/wt && cargo fmt --all -- --check",
            "cargo test -p rustykrab-control | tail -5",
        ];
        assert!(!verifies("cargo clippy --workspace --all-targets", &ran));
        assert!(!verifies(
            "cargo test -p rustykrab-e2e --no-default-features (86 passed)",
            &ran,
        ));
        assert!(!verifies("(86 passed)", &ran));
    }
}
