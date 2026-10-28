//! A conflicted handoff is executable technical work, not a request for the
//! owner to perform Git surgery. One durable repair per project/repository/parent
//! pair uses the normal graph, routing, approval, lease and cumulative budget.
use rustykrab_core::work::{
    ArtifactRef, EdgeKind, ItemRef, PlanEdge, Status, WorkItem, WorkItemDraft, WorkKind, WorkPlan,
};
use rustykrab_core::Error;
use rustykrab_store::WorkOp;
use serde::{Deserialize, Serialize};

use super::batch::Batch;
use super::Controller;
use crate::graph::FilingSource;
use crate::workspace::{ContinuationError, MergeConflict, Workspace};
use crate::Provenance;

/// Only a controller-authored row of this kind can select a repair workspace
/// or require the additional verifier. A worker cannot mint one in its report.
pub(super) const MERGE_RECOVERY: &str = "merge_recovery";
const CONTROLLER: &str = "controller";
pub(super) const LEGACY_CONSTRAINT: &str = "Work only in the supplied isolated worktree. Preserve both parent histories. Do not move the repository checkout/main, push, deploy, answer owner questions or change budgets.";

fn execution_constraints() -> Vec<String> {
    vec![
        "Plan, implement and verify this repair yourself; choose and revise the approach within this assignment.".into(),
        "Work only in the supplied isolated worktree. Preserve both parent histories and applicable recorded owner decisions. This brief is the repair's execution contract; copied historical constraints in project context remain scoped to their original source tasks.".into(),
        "Do not move the repository checkout/main, push, deploy, answer owner questions or change budgets.".into(),
    ]
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct Recovery {
    pub repair: String,
    pub project: String,
    pub conflict: MergeConflict,
}
impl Recovery {
    fn key(&self) -> String {
        let mut parents = [&self.conflict.left, &self.conflict.right];
        parents.sort();
        crate::capability_proof::binding_digest(&(
            &self.project,
            &self.conflict.repository,
            parents,
        ))
    }
}

impl Controller {
    /// Only fresh leases recognize the exact old controller-generated form.
    /// Retain stored history, and never erase additional/edited repair-specific
    /// constraints. Untrusted markers cannot opt into this execution contract.
    pub(super) async fn merge_execution_constraints(
        &self,
        snap: &crate::graph::Snapshot,
        item: &WorkItem,
    ) -> Result<Vec<String>, Error> {
        if item.constraints.last().map(String::as_str) != Some(LEGACY_CONSTRAINT) {
            return Ok(item.constraints.clone());
        }
        let source = item
            .artifact_refs
            .iter()
            .find(|a| a.kind == "item")
            .and_then(|a| snap.item(&a.value));
        let copied_source = source.is_some_and(|source| {
            item.constraints[..item.constraints.len() - 1] == source.constraints
        });
        if copied_source && self.merge_recovery(item).await?.is_some() {
            return Ok(execution_constraints());
        }
        Ok(item.constraints.clone())
    }

    pub(super) async fn merge_recovery(&self, item: &WorkItem) -> Result<Option<Recovery>, Error> {
        if item.kind != WorkKind::Internal {
            return Ok(None);
        }
        let rows = self.store.work_evidence_of_kind(MERGE_RECOVERY).await?;
        let mut selected = None;
        for row in rows
            .iter()
            .filter(|e| e.verified_by.as_deref() == Some(CONTROLLER))
        {
            let recovery: Recovery = serde_json::from_str(&row.reference)
                .map_err(|e| Error::Internal(format!("invalid merge recovery record: {e}")))?;
            if recovery.repair == item.id {
                selected = Some(recovery);
                break;
            }
        }
        let Some(recovery) = selected else {
            return Ok(None);
        };
        if Workspace::repo_of(&item.writable_resources).as_ref()
            != Some(&recovery.conflict.repository)
            || !item
                .artifact_refs
                .iter()
                .any(|a| a.kind == crate::handoff::PROJECT_REF && a.value == recovery.project)
        {
            return Err(Error::Internal(
                "merge recovery record does not match its work scope".into(),
            ));
        }
        Ok(Some(recovery))
    }

    /// File or reuse the same repair even when it failed, exhausted its budget
    /// or was archived. A new waiter/restart never buys another repair budget.
    pub(super) async fn ensure_merge_recovery(
        &self,
        b: &mut Batch,
        item: &WorkItem,
        conflict: MergeConflict,
    ) -> Result<String, Error> {
        let project = self.project_id(&b.snap, item).await?.ok_or_else(|| {
            Error::Internal("merge recovery requires an identified project".into())
        })?;
        if Workspace::repo_of(&item.writable_resources).as_ref() != Some(&conflict.repository) {
            return Err(Error::Internal(
                "conflict is outside the work's repository".into(),
            ));
        }
        if self.merge_recovery(item).await?.is_some() {
            return Err(Error::Internal(
                "a merge recovery cannot recursively repair its own preparation".into(),
            ));
        }
        let mut recovery = Recovery {
            repair: String::new(),
            project,
            conflict,
        };
        let key = recovery.key();
        let mut recorded = self.store.work_evidence_of_kind(MERGE_RECOVERY).await?;
        recorded.extend(
            b.evidence
                .iter()
                .filter(|e| e.kind == MERGE_RECOVERY)
                .cloned(),
        );
        let mut existing = None;
        for row in recorded
            .iter()
            .filter(|e| e.verified_by.as_deref() == Some(CONTROLLER))
        {
            let old: Recovery = serde_json::from_str(&row.reference)
                .map_err(|e| Error::Internal(format!("invalid shared merge recovery: {e}")))?;
            if old.key() != key {
                continue;
            }
            if let Some(found) = b.snap.item(&old.repair) {
                if found.status.is_closed() {
                    return Err(Error::Internal(format!(
                        "shared Git repair {} is {}; inspect that repair instead of allocating a fresh budget",
                        found.id, found.status)));
                }
                existing = Some(found.id.clone());
                break;
            }
            if self.store.work_archive_get(&old.repair).await?.is_some() {
                return Err(Error::Internal(format!(
                    "shared Git repair {} is archived; inspect its retained result",
                    old.repair
                )));
            }
        }
        let upstream = existing.as_ref().map_or_else(
            || ItemRef::Tmp {
                tmp: "merge-repair".into(),
            },
            |id| ItemRef::Id(id.clone()),
        );
        if let Some(id) = &existing {
            if b.snap
                .edges_held_by(&item.id)
                .any(|e| e.kind == EdgeKind::WaitsFor && e.depends_on == *id)
            {
                return Ok(id.clone());
            }
        }
        let mut budget = self.config.default_budget;
        // Retrying/repairing/switching this one job is bounded normally. It must
        // not create an endless chain of new investigations of its own failure.
        budget.rungs.improvements = 0;
        budget.rungs.replans = 0;
        let c = &recovery.conflict;
        // This independent prerequisite implements a Git repair, even when
        // the first waiter is a planning-only step. Its task-local contract
        // must not become the repair's contract. All waiters' constraints and
        // owner decisions remain in project context with their source items.
        let constraints = execution_constraints();
        let draft = WorkItemDraft {
            tmp: Some("merge-repair".into()), kind: Some(WorkKind::Internal),
            title: format!("Resolve shared Git conflict {}", &key[..12]),
            objective: format!("Resolve the handoff conflict for project {} in {}. The controller supplies a clean isolated worktree pinned to {}. Integrate {} there, resolve conflicts in {:?} according to the current project intent and recorded owner decisions, run relevant checks, commit the resolution on the supplied branch, and report its exact commit and changed paths. This is the repair itself; do not wait for the blocked original task or file another acquisition. Observed Git diagnostic: {}",
                recovery.project, c.repository.display(), c.left, c.right, c.paths, c.detail),
            done_when: "Git verifies the result on this run's branch, descending from both recorded parents, with exact changed paths; relevant checks are reported honestly. Original handoffs are rechecked independently before execution.".into(),
            constraints,
            artifact_refs: vec![ArtifactRef {kind: "item".into(), value: item.id.clone()},
                ArtifactRef {kind: crate::handoff::PROJECT_REF.into(), value: recovery.project.clone()}],
            writable_resources: vec![format!("repo:{}", c.repository.display())],
            budget: Some(budget), priority: item.priority,
            ..Default::default()
        };
        let plan = WorkPlan { root: upstream.clone(), items: if existing.is_some() {vec![]} else {vec![draft]},
            edges: vec![PlanEdge {item: ItemRef::Id(item.id.clone()), kind: EdgeKind::WaitsFor, depends_on: upstream}],
            rationale: format!("One shared executable Git recovery for {key}; retain original work and independent holds") };
        let accepted = self
            .file_into(
                b,
                &plan,
                &Provenance {
                    actor: CONTROLLER.into(),
                    filed_by_item: Some(item.id.clone()),
                    conversation_id: item.origin_conversation_id.clone(),
                },
                FilingSource::Ladder,
            )
            .map_err(|e| Error::Internal(format!("merge recovery filing refused: {e:?}")))?;
        if existing.is_some() {
            let artifact = ArtifactRef {
                kind: "item".into(),
                value: item.id.clone(),
            };
            if let Some(repair) = b.snap.item_mut(&accepted.root) {
                if !repair.artifact_refs.contains(&artifact) {
                    repair.artifact_refs.push(artifact.clone());
                    b.ops.push(WorkOp::AddArtifactRef {
                        item: accepted.root.clone(),
                        artifact,
                    });
                }
            }
        }
        if existing.is_none() {
            recovery.repair = accepted.root.clone();
            b.evidence(
                &item.id,
                MERGE_RECOVERY,
                serde_json::to_string(&recovery).map_err(|e| Error::Internal(e.to_string()))?,
                Some(CONTROLLER),
            );
        }
        Ok(accepted.root.clone())
    }

    pub(super) async fn hold_workspace(
        &self,
        item: &WorkItem,
        error: ContinuationError,
    ) -> Result<usize, Error> {
        let conflict = match error {
            ContinuationError::Other(why) => return self.hold_context(item, why).await,
            ContinuationError::Conflict(conflict) => conflict,
        };
        let mut b = Batch::new(self.load().await?, self.clock.now());
        match self.ensure_merge_recovery(&mut b, item, conflict).await {
            Ok(repair) => {
                let changed = b.move_to(
                    &item.id,
                    Status::Queued,
                    CONTROLLER,
                    format!("Waiting for shared Git repair {repair}; no owner Git action required"),
                );
                b.settle(changed);
                self.commit(b, &mut Default::default()).await?;
                Ok(1)
            }
            Err(why) => self.hold_context(item, why.to_string()).await,
        }
    }

    /// Ordinary Git claim verification has already checked branch, base and
    /// exact paths. A repair additionally needs a real commit containing BOTH
    /// observed histories; a no-op or a one-parent edit is not a resolution.
    pub(super) async fn verify_merge_recovery(
        &self,
        item: &WorkItem,
        verified_commit: Option<String>,
    ) -> Result<Option<String>, Error> {
        let Some(recovery) = self.merge_recovery(item).await? else {
            return Ok(None);
        };
        let Some(commit) = verified_commit else {
            return Ok(Some(
                "shared Git repair must land a Git-verified commit containing both parents".into(),
            ));
        };
        let verdict = tokio::task::spawn_blocking(move || {
            for parent in [&recovery.conflict.left, &recovery.conflict.right] {
                if !crate::workspace::is_ancestor(&recovery.conflict.repository, parent, &commit)? {
                    return Ok(Some(format!(
                        "shared Git repair {commit} does not contain recorded parent {parent}"
                    )));
                }
            }
            Ok::<_, String>(None)
        })
        .await
        .map_err(|e| Error::Internal(e.to_string()))?;
        Ok(match verdict {
            Ok(why) => why,
            Err(why) => Some(format!("cannot verify shared Git repair ancestry: {why}")),
        })
    }
}
