//! Questions and blocked states, carried out (plan section 7): the
//! router's verdicts ([`crate::questions`]) applied to the store.
//!
//! - A worker's typed block or questions ([`Controller::judge_questions`],
//!   from the reconcile step, and [`Controller::ask_locked`], from
//!   `ask_user` mid-run) are routed one by one. A blocking-now question
//!   parks the item in its typed state (`needs_decision`, `needs_consent`,
//!   `needs_credential`) and goes to the user at once as its root's
//!   message; a defaultable or delegated one is answered with the record and
//!   the item resumes; a researchable one files a `research` item the item
//!   waits on; a blocking-later one is recorded for the root's next message;
//!   an obsolete one closes. A question the item already had answered is
//!   the worker ignoring its answer: a failure for the ladder.
//! - An answer ([`Controller::answer_locked`], from REST, the CLI or the
//!   channel) settles the question, fires `on_answer` triggers, and resumes
//!   the item (not the conversation): it returns to `queued` and its next
//!   brief carries the answer ([`Controller::answers_for`]).
//! - Standing judgment ([`Controller::judgment`]) is the active grants
//!   folded over the configured baseline, cached until a grant changes.

use std::collections::{BTreeSet, HashSet};
use std::sync::Arc;

use crate::errors::{classify, Context, FailureInput, GapKind, ProviderProblem};
use crate::graph::FilingSource;
use crate::handle::{AnswerReply, JudgmentView};
use crate::questions::{
    compile, route, same_question, stated_default, Asked, Judgment, Route, RouteContext,
};
use chrono::{DateTime, Utc};
use rustykrab_core::questions::{DelegatedDecision, QuestionClass, QuestionKind, QuestionStatus};
use rustykrab_core::work::{
    ArtifactRef, BlockedReason, BlockedReport, EdgeKind, EventKind, ItemRef, PlanEdge, Question,
    ResultReport, Rung, RungEvent, Status, WorkItem, WorkItemDraft, WorkItemId, WorkKind, WorkPlan,
};
use rustykrab_core::{Error, ToolError};
use rustykrab_store::{JudgmentRow, QuestionFilter, QuestionRow, QuestionWrite, WorkOp};
use rustykrab_tools::work_backend::{AskOutcome, AskRequest, CapabilityAsk, Provenance};

use super::batch::{has_open_plan_b, Batch};
use super::filing::describe_rejection;
use super::notice::{label, short, short_question, Cause};
use super::{Controller, Finished};

/// The constraint that marks a `research` item the router filed: system
/// work, whose result goes back to the item that asked rather than to the
/// user (section 7).
pub(super) const RESEARCH_MARK: &str = "Filed by the question router";

/// A needs entry naming a question the run already recorded (`ask_user`,
/// `capability_request`), so the reconcile parks on it rather than asking
/// again.
const QUESTION_NEED: &str = "question:";

/// One question to record.
pub(super) struct NewQuestion {
    pub item: WorkItemId,
    pub kind: QuestionKind,
    pub class: QuestionClass,
    pub text: String,
    pub options: Vec<String>,
    pub default: Option<String>,
    pub asked_class: Option<String>,
    pub rule: &'static str,
    pub asked_by: String,
    pub status: QuestionStatus,
    /// `(answer, answered_by, decision)` for a question settled at once.
    pub answer: Option<(String, String, Option<DelegatedDecision>)>,
    /// Send it to the user now, as its root's message.
    pub notify: bool,
    /// Use this id (an approval question's id is the `held_by` value).
    pub id: Option<String>,
}

impl NewQuestion {
    pub(super) fn open(item: &str, kind: QuestionKind, text: String, rule: &'static str) -> Self {
        NewQuestion {
            item: item.to_string(),
            kind,
            class: QuestionClass::BlockingNow,
            text,
            options: Vec::new(),
            default: None,
            asked_class: None,
            rule,
            asked_by: "controller".to_string(),
            status: QuestionStatus::Open,
            answer: None,
            notify: true,
            id: None,
        }
    }
}

/// A caller's mistake, for the gateway's 400.
pub(super) fn invalid(msg: impl Into<String>) -> Error {
    Error::ToolExecution(ToolError::invalid_input(msg))
}

/// The option an answer names by number (`2` for the second), else the
/// answer as given.
fn resolve_option(answer: &str, options: &[String]) -> String {
    let trimmed = answer.trim();
    trimmed
        .parse::<usize>()
        .ok()
        .and_then(|n| n.checked_sub(1))
        .and_then(|i| options.get(i).cloned())
        .unwrap_or_else(|| trimmed.to_string())
}

fn is_yes(answer: &str) -> bool {
    matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "yes" | "y" | "ok" | "okay" | "approve" | "approved" | "go" | "go ahead" | "allow" | "1"
    )
}

fn is_no(answer: &str) -> bool {
    let a = answer.trim().to_ascii_lowercase();
    [
        "no", "n", "reject", "rejected", "decline", "declined", "deny", "stop", "2",
    ]
    .iter()
    .any(|w| a == *w || a.starts_with(&format!("{w} ")))
}

/// Who answered, as the brief says it.
fn answered_by_words(by: Option<&str>) -> String {
    match by {
        Some("default") => "its recorded default".to_string(),
        Some(b) if b.starts_with("policy:") => {
            format!("standing judgment {}", b.trim_start_matches("policy:"))
        }
        Some(b) if b.starts_with("research:") => {
            format!("research {}", short(b.trim_start_matches("research:")))
        }
        _ => "the user".to_string(),
    }
}

impl Controller {
    // ── standing judgment ─────────────────────────────────────────────

