//! Provider-neutral project context reconstructed from durable planning and work.
//! `project` artifact references bind work to a project; ancestors share that binding.
use rustykrab_core::work::{ArtifactRef, Status, WorkItem};
use rustykrab_projects::ProjectSnapshot;
use serde::{Deserialize, Serialize};

pub const PROJECT_REF: &str = "project";
pub const PROJECT_CONTEXT: &str = "project_context";
pub const CONTROLLER: &str = "controller";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProjectContext {
    /// Exact immutable planning revision delivered to this run, including provenance.
    pub snapshot: ProjectSnapshot,
    /// Durable progress, including open work. Summaries are reports; only evidence is verified.
    pub work: Vec<ProjectWork>,
    /// Completed work whose verified code is present in the pinned workspace base.
    pub base_sources: Vec<String>,
    /// Project-scoped work questions and their answers, retained after work compaction.
    #[serde(default)]
    pub questions: Vec<rustykrab_store::QuestionRow>,
    /// Work relevant to this execution slice. The complete history remains in this receipt.
    #[serde(default)]
    pub execution_items: Vec<String>,
}

/// The prompt view: exact project intent and the current task's dependencies,
/// rather than every earlier task's detailed history.
#[derive(Serialize)]
pub struct ProjectExecutionContext<'a> {
    pub snapshot: &'a ProjectSnapshot,
    pub work: Vec<&'a ProjectWork>,
    pub base_sources: &'a [String],
    pub questions: Vec<&'a rustykrab_store::QuestionRow>,
    pub history_items: usize,
}

impl ProjectContext {
    pub fn execution_view(&self) -> ProjectExecutionContext<'_> {
        let relevant = |id: &str| {
            self.execution_items.is_empty() || self.execution_items.iter().any(|item| item == id)
        };
        ProjectExecutionContext {
            snapshot: &self.snapshot,
            work: self.work.iter().filter(|w| relevant(&w.item)).collect(),
            base_sources: &self.base_sources,
            questions: self
                .questions
                .iter()
                .filter(|q| relevant(&q.item))
                .collect(),
            history_items: self.work.len(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProjectWork {
    pub item: String,
    pub title: String,
    pub status: Status,
    pub worker: Option<String>,
    pub summary: String,
    #[serde(default)]
    pub objective: String,
    #[serde(default)]
    pub done_when: String,
    #[serde(default)]
    pub constraints: Vec<String>,
    #[serde(default)]
    pub decisions_made: Vec<String>,
    pub evidence: Vec<ArtifactRef>,
    /// Repository scoped by the controller's recorded workspace, never by a model's claim.
    pub repository: Option<String>,
    /// Inspection pointers only. Partial effects are not accepted code or proof.
    #[serde(default)]
    pub unfinished_attempt: Option<ProjectAttempt>,
}

/// Explicit binding on an item or its ancestors. Conflicting identities fail closed.
pub fn binding<'a>(
    items: impl IntoIterator<Item = &'a WorkItem>,
) -> Result<Option<String>, String> {
    let mut id: Option<String> = None;
    for item in items {
        for r in item.artifact_refs.iter().filter(|r| r.kind == PROJECT_REF) {
            if id.as_ref().is_some_and(|id| id != &r.value) {
                return Err("work and its ancestors name different projects".into());
            }
            id = Some(r.value.clone());
        }
    }
    Ok(id)
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProjectAttempt {
    pub run: Option<String>,
    pub workspace: Option<crate::workspace::Workspace>,
    pub error: Option<rustykrab_core::work::WorkError>,
}
