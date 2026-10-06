//! The node side of the `peer` worker kind (plan
//! `docs/plans/control-layer-and-worker-fleet.md`, sections 5 and 12, Phase
//! 5): how a node runs a structured task a peer delegated to it.
//!
//! [`DelegatedRuns`] implements `rustykrab_control::peer::NodeWorkers`. For
//! each task it builds a [`LocalWorker`] whose tool ceiling is exactly what
//! the node's delegation policy allows that task (the gateway computes it:
//! the fixed denials, the node's allowlist, the task's own tighter limit, the
//! hop budget), so a delegated run is a local worker run in every other
//! respect: the brief rendered as its one user turn, its `required_tools`
//! activated before the first model call inside that ceiling (a tool outside
//! it fails the run before any model call, as a typed capability gap), the
//! budget enforced, the transcript kept, and the typed `ResultReport` its
//! `result_report` call handed in returned as the task's result.
//!
//! The run's `work_*` tools go to a [`DelegationBackend`] rather than this
//! node's controller: the item lives on the controller that delegated it, so
//! a report is recorded for the task and nothing more, `work_file` is
//! refused (follow-up work travels as `discovered` drafts in the report,
//! which that controller files), and `work_status` sees nothing.
//!
//! The advertisement a node gives (`GET /api/node`) is the ceiling as such a
//! worker sees it: its models, tools and MCP servers, with the node's
//! machine name and the resources a delegated run may write, which default
//! to none: a peer never writes this machine's repositories for a
//! controller that planned the work against its own.

use std::sync::Arc;

use async_trait::async_trait;
use rustykrab_control::peer::NodeWorkers;
use rustykrab_control::worker::{Worker, WorkerCapabilities};
use rustykrab_core::model::ModelProvider;
use rustykrab_core::work::{PlanOutcome, ResultReport, WorkItemDraft, WorkItemId};
use rustykrab_core::{AgentDefinition, Error, Result, Tool, ToolError};
use rustykrab_tools::work_backend::{
    Principal, Provenance, StatusQuery, ToolState, WorkBackend, WorkStatusView,
};
use tokio::sync::Semaphore;

use crate::local_worker::{LateTools, LocalWorker, RunTranscripts};
use crate::sandbox::Sandbox;

/// Builds the worker for each structured delegated task on this node.
pub struct DelegatedRuns {
    name: String,
    definition: AgentDefinition,
    provider: Arc<dyn ModelProvider>,
    tools: Vec<Arc<dyn Tool>>,
    sandbox: Arc<dyn Sandbox>,
    backend: Arc<DelegationBackend>,
    transcripts: Option<Arc<dyn RunTranscripts>>,
    late: Option<Arc<dyn LateTools>>,
    slot: Option<Arc<Semaphore>>,
    machine: Option<String>,
    writable_resources: Vec<String>,
}

impl DelegatedRuns {
    /// Delegated runs named `name` (the name their transcripts carry),
    /// running `definition` (the node's worker definition) on `provider`
    /// over the node's `tools`.
    pub fn new(
        name: impl Into<String>,
        definition: AgentDefinition,
        provider: Arc<dyn ModelProvider>,
        tools: Vec<Arc<dyn Tool>>,
        sandbox: Arc<dyn Sandbox>,
    ) -> DelegatedRuns {
        DelegatedRuns {
            name: name.into(),
            definition,
            provider,
            tools,
            sandbox,
            backend: Arc::new(DelegationBackend),
            transcripts: None,
            late: None,
            slot: None,
            machine: None,
            writable_resources: Vec::new(),
        }
    }

    /// Keep each run's conversation, as a local worker does.
    pub fn with_transcripts(mut self, transcripts: Arc<dyn RunTranscripts>) -> Self {
        self.transcripts = Some(transcripts);
        self
    }

    /// Tools written while the node runs, as the local worker takes them.
    pub fn with_late_tools(mut self, late: Arc<dyn LateTools>) -> Self {
        self.late = Some(late);
        self
    }

    /// Wait on the node's model slot, shared with its own local worker, so
    /// delegated and local work never share one KV slot (plan 12.1).
    pub fn with_slot(mut self, slot: Arc<Semaphore>) -> Self {
        self.slot = Some(slot);
        self
    }

    /// The machine name the node advertises.
    pub fn with_machine(mut self, machine: Option<String>) -> Self {
        self.machine = machine.filter(|m| !m.trim().is_empty());
        self
    }

