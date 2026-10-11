//! Reconstruct context independently of a Claude/Codex session or its transcript.
use std::collections::BTreeMap;

use chrono::Utc;
use rustykrab_core::questions::QuestionKind;
use rustykrab_core::work::{BlockedReason, Evidence, Status, WorkItem};
use rustykrab_core::Error;
use rustykrab_projects::{ProjectId, ProjectStatus};

use super::batch::Batch;
use super::questions::NewQuestion;
use super::Controller;
use crate::graph::Snapshot;
use crate::handoff::{
    binding, ProjectAttempt, ProjectContext, ProjectWork, CONTROLLER, PROJECT_CONTEXT,
};
use crate::workspace::{Workspace, WORKSPACE_EVIDENCE};

const CONTEXT_BYTES: usize = 128 * 1024;

impl Controller {
    async fn project_id(&self, snap: &Snapshot, item: &WorkItem) -> Result<Option<String>, Error> {
        let ancestors = snap.ancestors(&item.id);
        let explicit =
            binding(std::iter::once(item).chain(ancestors.iter().filter_map(|id| snap.item(id))))
                .map_err(Error::Internal)?;
        if explicit.is_some() {
            return Ok(explicit);
        }
        // Only an exact repository path or canonical conversation match is automatic.
        // Multiple projects on one repository require an explicit `project` reference.
        let repo = Workspace::repo_of(&item.writable_resources);
        let matches: Vec<_> = self
            .store
            .projects()
            .list()
            .await?
            .into_iter()
            .filter(|p| p.created_at <= item.created_at)
            .filter(|p| {
                repo.as_ref()
                    .is_some_and(|r| p.repository_id.as_deref() == r.to_str())
                    || item
                        .origin_conversation_id
                        .as_ref()
                        .is_some_and(|c| p.canonical_conversation_id.as_ref() == Some(c))
            })
            .map(|p| p.id.to_string())
            .collect();
        match matches.as_slice() {
            [] => Ok(None),
            [id] => Ok(Some(id.clone())),
            _ => Err(Error::Internal(
                "multiple projects match this work; name a project artifact reference".into(),
            )),
        }
    }

    pub(super) async fn project_context(
        &self,
        snap: &Snapshot,
        item: &WorkItem,
    ) -> Result<Option<ProjectContext>, Error> {
        let Some(id) = self.project_id(snap, item).await? else {
            return Ok(None);
        };
        let project_id = id
            .parse::<ProjectId>()
            .map_err(|_| Error::Internal(format!("invalid project reference {id}")))?;
        let snapshot = self
            .store
            .projects()
            .get(&project_id)
            .await?
            .ok_or_else(|| Error::Internal(format!("project {id} is not available")))?;
        if snapshot.project.status != ProjectStatus::Active {
            return Err(Error::Internal(format!("project {id} is not active")));
        }
        let mut members = BTreeMap::new();
        // Context evidence and work history outlive work-row compaction. Only the
        // controller can establish a historical binding; model artifacts cannot.
        for ev in self.store.work_evidence_of_kind(PROJECT_CONTEXT).await? {
            if ev.verified_by.as_deref() != Some(CONTROLLER) {
                continue;
            }
            let ctx: ProjectContext = serde_json::from_str(&ev.reference)
                .map_err(|e| Error::Internal(format!("invalid recorded project context: {e}")))?;
            let own = ctx.work.iter().find(|work| work.item == ev.item).cloned();
            members.insert(ev.item, (ctx.snapshot.project.id.to_string(), own));
        }
        let mut work = Vec::new();
        for row in snap.items() {
            let belongs = match members.get(&row.id) {
                Some((project, _)) => project == &id,
                None => self.project_id(snap, row).await?.as_deref() == Some(&id),
            };
            if !belongs {
                continue;
            }
            members.remove(&row.id);
            work.push(
                self.project_work(&row.id, &row.title, row.status, None, None)
                    .await?,
            );
        }
        for (item, (project, previous)) in members {
            if project != id {
                continue;
            }
            if let Some(row) = self.store.work_archive_get(&item).await? {
                work.push(
                    self.project_work(&item, &row.title, row.status, row.worker, previous.as_ref())
                        .await?,
                );
            } else {
                return Err(Error::Internal(format!(
                    "project work {item} has no durable status"
                )));
            }
        }
        work.sort_by(|a, b| a.item.cmp(&b.item));
        let ids: std::collections::BTreeSet<_> = work.iter().map(|w| w.item.as_str()).collect();
        let questions = self
            .store
            .questions_list(&Default::default())
            .await?
            .into_iter()
            .filter(|q| ids.contains(q.item.as_str()))
            .collect();
        let mut execution_items = std::collections::BTreeSet::from([item.id.clone()]);
        execution_items.extend(snap.ancestors(&item.id));
        let mut pending: Vec<_> = execution_items.iter().cloned().collect();
        while let Some(id) = pending.pop() {
            let inputs = snap
                .item(&id)
                .into_iter()
                .flat_map(|w| w.inputs_from.iter());
            let upstream = snap
                .edges_held_by(&id)
                .filter(|e| e.kind.is_ordering())
                .map(|e| &e.depends_on);
            for input in inputs.chain(upstream) {
                if execution_items.insert(input.clone()) {
                    pending.push(input.clone());
                }
            }
        }
        let context = ProjectContext {
            snapshot,
            work,
            base_sources: Vec::new(),
            questions,
            execution_items: execution_items.into_iter().collect(),
        };
        Ok(Some(context))
    }

