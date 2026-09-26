//! Workers as the controller sees them (plan section 5): a name, a kind,
//! advertised capabilities, and one way to run a brief. The local sub-agent
//! implementation lives in `rustykrab-agent`; peers, Claude Code and Codex
//! come in later phases.

use async_trait::async_trait;
use rustykrab_core::work::{
    ArtifactRef, Budget, Evidence, InputRef, ResultReport, WorkItemId, WorkKind, WorkerKind,
};
use rustykrab_core::Error;
use serde::{Deserialize, Serialize};

/// What a worker advertises (plan section 5).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerCapabilities {
    pub models: Vec<String>,
    pub tools: Vec<String>,
    pub mcp_servers: Vec<String>,
    pub repos: Vec<String>,
    pub machine: Option<String>,
    pub writable_resources: Vec<String>,
}

/// The brief a worker receives (plan section 6, step 4, and 6.3). Typed
/// fields, pointers not prose; the worker opens what it needs from the store
/// and the recall archive.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Brief {
    pub item: WorkItemId,
    pub kind: WorkKind,
    pub title: String,
    pub objective: String,
    pub done_when: String,
    pub constraints: Vec<String>,
    pub decisions_made: Vec<String>,
    pub artifact_refs: Vec<ArtifactRef>,
    pub required_tools: Vec<String>,
    pub required_mcp_servers: Vec<String>,
    pub writable_resources: Vec<String>,
    /// The fan-in block, capped per plan section 6.3; ids beyond the cap are
    /// listed in `more_inputs`.
    pub inputs: Vec<InputRef>,
    pub more_inputs: Vec<WorkItemId>,
    /// Evidence from the item's own failed attempts, for a repair run.
    pub prior_evidence: Vec<Evidence>,
    /// The error class and detail of the last failure, for a repair run.
    pub last_error: Option<String>,
    pub budget: Budget,
    pub origin_conversation_id: Option<String>,
}

/// A worker the controller can lease an item to.
#[async_trait]
pub trait Worker: Send + Sync {
    /// Stable, human-addressable name ("pinch", "krabby").
    fn name(&self) -> &str;
    fn kind(&self) -> WorkerKind;
    fn capabilities(&self) -> WorkerCapabilities;
    /// How many items it may run at once.
    fn concurrency(&self) -> usize {
        1
    }
    fn healthy(&self) -> bool {
        true
    }
    /// Run one brief to its typed result. The controller verifies the result
    /// before anything counts; a worker never transitions an item.
    async fn run(&self, brief: Brief) -> Result<ResultReport, Error>;
}