    /// The standing judgment in force, loaded once and cached until a grant
    /// changes.
    pub(super) async fn judgment(&self) -> Result<Arc<Judgment>, Error> {
        if let Some(j) = self.state().judgment.clone() {
            return Ok(j);
        }
        let grants = self.store.judgment_list(false).await?;
        let j = Arc::new(Judgment::from_grants(self.config.approval.clone(), &grants));
        self.state().judgment = Some(j.clone());
        Ok(j)
    }

    /// Grant standing judgment in ordinary language: compiled, stored and
    /// in force from the next filing or question.
    pub(super) async fn grant_locked(
        &self,
        text: &str,
        scope: &str,
        actor: &str,
    ) -> Result<JudgmentRow, Error> {
        let text = text.trim();
        if text.is_empty() {
            return Err(invalid(
                "a grant needs words: what may be decided without asking",
            ));
        }
        let compiled = compile(text);
        let row = JudgmentRow {
            id: uuid::Uuid::new_v4().to_string(),
            scope: if scope.trim().is_empty() {
                "all".to_string()
            } else {
                scope.trim().to_string()
            },
            text: text.to_string(),
            checks: compiled.checks,
            policy: compiled.policy,
            unrecognised: compiled.unrecognised,
            granted_by: Some(actor.to_string()),
            granted_at: Utc::now(),
            revoked_at: None,
        };
        self.store.judgment_grant(row.clone()).await?;
        self.state().judgment = None;
        Ok(row)
    }

    pub(super) async fn revoke_locked(&self, id: &str) -> Result<bool, Error> {
        let revoked = self.store.judgment_revoke(id, Utc::now()).await?;
        self.state().judgment = None;
        Ok(revoked)
    }

    pub(super) async fn judgment_view(&self) -> Result<JudgmentView, Error> {
        let grants = self.store.judgment_list(false).await?;
        let rules = self.judgment().await?.describe();
        Ok(JudgmentView { grants, rules })
    }

    /// The delegated decision a plan's acceptance records when no approval
    /// trigger fired (6.1, scenario 29): the plan ran because the policy
    /// said it could, and the record says why and how to revisit it.
    pub(super) fn plan_decision(
        &self,
        b: &mut Batch,
        accepted: &crate::graph::Accepted,
        approval: &crate::graph::ApprovalPolicy,
    ) {
        let leaves: Vec<&WorkItem> = accepted
            .items
            .iter()
            .filter(|i| !b.snap.has_children(&i.id))
            .collect();
        let tokens: u64 = leaves.iter().map(|i| i.budget.tokens).sum();
        let writes: BTreeSet<&str> = leaves
            .iter()
            .flat_map(|i| i.writable_resources.iter().map(String::as_str))
            .collect();
        let mut limits = Vec::new();
        if let Some(n) = approval.max_items {
            limits.push(format!("up to {n} items"));
        }
        if let Some(n) = approval.max_total_tokens {
            limits.push(format!("up to {n} tokens"));
        }
        if !approval.consent_resources.is_empty() {
            let named: Vec<&str> = approval
                .consent_resources
                .iter()
                .map(String::as_str)
                .collect();
            limits.push(format!("nothing that writes {}", named.join(" or ")));
        }
        let decision = DelegatedDecision {
            chosen: "run the plan without asking".to_string(),
            alternatives: vec!["hold it for the user's approval".to_string()],
            rationale: format!(
                "{} new items, {tokens} tokens across its leaves, writing {}",
                accepted.items.len(),
                if writes.is_empty() {
                    "no resources".to_string()
                } else {
                    writes.into_iter().collect::<Vec<_>>().join(", ")
                }
            ),
            authority: format!(
                "approval policy {}: plans of {} run without asking",
                approval.id.as_deref().unwrap_or("default"),
                if limits.is_empty() {
                    "any size".to_string()
                } else {
                    limits.join(", ")
                }
            ),
            revisit: format!(
                "grant standing judgment that lowers the thresholds or names the side effect \
                 (`rustykrab judgment grant`), or cancel the plan with /cancel {}",
                accepted.root
            ),
        };
        b.note(
            &accepted.root,
            EventKind::Decision,
            "policy",
            serde_json::to_string(&decision).unwrap_or_default(),
        );
    }

    // ── recording ─────────────────────────────────────────────────────

    /// Record one question in the batch, with its event, and when it is
    /// open and `notify`, the root's message carrying it. Returns its id.
    pub(super) fn record_question(&self, b: &mut Batch, q: NewQuestion) -> String {
        let id = q.id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let root = b.root_of(&q.item);
        let (answer, answered_by, decision) = match q.answer {
            Some((a, by, d)) => (Some(a), Some(by), d),
            None => (None, None, None),
        };
        let row = QuestionRow {
            id: id.clone(),
            item: q.item.clone(),
            root,
            kind: q.kind,
            class: q.class,
            text: q.text.clone(),
            options: q.options.clone(),
            default_answer: q.default,
            asked_class: q.asked_class,
            rule: q.rule.to_string(),
            asked_by: q.asked_by,
            status: q.status,
            delivered_via: (q.status == QuestionStatus::Open && q.notify)
                .then(|| self.config.notice_channel.clone()),
            answered_at: answer.is_some().then_some(b.now),
            answer: answer.clone(),
            answered_by: answered_by.clone(),
            research_item: None,
            decision: decision.clone(),
            created_at: Utc::now(),
        };
        b.ops
            .push(WorkOp::Question(QuestionWrite::Insert(Box::new(row))));
        let mut line = format!(
            "question {} {} by rule {}: {}",
            short_question(&id),
            q.class.as_str(),
            q.rule,
            q.status.as_str()
        );
        if let Some(by) = &answered_by {
            line.push_str(&format!(" by {by}"));
        }
        b.note(&q.item, EventKind::Question, "controller", line);
        if let Some(d) = &decision {
            b.note(
                &q.item,
                EventKind::Decision,
                "policy",
                serde_json::to_string(d).unwrap_or_default(),
            );
        }
        if q.status == QuestionStatus::Open && q.notify {
            b.notify(
                &q.item,
                Cause::Question {
                    item: q.item.clone(),
                    id: id.clone(),
                    kind: q.kind,
                    text: q.text,
                    options: q.options,
                },
            );
        }
        id
    }