    /// Check the actual prompt view after the workspace base has been pinned.
    pub(super) fn check_execution_context(context: &ProjectContext) -> Result<(), Error> {
        if serde_json::to_vec(&context.execution_view())
            .map_err(|e| Error::Internal(e.to_string()))?
            .len()
            > CONTEXT_BYTES
        {
            return Err(Error::Internal(
                "the execution slice exceeds the handoff limit; split this task into smaller items"
                    .into(),
            ));
        }
        Ok(())
    }

    async fn project_work(
        &self,
        item: &str,
        title: &str,
        status: Status,
        archived_worker: Option<String>,
        previous: Option<&ProjectWork>,
    ) -> Result<ProjectWork, Error> {
        let evidence = self.store.work_evidence_list(item).await?;
        let summary = evidence
            .iter()
            .rev()
            .find(|e| e.kind == "summary")
            .map(|e| e.reference.clone())
            .unwrap_or_default();
        let workspace = evidence
            .iter()
            .rev()
            .find(|e| e.kind == WORKSPACE_EVIDENCE && e.verified_by.is_none() && e.hash.is_some())
            .and_then(|e| serde_json::from_str::<Workspace>(&e.reference).ok());
        let repository = workspace
            .as_ref()
            .map(|ws| ws.repo.to_string_lossy().into_owned());
        let unfinished_attempt = if status != Status::Done {
            let run = evidence
                .iter()
                .rev()
                .find(|e| e.kind == "run" && e.verified_by.is_none())
                .map(|e| e.reference.clone());
            let error = super::load::last_error(&self.store.work_events(item).await?);
            (run.is_some() || workspace.is_some() || error.is_some()).then_some(ProjectAttempt {
                run,
                workspace,
                error,
            })
        } else {
            None
        };
        let worker = match archived_worker {
            Some(worker) => Some(worker),
            None => self
                .store
                .work_lease_history(item)
                .await?
                .last()
                .map(|lease| lease.lease.worker.clone()),
        };
        // Proof, not a worker's artifact pointers or unverified partial output.
        let evidence = evidence
            .into_iter()
            .filter(|e| match e.kind.as_str() {
                "commit" | "changed_path" => e.verified_by.as_deref() == Some("git"),
                "check_run" => e.verified_by.as_deref() == Some(crate::worker::COMMAND_RUN),
                "tool" => e.verified_by.as_deref() == Some("catalog"),
                _ => false,
            })
            .map(|e| rustykrab_core::work::ArtifactRef {
                kind: e.kind,
                value: e.reference,
            })
            .collect();
        let current = self.store.work_get(item).await?;
        Ok(ProjectWork {
            objective: current
                .as_ref()
                .map(|w| w.objective.clone())
                .or_else(|| previous.map(|w| w.objective.clone()))
                .unwrap_or_default(),
            done_when: current
                .as_ref()
                .map(|w| w.done_when.clone())
                .or_else(|| previous.map(|w| w.done_when.clone()))
                .unwrap_or_default(),
            constraints: current
                .as_ref()
                .map(|w| w.constraints.clone())
                .or_else(|| previous.map(|w| w.constraints.clone()))
                .unwrap_or_default(),
            decisions_made: current
                .as_ref()
                .map(|w| w.decisions_made.clone())
                .or_else(|| previous.map(|w| w.decisions_made.clone()))
                .unwrap_or_default(),
            item: item.to_owned(),
            title: title.to_owned(),
            status,
            worker,
            summary,
            evidence,
            repository,
            unfinished_attempt,
        })
    }

    /// A failed handoff never starts a worker on a guessed or stale base.
    pub(super) async fn hold_context(&self, item: &WorkItem, why: String) -> Result<usize, Error> {
        let mut b = Batch::new(self.load().await?, self.clock.now());
        b.move_to(
            &item.id,
            Status::Blocked(BlockedReason::NeedsDecision),
            "controller",
            why.clone(),
        );
        self.record_question(
            &mut b,
            NewQuestion::open(
                &item.id,
                QuestionKind::Decision,
                format!("Project handoff needs resolution: {why}"),
                "project_context",
            ),
        );
        b.settle(vec![item.id.clone()]);
        self.commit(b, &mut Default::default()).await?;
        Ok(1)
    }

    pub(super) fn context_evidence(
        item: &str,
        context: &ProjectContext,
    ) -> Result<Evidence, Error> {
        Ok(Evidence {
            item: item.to_owned(),
            kind: PROJECT_CONTEXT.into(),
            reference: serde_json::to_string(context)
                .map_err(|e| Error::Internal(e.to_string()))?,
            hash: Some(context.snapshot.revision.id.to_string()),
            verified_by: Some(CONTROLLER.into()),
            at: Utc::now(),
        })
    }
}
