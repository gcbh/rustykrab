//! Human-owned work preferences. Reuses judgment storage and the controller's
//! existing review/approval paths; workers cannot grant authority.
use super::{batch::Batch, questions::invalid, Controller};
use chrono::TimeDelta;
use rustykrab_core::{
    proposal::ReviewDecision,
    questions::JudgmentCheck,
    work::{protected_subject, BlockedReason, EventKind, Status, WorkItem, WorkKind},
    work_preference::{WorkStartMode, WorkStartReply, WorkTemplate},
    Error,
};
use rustykrab_store::{JudgmentRow, WorkFilter, WorkOp};
use std::collections::HashSet;

impl Controller {
    async fn template_for(&self, item: &WorkItem) -> Result<WorkTemplate, Error> {
        let mut template = WorkTemplate::capture(item, self.store.work_facets_get(&item.id).await?);
        template.project_id = self.project_id(&self.load().await?, item).await?;
        Ok(template)
    }
    async fn automatic_eligible(&self, item: &WorkItem) -> Result<bool, Error> {
        let snap = self.load().await?;
        // A single example never authorizes a whole plan, inherited scope,
        // protected policy changes, capability acquisition or routing changes.
        Ok(
            (item.held_by.is_none() || self.held_under(&snap, &item.id).await?.len() == 1)
                && item.parent.is_none()
                && !snap.has_children(&item.id)
                && !crate::graph::is_planning(item)
                && matches!(
                    item.kind,
                    WorkKind::Personal | WorkKind::Code | WorkKind::Research | WorkKind::Proposal
                )
                && !item.writable_resources.iter().any(|r| r.contains('*'))
                && !item
                    .required_tools
                    .iter()
                    .chain(&item.required_mcp_servers)
                    .any(|r| r.contains('*'))
                && !item
                    .artifact_refs
                    .iter()
                    .any(|r| r.kind == "routing_default")
                && self
                    .store
                    .work_facets_get(&item.id)
                    .await?
                    .as_ref()
                    .and_then(|f| f.subject.as_deref())
                    .and_then(protected_subject)
                    .is_none(),
        )
    }
    pub(super) async fn start_work_locked(
        &self,
        id: &str,
        mode: WorkStartMode,
        daily_limit: u32,
        note: &str,
        actor: &str,
    ) -> Result<WorkStartReply, Error> {
        if !actor.starts_with("user:") {
            return Err(Error::Auth(
                "only an authenticated human can record a work preference".into(),
            ));
        }
        if note.len() > 4096 || !(1..=50).contains(&daily_limit) {
            return Err(invalid(
                "note must fit 4096 bytes and daily limit must be between 1 and 50",
            ));
        }
        let item = self
            .store
            .work_get(id)
            .await?
            .ok_or_else(|| Error::NotFound(format!("work item {id}")))?;
        if item.status.is_closed() {
            return Err(invalid("closed work cannot be started"));
        }
        if mode == WorkStartMode::Automatic && !self.automatic_eligible(&item).await? {
            return Err(invalid("automatic permission needs a single bounded task with explicit scope; plans and protected changes require individual review"));
        }
        let template = self.template_for(&item).await?;
        if mode == WorkStartMode::Automatic
            && serde_json::to_vec(&template)
                .map_err(|e| Error::Internal(e.to_string()))?
                .len()
                > 32768
        {
            return Err(invalid(
                "work template exceeds 32 KiB; narrow the task first",
            ));
        }
        let check = JudgmentCheck::WorkTemplate {
            mode,
            daily_limit,
            template: Box::new(template),
        };
        let text = note.trim().to_string();
        let row = JudgmentRow {
            id: uuid::Uuid::new_v4().to_string(),
            scope: "work_template".into(),
            text: text.clone(),
            checks: vec![check.clone()],
            policy: rustykrab_projects::JudgmentPolicy {
                statement: text,
                delegated_scopes: if mode == WorkStartMode::Automatic {
                    vec![check.describe()]
                } else {
                    vec![]
                },
                ..Default::default()
            },
            unrecognised: vec![],
            granted_by: Some(actor.into()),
            granted_at: self.clock.now(),
            revoked_at: None,
        };
        let pref = row.id.clone();
        let mut reply = WorkStartReply {
            item: id.into(),
            mode,
            preference_id: pref,
            released: vec![],
            implementation: None,
        };
        if item.kind == WorkKind::Proposal {
            reply.implementation = self
                .review_with_preference_locked(id, ReviewDecision::Accept, actor, Some(row), None)
                .await?
                .code_item;
        } else if item.held_by.is_some() {
            reply.released = self
                .approve_with_preference_locked(id, actor, Some(row), None, None)
                .await?;
        } else if matches!(
            item.status,
            Status::Queued | Status::Ready | Status::Leased | Status::Running | Status::Verifying
        ) {
            let mut b = Batch::new(self.load().await?, self.clock.now());
            if !row.text.is_empty() {
                b.note(
                    id,
                    EventKind::Comment,
                    actor,
                    format!("Direction recorded with work permission: {}", row.text),
                );
            }
            b.ops.push(WorkOp::Judgment(Box::new(row)));
            b.note(
                id,
                EventKind::Review,
                actor,
                "started: recorded work preference; existing execution continues".into(),
            );
            self.commit(b, &mut HashSet::new()).await?;
        } else {
            return Err(invalid(
                "answer this task's unresolved question before starting it",
            ));
        }
        self.state().judgment = None;
        Ok(reply)
    }
    pub(super) async fn start_matching_templates_locked(&self) -> Result<(), Error> {
        let grants: Vec<_> = self
            .store
            .judgment_list(false)
            .await?
            .into_iter()
            .filter(|r| {
                r.scope == "work_template"
                    && r.checks.iter().any(|c| {
                        matches!(
                            c,
                            JudgmentCheck::WorkTemplate {
                                mode: WorkStartMode::Automatic,
                                ..
                            }
                        )
                    })
            })
            .collect();
        if grants.is_empty() {
            return Ok(());
        }
        let since = self.clock.now() - TimeDelta::hours(24);
        let events = self.store.work_events_since(since).await?;
        let items = self
            .store
            .work_list(&WorkFilter {
                include_closed: false,
                ..Default::default()
            })
            .await?;
        let mut started = 0;
        for item in items {
            if started >= 5 {
                break;
            }
            if item.status != Status::Blocked(BlockedReason::NeedsConsent)
                || !self.automatic_eligible(&item).await?
            {
                continue;
            }
            // New technical/credential questions remain decisions, even for a template.
            if item.kind != WorkKind::Proposal && item.held_by.is_none() {
                continue;
            }
            if self.questions_of(&item.id).await?.iter().any(|q| {
                q.status.is_waiting()
                    && !(q.kind == rustykrab_core::questions::QuestionKind::Approval
                        && item.held_by.as_deref() == Some(&q.id))
            }) {
                continue;
            }
            let candidate = self.template_for(&item).await?;
            for row in grants.iter().rev().filter(|r| {
                r.scope == "work_template"
                    && r.granted_by
                        .as_ref()
                        .is_some_and(|a| a.starts_with("user:"))
            }) {
                let actor = format!("policy:{}", row.id);
                let Some(limit) = row.checks.iter().find_map(|c| match c {
                    JudgmentCheck::WorkTemplate {
                        mode: WorkStartMode::Automatic,
                        daily_limit,
                        template,
                    } if template.matches(&candidate) => Some(*daily_limit),
                    _ => None,
                }) else {
                    continue;
                };
                let used = events
                    .iter()
                    .filter(|e| {
                        e.actor == actor
                            && e.kind == EventKind::Review
                            && e.reason.as_deref().is_some_and(|r| {
                                r.starts_with("accepted:") || r.starts_with("started:")
                            })
                    })
                    .count();
                if used >= limit as usize {
                    continue;
                }
                // Recheck the count after each committed start, including same-tick starts.
                let fresh = self.store.work_events_since(since).await?;
                if fresh
                    .iter()
                    .filter(|e| {
                        e.actor == actor
                            && e.kind == EventKind::Review
                            && e.reason.as_deref().is_some_and(|r| {
                                r.starts_with("accepted:") || r.starts_with("started:")
                            })
                    })
                    .count()
                    >= limit as usize
                {
                    continue;
                }
                let prior = self.store.work_events(&item.id).await?;
                if prior.iter().any(|e| {
                    e.actor == actor
                        && e.kind == EventKind::Warning
                        && e.reason
                            .as_deref()
                            .is_some_and(|r| r.starts_with("automatic start refused:"))
                }) {
                    continue;
                }
                let result = if item.kind == WorkKind::Proposal {
                    self.review_with_preference_locked(
                        &item.id,
                        ReviewDecision::Accept,
                        &actor,
                        None,
                        Some(row.text.clone()),
                    )
                    .await
                    .map(|_| ())
                } else {
                    self.approve_with_preference_locked(
                        &item.id,
                        &actor,
                        None,
                        Some(row.text.clone()),
                        None,
                    )
                    .await
                    .map(|_| ())
                };
                if let Err(error) = result {
                    let mut b = Batch::new(self.load().await?, self.clock.now());
                    b.note(&item.id,EventKind::Warning,&actor,format!("automatic start refused: {error}; inspect this work before granting a replacement preference"));
                    self.commit(b, &mut HashSet::new()).await?;
                    break;
                }
                started += 1;
                break;
            }
        }
        Ok(())
    }
}
