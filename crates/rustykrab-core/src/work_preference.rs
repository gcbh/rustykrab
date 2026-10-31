//! Explicit human decisions recorded as reusable, exact work templates.
//! Templates are data, not a model classifier or a new execution path.
use crate::work::{
    ArtifactRef, Budget, Precondition, Trigger, WorkFacets, WorkItem, WorkKind, WorkerKind,
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum WorkStartMode {
    #[default]
    Once,
    Automatic,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkTemplate {
    pub example_item: String,
    pub title: String,
    pub kind: WorkKind,
    /// Includes canonical-conversation/repository inference performed by the host.
    #[serde(default)]
    pub project_id: Option<String>,
    pub objective: String,
    pub done_when: String,
    pub constraints: Vec<String>,
    pub decisions_made: Vec<String>,
    pub artifact_refs: Vec<ArtifactRef>,
    pub required_tools: Vec<String>,
    pub required_mcp_servers: Vec<String>,
    pub writable_resources: Vec<String>,
    pub worker_kind: WorkerKind,
    pub inputs_from: Vec<String>,
    pub trigger: Trigger,
    pub preconditions: Vec<Precondition>,
    pub budget: Budget,
    pub facets: Option<WorkFacets>,
}
impl WorkTemplate {
    pub fn capture(i: &WorkItem, facets: Option<WorkFacets>) -> Self {
        Self {
            example_item: i.id.clone(),
            title: i.title.clone(),
            kind: i.kind,
            project_id: None,
            objective: i.objective.clone(),
            done_when: i.done_when.clone(),
            constraints: i.constraints.clone(),
            decisions_made: i.decisions_made.clone(),
            artifact_refs: i.artifact_refs.clone(),
            required_tools: i.required_tools.clone(),
            required_mcp_servers: i.required_mcp_servers.clone(),
            writable_resources: i.writable_resources.clone(),
            worker_kind: i.worker_kind,
            inputs_from: i.inputs_from.clone(),
            trigger: i.trigger.clone(),
            preconditions: i.preconditions.clone(),
            budget: i.budget,
            facets,
        }
    }
    /// Exact reviewed content/scope; changing any field needs another human decision.
    /// Runtime ids, statuses, priorities and timestamps cannot confer authority.
    pub fn matches(&self, candidate: &Self) -> bool {
        let mut candidate = candidate.clone();
        candidate.example_item = self.example_item.clone();
        let mut reviewed = self.clone();
        // Legacy task labels and backend pins have no execution semantics.
        // Approval still requires the same objective, tools, resources,
        // constraints, recovery rungs and internal lifecycle facets.
        for template in [&mut reviewed, &mut candidate] {
            if !matches!(template.kind, WorkKind::Proposal | WorkKind::Capability) {
                template.kind = WorkKind::Task;
            }
            template.worker_kind = WorkerKind::Any;
            template.budget = template.budget.without_execution_limits();
        }
        reviewed == candidate
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkStartReply {
    pub item: String,
    pub mode: WorkStartMode,
    pub preference_id: String,
    pub released: Vec<String>,
    pub implementation: Option<String>,
}

// Preserve JudgmentCheck's Hash contract. Equal templates have equal identity,
// title and objective; omitted fields may collide but still compare through Eq.
impl std::hash::Hash for WorkTemplate {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.example_item.hash(state);
        self.title.hash(state);
        self.kind.hash(state);
        self.objective.hash(state);
        self.done_when.hash(state);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn scope() -> WorkTemplate {
        WorkTemplate {
            example_item: "original".into(),
            title: "Useful work".into(),
            kind: WorkKind::Task,
            project_id: None,
            objective: "Complete the task".into(),
            done_when: "Verified result".into(),
            constraints: vec!["Only the assigned repository".into()],
            decisions_made: vec![],
            artifact_refs: vec![],
            required_tools: vec![],
            required_mcp_servers: vec![],
            writable_resources: vec![],
            worker_kind: WorkerKind::Any,
            inputs_from: vec![],
            trigger: Trigger::Now,
            preconditions: vec![],
            budget: Budget::default(),
            facets: None,
        }
    }
    #[test]
    fn retired_execution_estimates_do_not_change_an_otherwise_identical_reviewed_scope() {
        let original = scope();
        let mut candidate = original.clone();
        candidate.budget.tokens = 1_603_490;
        candidate.budget.iterations = 1;
        candidate.budget.wall_seconds = 10;
        assert!(original.matches(&candidate));
        candidate.required_tools.push("exec".into());
        assert!(
            !original.matches(&candidate),
            "tool authority remains exact"
        );
    }
    #[test]
    fn finite_recovery_rungs_and_constraints_still_require_the_reviewed_scope() {
        let original = scope();
        let mut candidate = original.clone();
        candidate.budget.rungs.repairs += 1;
        assert!(!original.matches(&candidate));
        candidate = original.clone();
        candidate.constraints.clear();
        assert!(!original.matches(&candidate));
    }
}