    /// The questions an item asked before, oldest first.
    async fn questions_of(&self, item: &str) -> Result<Vec<QuestionRow>, Error> {
        let mut rows = self
            .store
            .questions_list(&QuestionFilter {
                item: Some(item.to_string()),
                ..QuestionFilter::default()
            })
            .await?;
        rows.reverse();
        Ok(rows)
    }

    /// Answered questions of `item`, and blocking-later ones answered
    /// anywhere in its tree, as lines for the brief's decisions: the answer
    /// resumes the item, not the conversation.
    pub(super) async fn answers_for(
        &self,
        item: &WorkItem,
        root: &str,
    ) -> Result<Vec<String>, Error> {
        let mut rows = self.questions_of(&item.id).await?;
        let tree = self
            .store
            .questions_list(&QuestionFilter {
                root: Some(root.to_string()),
                ..QuestionFilter::default()
            })
            .await?;
        for q in tree.into_iter().rev() {
            if q.class == QuestionClass::BlockingLater && q.item != item.id {
                rows.push(q);
            }
        }
        let mut lines = Vec::new();
        for q in &rows {
            let (answer, by) = if q.status.has_answer() {
                (q.answer.clone().unwrap_or_default(), q.answered_by.clone())
            } else if let Some(r) = q
                .research_item
                .as_deref()
                .filter(|_| q.status == QuestionStatus::Researching)
            {
                // Research that landed answers its question before the
                // sweep records it: the brief must not wait a tick.
                match self.research_answer(r).await? {
                    Some(answer) => (answer, Some(format!("research:{r}"))),
                    None => continue,
                }
            } else {
                continue;
            };
            lines.push(format!(
                "Asked \"{}\": answered {} (by {})",
                q.text.trim(),
                answer.trim(),
                answered_by_words(by.as_deref())
            ));
        }
        Ok(lines)
    }

    /// The answer a research item gives: its result summary once it is
    /// done, `None` before.
    async fn research_answer(&self, research: &str) -> Result<Option<String>, Error> {
        let done = self
            .store
            .work_get(research)
            .await?
            .is_some_and(|i| i.status == Status::Done);
        if !done {
            return Ok(None);
        }
        Ok(Some(
            self.store
                .work_evidence_list(research)
                .await?
                .into_iter()
                .rev()
                .find(|e| e.kind == super::brief::SUMMARY)
                .map(|e| e.reference)
                .unwrap_or_else(|| format!("see research item {}", short(research))),
        ))
    }

    // ── routing a worker's questions ──────────────────────────────────

