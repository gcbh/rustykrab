//! Human-owned work preferences. Reuses judgment storage and the controller's
//! existing review/approval paths; workers cannot grant authority.
use super::project_context::ProjectList;
use super::{batch::Batch, questions::invalid, Controller};
use crate::graph::Snapshot;
use chrono::TimeDelta;
use rustykrab_core::{
    proposal::ReviewDecision,
    questions::JudgmentCheck,
    work::{protected_subject, BlockedReason, EventKind, Status, WorkEvent, WorkItem, WorkKind},
    work_preference::{WorkStartMode, WorkStartReply, WorkTemplate},
    Error,
};
use rustykrab_store::{JudgmentRow, WorkOp};
use std::collections::HashSet;

impl Controller {
    async fn template_for(&self, item: &WorkItem) -> Result<WorkTemplate, Error> {
        let snap = self.load().await?;
        self.template_in(&snap, item, &mut ProjectList::default())
            .await
    }
    /// `item`'s template against an already loaded snapshot, listing
    /// projects at most once per `projects`.
    async fn template_in(
        &self,
        snap: &Snapshot,
        item: &WorkItem,
        projects: &mut ProjectList,
    ) -> Result<WorkTemplate, Error> {
        let mut template = WorkTemplate::capture(item, self.store.work_facets_get(&item.id).await?);
        template.project_id = self.project_id_in(snap, item, projects).await?;
        Ok(template)
    }
    async fn automatic_eligible(&self, item: &WorkItem) -> Result<bool, Error> {
        let snap = self.load().await?;
        self.automatic_eligible_in(&snap, item).await
    }
    /// The checks of [`Self::automatic_eligible`] that read only the item and
    /// the snapshot, so a caller can skip an item before any store read.
    fn automatic_shape(snap: &Snapshot, item: &WorkItem) -> bool {
        // A single example never authorizes a whole plan, inherited scope,
        // protected policy changes, capability acquisition or routing changes.
        item.parent.is_none()
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
    }
    async fn automatic_eligible_in(&self, snap: &Snapshot, item: &WorkItem) -> Result<bool, Error> {
        Ok(Self::automatic_shape(snap, item)
            && (item.held_by.is_none() || self.held_under(snap, &item.id).await?.len() == 1)
            && self
                .store
                .work_facets_get(&item.id)
                .await?
                .as_ref()
                .and_then(|f| f.subject.as_deref())
                .and_then(protected_subject)
                .is_none())
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
    /// Start the waiting items an automatic work preference covers, at most
    /// five a pass. Runs every tick, so it does the cheap work first: no
    /// grant with allowance left today ends it before any work is read; then
    /// one snapshot and one project list serve every candidate, and an item
    /// that fails a check needing no store read (status, kind, parent,
    /// children, hold, scope) is skipped before its facets, plan or
    /// questions are read. The snapshot is reloaded only after a start
    /// changes the store.
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
        let starts_by = |events: &[WorkEvent], actor: &str| {
            events
                .iter()
                .filter(|e| {
                    e.actor == actor
                        && e.kind == EventKind::Review
                        && e.reason.as_deref().is_some_and(|r| {
                            r.starts_with("accepted:") || r.starts_with("started:")
                        })
                })
                .count()
        };
        // Only grants a human gave, with allowance left today, can start
        // anything this pass.
        let open: Vec<_> = grants
            .iter()
            .rev()
            .filter(|r| {
                r.scope == "work_template"
                    && r.granted_by
                        .as_ref()
                        .is_some_and(|a| a.starts_with("user:"))
            })
            .filter(|r| {
                let used = starts_by(&events, &format!("policy:{}", r.id));
                r.checks.iter().any(|c| {
                    matches!(c, JudgmentCheck::WorkTemplate {
                        mode: WorkStartMode::Automatic,
                        daily_limit,
                        ..
                    } if used < *daily_limit as usize)
                })
            })
            .collect();
        if open.is_empty() {
            return Ok(());
        }
        let mut snap = self.load().await?;
        let mut projects = ProjectList::default();
        let candidates: Vec<WorkItem> = snap
            .items()
            .iter()
            .filter(|item| {
                item.status == Status::Blocked(BlockedReason::NeedsConsent)
                    // New technical/credential questions remain decisions,
                    // even for a template.
                    && (item.kind == WorkKind::Proposal || item.held_by.is_some())
                    && Self::automatic_shape(&snap, item)
            })
            .cloned()
            .collect();
        let mut started = 0;
        for item in candidates {
            if started >= 5 {
                break;
            }
            if !self.automatic_eligible_in(&snap, &item).await? {
                continue;
            }
            if self.questions_of(&item.id).await?.iter().any(|q| {
                q.status.is_waiting()
                    && !(q.kind == rustykrab_core::questions::QuestionKind::Approval
                        && item.held_by.as_deref() == Some(&q.id))
            }) {
                continue;
            }
            let candidate = self.template_in(&snap, &item, &mut projects).await?;
            for row in &open {
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
                if starts_by(&events, &actor) >= limit as usize {
                    continue;
                }
                // Recheck the count after each committed start, including same-tick starts.
                let fresh = self.store.work_events_since(since).await?;
                if starts_by(&fresh, &actor) >= limit as usize {
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
                // The start wrote holds, edges and statuses the next
                // candidate's checks read.
                snap = self.load().await?;
                break;
            }
        }
        Ok(())
    }
}