    /// The resources a delegated run may write, as advertised; none by
    /// default.
    pub fn with_writable_resources(mut self, resources: Vec<String>) -> Self {
        self.writable_resources = resources;
        self
    }

    fn local(&self, allowed: &[String]) -> LocalWorker {
        let mut definition = self.definition.clone();
        definition.allowed_tools = Some(match &self.definition.allowed_tools {
            Some(own) => allowed
                .iter()
                .filter(|t| own.contains(t))
                .cloned()
                .collect(),
            None => allowed.to_vec(),
        });
        // The node, not the definition file, decides what a delegated run
        // may write.
        definition.writable_resources = self.writable_resources.clone();
        let mut worker = LocalWorker::new(
            self.name.clone(),
            definition,
            self.provider.clone(),
            self.tools.clone(),
            self.sandbox.clone(),
            self.backend.clone() as Arc<dyn WorkBackend>,
        );
        if let Some(t) = &self.transcripts {
            worker = worker.with_transcripts(t.clone());
        }
        if let Some(late) = &self.late {
            worker = worker.with_late_tools(late.clone());
        }
        if let Some(slot) = &self.slot {
            worker = worker.with_slot(slot.clone());
        }
        worker
    }
}

impl NodeWorkers for DelegatedRuns {
    fn worker(&self, allowed: &[String]) -> Arc<dyn Worker> {
        Arc::new(self.local(allowed))
    }

    fn advertise(&self, ceiling: &[String]) -> WorkerCapabilities {
        let caps = self.local(ceiling).capabilities();
        WorkerCapabilities {
            machine: self.machine.clone(),
            writable_resources: self.writable_resources.clone(),
            repos: Vec::new(),
            ..caps
        }
    }
}

/// The work backend of a delegated run: the item is on another machine, so
/// a report is simply accepted (the delegating controller verifies it),
/// filing is refused and there is nothing to read.
#[derive(Debug, Clone, Copy, Default)]
pub struct DelegationBackend;

#[async_trait]
impl WorkBackend for DelegationBackend {
    async fn file(&self, _draft: WorkItemDraft, _provenance: Provenance) -> Result<PlanOutcome> {
        Err(Error::ToolExecution(ToolError::invalid_input(
            "this run was delegated by another RustyKrab and cannot file work here: put \
             follow-up work in result_report's `discovered` drafts, which the delegating \
             controller files",
        )))
    }

    async fn status(
        &self,
        _query: StatusQuery,
        _principal: &Principal,
    ) -> Result<Vec<WorkStatusView>> {
        Ok(Vec::new())
    }

    async fn report(
        &self,
        _item: WorkItemId,
        _report: ResultReport,
        _provenance: Provenance,
    ) -> Result<()> {
        Ok(())
    }

    fn tool_state(&self, _name: &str) -> ToolState {
        ToolState::Unknown
    }

    fn mcp_server_configured(&self, _name: &str) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustykrab_control::worker::Brief;
    use rustykrab_core::model::{ModelResponse, StopReason, ToolChoice, Usage};
    use rustykrab_core::types::{Message, MessageContent, Role, ToolCall, ToolSchema};
    use rustykrab_core::work::{Budget, WorkKind};
    use serde_json::{json, Value};
    use std::sync::Mutex;

    use crate::sandbox::NoSandbox;

    /// Calls `result_report` once, recording the tools each request carried.
    struct Reporter {
        seen: Mutex<Vec<Vec<String>>>,
    }

    impl Reporter {
        fn respond(&self, tools: &[ToolSchema]) -> ModelResponse {
            let mut seen = self.seen.lock().unwrap();
            seen.push(tools.iter().map(|t| t.name.clone()).collect());
            let content = if seen.len() == 1 {
                MessageContent::ToolCall(ToolCall {
                    id: "c1".into(),
                    name: "result_report".into(),
                    arguments: json!({
                        "summary": "Checked Saturday.",
                        "artifacts": [{ "kind": "message", "value": "free" }],
                    }),
                })
            } else {
                MessageContent::Text("done".into())
            };
            ModelResponse {
                message: Message::stamped(Role::Assistant, content),
                usage: Usage::default(),
                stop_reason: if seen.len() == 1 {
                    StopReason::ToolUse
                } else {
                    StopReason::EndTurn
                },
                text: None,
            }
        }
    }