    /// The reconcile step's half of section 7 for a report that carries a
    /// block the user must meet, or questions. `None` when the report has
    /// neither (or its block is not the user's to meet), so the caller goes
    /// on as before.
    pub(super) async fn judge_questions(
        &self,
        b: &mut Batch,
        item: &WorkItem,
        worker: &str,
        result: &ResultReport,
    ) -> Result<Option<Vec<WorkItemId>>, Error> {
        let blocked: Option<&BlockedReport> = result.blocked.as_ref();
        let recorded: Vec<String> = blocked
            .map(|bl| {
                bl.needs
                    .iter()
                    .filter_map(|n| n.strip_prefix(QUESTION_NEED))
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        if !recorded.is_empty() {
            return self.park_on_recorded(b, item, &recorded).await.map(Some);
        }
        let user_block = blocked.is_some_and(|bl| bl.reason.needs_user());
        if result.questions.is_empty() && !user_block {
            return Ok(None);
        }
        let reason = blocked
            .map(|bl| bl.reason)
            .filter(|r| r.needs_user())
            .unwrap_or(BlockedReason::NeedsDecision);
        let kind = QuestionKind::for_blocked(reason);
        let mut asked: Vec<(Asked, Option<String>)> = result
            .questions
            .iter()
            .map(|q: &Question| {
                (
                    Asked {
                        text: q.text.trim().to_string(),
                        claimed: QuestionClass::parse(&q.class),
                        kind,
                        options: q.options.clone(),
                        default: None,
                    },
                    Some(q.class.clone()).filter(|c| !c.trim().is_empty()),
                )
            })
            .collect();
        if asked.is_empty() {
            let bl = blocked.expect("a user block when there are no questions");
            let mut text = [bl.detail.trim(), result.summary.trim()]
                .into_iter()
                .find(|text| !text.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| {
                    format!(
                        "the worker reported {} without saying what it needs",
                        Status::Blocked(reason)
                    )
                });
            if !bl.needs.is_empty() {
                text.push_str(&format!(" (needs {})", bl.needs.join(", ")));
            }
            if text.trim().is_empty() {
                text = format!("{} needs you ({})", label(item), reason.as_str());
            }
            asked.push((
                Asked {
                    text,
                    claimed: None,
                    kind,
                    options: Vec::new(),
                    default: None,
                },
                None,
            ));
        }
        let judgment = self.judgment().await?;
        let prior = self.questions_of(&item.id).await?;
        let success = blocked.is_none();

        let mut routed = Vec::new();
        for (a, _) in &asked {
            let before = prior
                .iter()
                .find(|p| p.status.has_answer() && same_question(&p.text, &a.text));
            let ctx = RouteContext {
                item_open: !item.status.is_closed(),
                answered_before: before.map(|p| (p.id.as_str(), p.answer.as_deref().unwrap_or(""))),
                judgment: &judgment,
            };
            routed.push(route(a, &ctx));
        }

        // A question already answered: the worker did not use its answer.
        if let Some(previous) = routed.iter().find_map(|r| match &r.route {
            Route::Repeat { previous } => Some(previous.clone()),
            _ => None,
        }) {
            let error = classify(
                &FailureInput::Provider {
                    problem: ProviderProblem::Loop,
                    detail: format!(
                        "asked again question {} after it was answered; the brief carried \
                         the answer",
                        short_question(&previous)
                    ),
                },
                &Context {
                    tool: None,
                    worker_kind: Some(self.worker_kind(worker)),
                },
            );
            return self
                .fail(b, item, worker, error, &result.artifacts)
                .await
                .map(Some);
        }

        let plan_b_first = !success && has_open_plan_b(&b.snap, &item.id);
        // (question id, kind, text, options) of each question for the user.
        let mut asks: Vec<(String, QuestionKind, String, Vec<String>)> = Vec::new();
        let mut research: Vec<(String, String)> = Vec::new();
        let mut answered = 0usize;
        for ((a, claimed), r) in asked.iter().zip(&routed) {
            let mut q = NewQuestion {
                item: item.id.clone(),
                kind: a.kind,
                class: r.class,
                text: a.text.clone(),
                options: a.options.clone(),
                default: a
                    .default
                    .clone()
                    .or_else(|| stated_default(&a.text, &a.options)),
                asked_class: claimed.clone(),
                rule: r.rule,
                asked_by: format!("worker:{worker}"),
                status: QuestionStatus::Open,
                answer: None,
                notify: false,
                id: None,
            };
            let mut for_user = false;
            match &r.route {
                // A block now cannot wait for a later message.
                Route::Ask | Route::Later if !success || r.route == Route::Ask => {
                    q.class = QuestionClass::BlockingNow;
                    q.status = if plan_b_first {
                        QuestionStatus::Recorded
                    } else {
                        QuestionStatus::Open
                    };
                    for_user = true;
                }
                Route::Later | Route::Ask => q.status = QuestionStatus::Open,
                Route::Research => q.status = QuestionStatus::Researching,
                Route::Default { answer } => {
                    q.status = QuestionStatus::Defaulted;
                    q.default = Some(answer.clone());
                    q.answer = Some((answer.clone(), "default".to_string(), None));
                    answered += 1;
                }
                Route::Delegated {
                    answer,
                    decision,
                    policy,
                } => {
                    q.status = QuestionStatus::Delegated;
                    q.answer = Some((
                        answer.clone(),
                        format!("policy:{policy}"),
                        Some(decision.clone()),
                    ));
                    answered += 1;
                }
                Route::Obsolete { .. } => q.status = QuestionStatus::Obsolete,
                Route::Repeat { .. } => unreachable!("handled above"),
            }
            let status = q.status;
            let id = self.record_question(b, q);
            if for_user {
                asks.push((id, a.kind, a.text.clone(), a.options.clone()));
            } else if status == QuestionStatus::Researching {
                research.push((id, a.text.clone()));
            }
        }

        if success && asks.is_empty() {
            // Nothing blocks the item: the success path goes on, with the
            // questions recorded (answered, or for a later message).
            self.file_research_for(b, item, &research);
            return Ok(None);
        }

        if let Some((_, first_kind, _, _)) = asks.first() {
            let reason = if blocked.is_some() {
                reason
            } else {
                first_kind.blocked_reason()
            };
            let texts: Vec<String> = asks.iter().map(|(_, _, t, _)| t.clone()).collect();
            if plan_b_first {
                // The plan B runs before the user is asked (6.4); the
                // question is recorded, not sent.
                return self.park(b, item, reason, &texts.join(" ")).await.map(Some);
            }
            let mut state = self.ladder_of(item).await?;
            b.rung(
                &item.id,
                &mut state,
                RungEvent {
                    rung: Rung::Surface,
                    at: b.now,
                    error: None,
                    outcome: format!("parked {}: asked the user", reason.as_str()),
                },
            );
            let changed = b.move_to(
                &item.id,
                Status::Blocked(reason),
                "controller",
                format!(
                    "asked the user {} question(s): {}",
                    asks.len(),
                    asks.iter()
                        .map(|(id, _, _, _)| short_question(id))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            );
            // One message for the subtree, every question in it.
            for (id, kind, text, options) in asks {
                b.notify(
                    &item.id,
                    Cause::Question {
                        item: item.id.clone(),
                        id,
                        kind,
                        text,
                        options,
                    },
                );
            }
            self.file_research_for(b, item, &research);
            return Ok(Some(changed));
        }

        // Blocked, and every question answered, researched or obsolete.
        let changed = b.move_to(
            &item.id,
            Status::Queued,
            "controller",
            if research.is_empty() {
                format!("{answered} question(s) answered without the user; resuming")
            } else {
                "waiting on research for its question".to_string()
            },
        );
        let mut changed = changed;
        changed.extend(self.file_research_for(b, item, &research));
        Ok(Some(changed))
    }

    /// File a `research` item for each `(question id, text)` and, when the
    /// item is waiting, make it wait on the research (section 7: a
    /// researchable question files a `research` item). Returns the ids to
    /// settle.
    fn file_research_for(
        &self,
        b: &mut Batch,
        item: &WorkItem,
        research: &[(String, String)],
    ) -> Vec<WorkItemId> {
        let mut changed = Vec::new();
        for (qid, text) in research {
            let tmp = "research".to_string();
            let draft = WorkItemDraft {
                tmp: Some(tmp.clone()),
                kind: Some(WorkKind::Research),
                title: format!("Research: {}", text.chars().take(100).collect::<String>()),
                objective: format!(
                    "Find the answer to a question {} asked: {text}",
                    short(&item.id)
                ),
                done_when: "The answer is attached as evidence, with its source.".to_string(),
                constraints: vec![format!(
                    "{RESEARCH_MARK} for question {}; the answer goes back to the item that \
                     asked.",
                    short_question(qid)
                )],
                artifact_refs: vec![ArtifactRef {
                    kind: "item".to_string(),
                    value: item.id.clone(),
                }],
                ..WorkItemDraft::default()
            };
            let root = match &item.parent {
                Some(p) => ItemRef::Id(p.clone()),
                None => ItemRef::Tmp { tmp: tmp.clone() },
            };
            let waits = b.status(&item.id).is_some_and(|s| s.is_waiting());
            let edges = if waits {
                vec![PlanEdge {
                    item: ItemRef::Id(item.id.clone()),
                    kind: EdgeKind::Blocks,
                    depends_on: ItemRef::Tmp { tmp: tmp.clone() },
                }]
            } else {
                Vec::new()
            };
            let plan = WorkPlan {
                root,
                items: vec![draft],
                edges,
                rationale: format!("research for question {}", short_question(qid)),
            };
            let provenance = Provenance {
                conversation_id: item.origin_conversation_id.clone(),
                filed_by_item: Some(item.id.clone()),
                actor: "controller".to_string(),
            };
            match self.file_into(b, &plan, &provenance, FilingSource::Ladder) {
                Ok(accepted) => {
                    let research_item = accepted.ids.get(&tmp).cloned().unwrap_or_default();
                    b.ops.push(WorkOp::Question(QuestionWrite::Research {
                        id: qid.clone(),
                        item: research_item,
                    }));
                    changed.extend(accepted.changed());
                }
                Err(rejection) => b.note(
                    &item.id,
                    EventKind::Rejection,
                    "controller",
                    format!(
                        "research item not filed: {}",
                        describe_rejection(&rejection)
                    ),
                ),
            }
        }
        changed
    }

    /// Park an item on questions its run already recorded (`ask_user`,
    /// `capability_request`): the typed state of the first open one, and
    /// the root's message carrying every open one.
    async fn park_on_recorded(
        &self,
        b: &mut Batch,
        item: &WorkItem,
        ids: &[String],
    ) -> Result<Vec<WorkItemId>, Error> {
        let mut rows = Vec::new();
        for id in ids {
            if let Some(q) = self.store.question_get(id).await? {
                rows.push(q);
            }
        }
        let open: Vec<&QuestionRow> = rows
            .iter()
            .filter(|q| q.status == QuestionStatus::Open)
            .collect();
        if let Some(first) = open.first() {
            let reason = first.kind.blocked_reason();
            if reason.needs_user() && has_open_plan_b(&b.snap, &item.id) {
                return self.park(b, item, reason, &first.text).await;
            }
            let mut state = self.ladder_of(item).await?;
            b.rung(
                &item.id,
                &mut state,
                RungEvent {
                    rung: Rung::Surface,
                    at: b.now,
                    error: None,
                    outcome: format!("parked {}: asked the user", reason.as_str()),
                },
            );
            let changed = b.move_to(
                &item.id,
                Status::Blocked(reason),
                "controller",
                format!("asked the user (question {})", short_question(&first.id)),
            );
            for q in &open {
                b.notify(
                    &item.id,
                    Cause::Question {
                        item: item.id.clone(),
                        id: q.id.clone(),
                        kind: q.kind,
                        text: q.text.clone(),
                        options: q.options.clone(),
                    },
                );
            }
            return Ok(changed);
        }
        let research: Vec<(String, String)> = rows
            .iter()
            .filter(|q| q.status == QuestionStatus::Researching)
            .map(|q| (q.id.clone(), q.text.clone()))
            .collect();
        let mut changed = b.move_to(
            &item.id,
            Status::Queued,
            "controller",
            if research.is_empty() {
                "its question was settled; resuming".to_string()
            } else {
                "waiting on research for its question".to_string()
            },
        );
        changed.extend(self.file_research_for(b, item, &research));
        Ok(changed)
    }

    // ── a question mid-run (`ask_user`, `capability_request`) ─────────

    /// Check `provenance` holds `item`'s lease, and return its worker.
    async fn lease_holder(&self, item: &str, provenance: &Provenance) -> Result<String, Error> {
        if provenance.filed_by_item.as_deref() != Some(item) {
            return Err(Error::Auth(format!(
                "a question for {item} is accepted only from the run holding its lease"
            )));
        }
        let lease = self
            .store
            .work_lease_get(item)
            .await?
            .ok_or_else(|| Error::Auth(format!("{item} has no live lease")))?;
        if let Some(name) = provenance.actor.strip_prefix("worker:") {
            if name != lease.worker {
                return Err(Error::Auth(format!(
                    "{item} is leased to {}, not {name}",
                    lease.worker
                )));
            }
        }
        Ok(lease.worker)
    }

    /// End the run holding `item` with `report`, which the next reconcile
    /// applies: the run is marked reported, so what its task returns later
    /// is ignored.
    pub(super) fn end_run_with(&self, item: &str, worker: &str, report: ResultReport) {
        let mut state = self.state();
        if let Some(run) = state.runs.get_mut(item) {
            run.reported = true;
        }
        state.finished.insert(
            item.to_string(),
            Finished {
                worker: worker.to_string(),
                outcome: Ok(report),
            },
        );
    }

    /// `ask_user` (section 7): route now; answer at once when the router
    /// can, else record the question and end the run parked on it.
    pub(super) async fn ask_locked(
        &self,
        item_id: &str,
        request: AskRequest,
        provenance: Provenance,
    ) -> Result<AskOutcome, Error> {
        let worker = self.lease_holder(item_id, &provenance).await?;
        let judgment = self.judgment().await?;
        let b = Batch::new(self.load().await?, self.clock.now());
        let item = b
            .snap
            .item(item_id)
            .cloned()
            .ok_or_else(|| Error::NotFound(format!("work item {item_id}")))?;
        let asked = Asked {
            text: request.text.trim().to_string(),
            claimed: request.class.as_deref().and_then(QuestionClass::parse),
            kind: request.kind,
            options: request.options.clone(),
            default: request.default.clone(),
        };
        self.ask_now(b, &item, &worker, asked, request.class, &judgment)
            .await
    }

    async fn ask_now(
        &self,
        mut b: Batch,
        item: &WorkItem,
        worker: &str,
        asked: Asked,
        claimed: Option<String>,
        judgment: &Judgment,
    ) -> Result<AskOutcome, Error> {
        let prior = self.questions_of(&item.id).await?;
        let before = prior
            .iter()
            .find(|p| p.status.has_answer() && same_question(&p.text, &asked.text));
        let routed = route(
            &asked,
            &RouteContext {
                item_open: !item.status.is_closed(),
                answered_before: before.map(|p| (p.id.as_str(), p.answer.as_deref().unwrap_or(""))),
                judgment,
            },
        );
        if let (Route::Repeat { previous }, Some(p)) = (&routed.route, before) {
            return Ok(AskOutcome {
                question: previous.clone(),
                class: p.class,
                answer: p.answer.clone(),
                answered_by: p.answered_by.clone(),
                parked: false,
                note: "you asked this before; this is the answer you were given".to_string(),
            });
        }
        let mut q = NewQuestion {
            item: item.id.clone(),
            kind: asked.kind,
            class: routed.class,
            text: asked.text.clone(),
            options: asked.options.clone(),
            default: asked
                .default
                .clone()
                .or_else(|| stated_default(&asked.text, &asked.options)),
            asked_class: claimed,
            rule: routed.rule,
            asked_by: format!("worker:{worker}"),
            status: QuestionStatus::Open,
            answer: None,
            // The message goes when the reconcile parks the item.
            notify: false,
            id: None,
        };
        let mut outcome = AskOutcome {
            question: String::new(),
            class: routed.class,
            answer: None,
            answered_by: None,
            parked: false,
            note: String::new(),
        };
        let mut park = false;
        match &routed.route {
            Route::Default { answer } => {
                q.status = QuestionStatus::Defaulted;
                q.default = Some(answer.clone());
                q.answer = Some((answer.clone(), "default".to_string(), None));
                outcome.answer = Some(answer.clone());
                outcome.answered_by = Some("default".to_string());
                outcome.note = "answered with its recorded default".to_string();
            }
            Route::Delegated {
                answer,
                decision,
                policy,
            } => {
                q.status = QuestionStatus::Delegated;
                let by = format!("policy:{policy}");
                q.answer = Some((answer.clone(), by.clone(), Some(decision.clone())));
                outcome.answer = Some(answer.clone());
                outcome.answered_by = Some(by);
                outcome.note = "decided by standing judgment; the decision is recorded".to_string();
            }
            Route::Obsolete { why } => {
                q.status = QuestionStatus::Obsolete;
                outcome.note = format!("no answer is needed: {why}");
            }
            Route::Later => {
                outcome.note = "recorded; it goes to the user with the next message about this \
                                work. Carry on with what you can do now."
                    .to_string();
            }
            Route::Research => {
                q.status = QuestionStatus::Researching;
                park = true;
                outcome.note = "a research item will find the answer; this run ends and the \
                                item resumes with it"
                    .to_string();
            }
            Route::Ask => {
                park = true;
                outcome.note = "asked the user; this run ends and the item resumes with the \
                                answer"
                    .to_string();
            }
            Route::Repeat { .. } => {}
        }
        let kind = q.kind;
        let text = q.text.clone();
        let id = self.record_question(&mut b, q);
        outcome.question = id.clone();
        outcome.parked = park;
        self.commit(b, &mut HashSet::new()).await?;
        if park {
            self.end_run_with(
                &item.id,
                worker,
                ResultReport {
                    summary: format!("Asked: {}", text.chars().take(200).collect::<String>()),
                    blocked: Some(BlockedReport {
                        reason: kind.blocked_reason(),
                        detail: text,
                        needs: vec![format!("{QUESTION_NEED}{id}")],
                    }),
                    ..ResultReport::default()
                },
            );
        }
        Ok(outcome)
    }

    /// `capability_request` (sections 7 and 8): a credential or consent is
    /// a question for the user; a tool, install, compute or knowledge gap
    /// is a failure for the ladder's order 2. Either way the item parks.
    pub(super) async fn capability_locked(
        &self,
        item_id: &str,
        request: CapabilityAsk,
        provenance: Provenance,
    ) -> Result<AskOutcome, Error> {
        let worker = self.lease_holder(item_id, &provenance).await?;
        let judgment = self.judgment().await?;
        let b = Batch::new(self.load().await?, self.clock.now());
        let item = b
            .snap
            .item(item_id)
            .cloned()
            .ok_or_else(|| Error::NotFound(format!("work item {item_id}")))?;
        let reason = request.reason.trim();
        match request.kind.as_str() {
            "credential" | "consent" => {
                let (kind, text) = if request.kind == "credential" {
                    (
                        QuestionKind::Credential,
                        format!(
                            "{} needs the credential `{}`{}",
                            label(&item),
                            request.name,
                            if reason.is_empty() {
                                String::new()
                            } else {
                                format!(": {reason}")
                            }
                        ),
                    )
                } else {
                    (
                        QuestionKind::Consent,
                        format!(
                            "May {} go ahead with {}{}?",
                            label(&item),
                            request.name,
                            if reason.is_empty() {
                                String::new()
                            } else {
                                format!(" ({reason})")
                            }
                        ),
                    )
                };
                let asked = Asked {
                    text,
                    claimed: Some(QuestionClass::BlockingNow),
                    kind,
                    options: if kind == QuestionKind::Consent {
                        vec!["yes".to_string(), "no".to_string()]
                    } else {
                        Vec::new()
                    },
                    default: None,
                };
                self.ask_now(b, &item, &worker, asked, None, &judgment)
                    .await
            }
            other => {
                let gap = match other {
                    "tool" => GapKind::Tool,
                    "install" => GapKind::Install,
                    "compute" => GapKind::Compute,
                    _ => GapKind::Knowledge,
                };
                let error = classify(
                    &FailureInput::CapabilityGap {
                        gap,
                        name: request.name.clone(),
                    },
                    &Context {
                        tool: None,
                        worker_kind: Some(self.worker_kind(&worker)),
                    },
                );
                self.end_run_with(
                    &item.id,
                    &worker,
                    ResultReport {
                        summary: format!("Needs the {} `{}`", gap.as_str(), request.name),
                        error: Some(error),
                        ..ResultReport::default()
                    },
                );
                Ok(AskOutcome {
                    question: String::new(),
                    class: QuestionClass::BlockingNow,
                    answer: None,
                    answered_by: None,
                    parked: true,
                    note: format!(
                        "filed for the ladder's order 2: the item parks until the {} `{}` \
                         is available",
                        gap.as_str(),
                        request.name
                    ),
                })
            }
        }
    }

    // ── answers ───────────────────────────────────────────────────────

    /// A question by its id or the first eight characters of it, among the
    /// questions still waiting.
    async fn resolve_question(&self, id: &str) -> Result<QuestionRow, Error> {
        let id = id.trim().trim_start_matches('#');
        if let Some(q) = self.store.question_get(id).await? {
            return Ok(q);
        }
        let waiting = self
            .store
            .questions_list(&QuestionFilter {
                waiting: true,
                ..QuestionFilter::default()
            })
            .await?;
        let matches: Vec<QuestionRow> = waiting
            .into_iter()
            .filter(|q| !id.is_empty() && q.id.starts_with(id))
            .collect();
        match matches.len() {
            1 => Ok(matches.into_iter().next().expect("one match")),
            0 => Err(Error::NotFound(format!("question {id}"))),
            _ => Err(invalid(format!(
                "question {id} is ambiguous: give more of its id"
            ))),
        }
    }

    /// Answer a question (section 7): settle it and resume what waits on
    /// it. An approval approves or rejects its plan; a consent's `no`
    /// cancels its item.
    pub(super) async fn answer_locked(
        &self,
        question: &str,
        answer: &str,
        actor: &str,
    ) -> Result<AnswerReply, Error> {
        let q = self.resolve_question(question).await?;
        if !q.status.is_waiting() {
            return Err(Error::AlreadyExists(format!(
                "question {} is already {}",
                q.id, q.status
            )));
        }
        let answer = resolve_option(answer, &q.options);
        if answer.is_empty() {
            return Err(invalid("an answer needs words"));
        }
        let mut reply = AnswerReply {
            question: q.clone(),
            resumed: Vec::new(),
            released: Vec::new(),
            cancelled: Vec::new(),
        };
        if q.kind == QuestionKind::Approval {
            if is_yes(&answer) {
                reply.released = self.approve_locked(&q.item, actor).await?;
            } else if is_no(&answer) {
                let why = answer
                    .split_once(' ')
                    .map(|(_, r)| r.trim().to_string())
                    .filter(|r| !r.is_empty());
                reply.cancelled = self.reject_locked(&q.item, why, actor).await?;
            } else {
                return Err(invalid(
                    "a plan's approval is answered approve or reject [reason]",
                ));
            }
            reply.question = self
                .store
                .question_get(&q.id)
                .await?
                .unwrap_or(reply.question);
            return Ok(reply);
        }
        if q.kind == QuestionKind::Consent && !is_yes(&answer) && !is_no(&answer) {
            return Err(invalid("a consent is answered yes or no"));
        }
        let now = self.clock.now();
        let mut b = Batch::new(self.load().await?, now);
        b.ops.push(WorkOp::Question(QuestionWrite::Settle {
            id: q.id.clone(),
            status: QuestionStatus::Answered,
            answer: Some(answer.clone()),
            by: Some(actor.to_string()),
            decision: None,
            at: now,
        }));
        b.note(
            &q.item,
            EventKind::Question,
            actor,
            format!("question {} answered", short_question(&q.id)),
        );
        // `on_answer` triggers naming the question fire with this batch.
        for i in b.snap.items().to_vec() {
            if i.trigger == rustykrab_core::work::Trigger::OnAnswer(q.id.clone()) {
                b.snap.fire(&i.id);
            }
        }
        let mut changed: Vec<WorkItemId> = b
            .snap
            .items()
            .iter()
            .filter(|i| i.trigger == rustykrab_core::work::Trigger::OnAnswer(q.id.clone()))
            .map(|i| i.id.clone())
            .collect();
        let declined = q.kind == QuestionKind::Consent && is_no(&answer);
        let still_open = self
            .questions_of(&q.item)
            .await?
            .into_iter()
            .any(|o| o.id != q.id && o.status == QuestionStatus::Open);
        let parked = matches!(
            b.status(&q.item),
            Some(Status::Blocked(
                BlockedReason::NeedsDecision
                    | BlockedReason::NeedsConsent
                    | BlockedReason::NeedsCredential
                    | BlockedReason::NeedsTool
                    | BlockedReason::WorkerUnavailable
            ))
        );
        if declined {
            let (touched, cancelled) =
                b.cancel_tree(&q.item, actor, Some(format!("declined: {answer}")));
            changed.extend(touched);
            reply.cancelled = cancelled;
        } else if parked && !still_open {
            changed.extend(b.move_to(
                &q.item,
                Status::Queued,
                actor,
                format!("answered question {}", short_question(&q.id)),
            ));
            reply.resumed.push(q.item.clone());
        }
        b.settle(changed);
        self.commit(b, &mut HashSet::new()).await?;
        reply.question = self
            .store
            .question_get(&q.id)
            .await?
            .unwrap_or(reply.question);
        Ok(reply)
    }

    /// Settle the approval question of every item `ids` released or
    /// cancelled, in `b`.
    pub(super) async fn settle_approval(
        &self,
        b: &mut Batch,
        questions: &BTreeSet<String>,
        answer: &str,
        actor: &str,
    ) -> Result<(), Error> {
        for id in questions {
            if self
                .store
                .question_get(id)
                .await?
                .is_some_and(|q| q.status.is_waiting())
            {
                b.ops.push(WorkOp::Question(QuestionWrite::Settle {
                    id: id.clone(),
                    status: QuestionStatus::Answered,
                    answer: Some(answer.to_string()),
                    by: Some(actor.to_string()),
                    decision: None,
                    at: b.now,
                }));
            }
        }
        Ok(())
    }

    // ── the sweep's half ──────────────────────────────────────────────

    /// Waiting questions the store holds, and which items they belong to.
    pub(super) async fn waiting_questions(&self) -> Result<Vec<QuestionRow>, Error> {
        Ok(self
            .store
            .questions_list(&QuestionFilter {
                waiting: true,
                ..QuestionFilter::default()
            })
            .await?)
    }

    /// Questions whose need went away or was met, settled in the sweep: a
    /// research item that landed answers its question; a question held
    /// back while a plan B ran closes once the plan B is done (6.4); an
    /// open question whose item or tree closed is obsolete.
    pub(super) async fn sweep_questions(
        &self,
        b: &mut Batch,
        waiting: &[QuestionRow],
    ) -> Result<(), Error> {
        for q in waiting {
            let settle = |status: QuestionStatus, answer: Option<String>, by: Option<String>| {
                WorkOp::Question(QuestionWrite::Settle {
                    id: q.id.clone(),
                    status,
                    answer,
                    by,
                    decision: None,
                    at: b.now,
                })
            };
            match q.status {
                QuestionStatus::Researching => {
                    let Some(r) = q.research_item.as_deref().filter(|r| !r.is_empty()) else {
                        continue;
                    };
                    if b.status(r) == Some(Status::Done) {
                        let summary = self.research_answer(r).await?.unwrap_or_default();
                        b.ops.push(settle(
                            QuestionStatus::Answered,
                            Some(summary),
                            Some(format!("research:{r}")),
                        ));
                    }
                }
                QuestionStatus::Recorded => {
                    let covered = b
                        .snap
                        .edges_naming(&q.item)
                        .filter(|e| e.kind == EdgeKind::ConditionalOnFailure)
                        .any(|e| b.status(&e.item) == Some(Status::Done));
                    if covered {
                        b.ops.push(settle(QuestionStatus::Obsolete, None, None));
                    }
                }
                QuestionStatus::Open => {
                    // A question for a later step outlives the item that
                    // asked it, until its tree closes; any other closes
                    // with its item (a plan's approval with its root).
                    let tree_closed = b.status(&q.root).is_none_or(|s| s.is_closed());
                    let item_closed = b.status(&q.item).is_none_or(|s| s.is_closed());
                    let later = q.class == QuestionClass::BlockingLater;
                    if (item_closed && !later) || (tree_closed && later) {
                        b.ops.push(settle(QuestionStatus::Obsolete, None, None));
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// Items parked on a credential question whose credential the host now
    /// holds: the question is answered by the credential and the item
    /// resumes (the credential page's park-and-wake, for work items).
    pub(super) fn wake_on_credentials(
        &self,
        b: &mut Batch,
        waiting: &[QuestionRow],
    ) -> Vec<WorkItemId> {
        let mut changed = Vec::new();
        for q in waiting {
            if q.kind != QuestionKind::Credential || q.status != QuestionStatus::Open {
                continue;
            }
            let Some(name) = q.text.split('`').nth(1) else {
                continue;
            };
            if !self.catalog.credential_available(name) {
                continue;
            }
            if b.status(&q.item) != Some(Status::Blocked(BlockedReason::NeedsCredential)) {
                continue;
            }
            b.ops.push(WorkOp::Question(QuestionWrite::Settle {
                id: q.id.clone(),
                status: QuestionStatus::Answered,
                answer: Some(format!("the credential {name} is stored")),
                by: Some("controller".to_string()),
                decision: None,
                at: b.now,
            }));
            changed.extend(b.move_to(
                &q.item,
                Status::Queued,
                "controller",
                format!("credential {name} available"),
            ));
        }
        changed
    }

    /// Open roots whose chain has been open past the digest window with no
    /// message about it inside the window get a digest (6.6).
    pub(super) async fn digests(&self, b: &mut Batch, now: DateTime<Utc>) -> Result<(), Error> {
        let window = self.config.digest_window;
        let roots: Vec<WorkItem> = b
            .snap
            .items()
            .iter()
            .filter(|i| i.parent.is_none() && !i.status.is_closed() && b.snap.has_children(&i.id))
            .filter(|i| i.created_at + window <= now)
            .cloned()
            .collect();
        for root in roots {
            let latest = self.store.work_outbox_latest(&root.id).await?;
            if latest.is_none_or(|t| t + window <= now) {
                b.notify(&root.id, Cause::Digest);
            }
        }
        Ok(())
    }
}