    #[async_trait]
    impl ModelProvider for Reporter {
        fn name(&self) -> &str {
            "node-model"
        }
        async fn chat(&self, _m: &[Message], tools: &[ToolSchema]) -> Result<ModelResponse> {
            Ok(self.respond(tools))
        }
        async fn chat_with_choice(
            &self,
            _m: &[Message],
            tools: &[ToolSchema],
            _: ToolChoice,
        ) -> Result<ModelResponse> {
            Ok(self.respond(tools))
        }
    }

    struct Named(&'static str);

    #[async_trait]
    impl Tool for Named {
        fn name(&self) -> &str {
            self.0
        }
        fn description(&self) -> &str {
            "test tool"
        }
        fn schema(&self) -> ToolSchema {
            ToolSchema {
                name: self.0.into(),
                description: "test tool".into(),
                parameters: json!({"type": "object", "properties": {}}),
            }
        }
        async fn execute(&self, _: Value) -> Result<Value> {
            Ok(json!({"ok": true}))
        }
    }

    fn runs(provider: Arc<Reporter>) -> DelegatedRuns {
        let tools: Vec<Arc<dyn Tool>> = vec![
            Arc::new(Named("caldav")),
            Arc::new(Named("read")),
            Arc::new(Named("exec")),
        ];
        DelegatedRuns::new(
            "node",
            LocalWorker::default_definition("node"),
            provider,
            tools,
            Arc::new(NoSandbox),
        )
        .with_machine(Some("m4max".into()))
    }

    fn brief(required: &[&str]) -> Brief {
        Brief {
            item: "41".into(),
            kind: WorkKind::Personal,
            title: "Check the harbour calendar".into(),
            objective: "Check Saturday".into(),
            done_when: "Saturday is listed".into(),
            constraints: Vec::new(),
            decisions_made: Vec::new(),
            artifact_refs: Vec::new(),
            required_tools: required.iter().map(|t| t.to_string()).collect(),
            required_mcp_servers: Vec::new(),
            writable_resources: Vec::new(),
            inputs: Vec::new(),
            more_inputs: Vec::new(),
            prior_evidence: Vec::new(),
            last_error: None,
            budget: Budget::default(),
            origin_conversation_id: None,
            run: Some(uuid::Uuid::new_v4().to_string()),
            workspace: None,
            capability: None,
            project_context: None,
        }
    }

    #[tokio::test]
    async fn a_required_tool_is_active_at_the_first_call_and_the_result_is_typed() {
        let provider = Arc::new(Reporter {
            seen: Mutex::new(Vec::new()),
        });
        let node = runs(provider.clone());
        let allowed = vec!["caldav".to_string(), "read".to_string()];
        let report = node
            .worker(&allowed)
            .run(brief(&["caldav"]))
            .await
            .expect("a typed report");
        assert_eq!(report.summary, "Checked Saturday.");
        let first = provider.seen.lock().unwrap()[0].clone();
        assert!(first.contains(&"caldav".to_string()), "{first:?}");
        assert!(
            !first.contains(&"exec".to_string()),
            "a tool outside the task's ceiling is never offered: {first:?}"
        );
    }

    #[tokio::test]
    async fn a_required_tool_outside_the_ceiling_fails_before_any_model_call() {
        let provider = Arc::new(Reporter {
            seen: Mutex::new(Vec::new()),
        });
        let node = runs(provider.clone());
        let err = node
            .worker(&["read".to_string()])
            .run(brief(&["caldav"]))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("caldav"), "{err}");
        assert!(provider.seen.lock().unwrap().is_empty());
    }

    #[test]
    fn the_advertisement_is_the_ceiling_with_the_nodes_machine() {
        let node = runs(Arc::new(Reporter {
            seen: Mutex::new(Vec::new()),
        }));
        let caps = node.advertise(&["caldav".to_string()]);
        assert!(caps.tools.contains(&"caldav".to_string()));
        assert!(!caps.tools.contains(&"exec".to_string()));
        assert_eq!(caps.models, ["node-model"]);
        assert_eq!(caps.machine.as_deref(), Some("m4max"));
        assert!(
            caps.writable_resources.is_empty(),
            "a peer writes nothing it was not configured to"
        );
    }

    #[tokio::test]
    async fn a_delegated_run_cannot_file_work_on_the_node() {
        let err = DelegationBackend
            .file(
                WorkItemDraft::default(),
                Provenance {
                    conversation_id: None,
                    filed_by_item: None,
                    actor: "worker:node".into(),
                },
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("discovered"));
    }
}
