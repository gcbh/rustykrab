//! Phase 5 over a real router and task worker: a `PeerWorker` pointed at
//! this node reads its advertisement, submits a brief whose required tool
//! is active at the node's first model call, gets the typed result back,
//! finds its run again by id, and is refused a tool outside the ceiling.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use rustykrab_agent::sandbox::NoSandbox;
use rustykrab_agent::{DelegatedRuns, LocalWorker, PeerConfig, PeerWorker};
use rustykrab_control::errors::{classify, Context};
use rustykrab_control::worker::{run_failure_input, Brief, Worker};
use rustykrab_core::model::{ModelProvider, ModelResponse, StopReason, ToolChoice, Usage};
use rustykrab_core::types::{Message, MessageContent, Role, ToolCall, ToolSchema};
use rustykrab_core::work::{Budget, ErrorClass, ErrorSubclass, WorkKind, WorkerKind};
use rustykrab_core::Tool;
use rustykrab_store::{Store, TaskStatus};
use serde_json::{json, Value};
use uuid::Uuid;

use crate::AppState;

const TOKEN: &str = "peer-tests-token";

/// The node's model: reports once, and records the tools each request
/// carried.
struct NodeModel {
    seen: Mutex<Vec<Vec<String>>>,
}

impl NodeModel {
    fn respond(&self, messages: &[Message], tools: &[ToolSchema]) -> ModelResponse {
        self.seen
            .lock()
            .unwrap()
            .push(tools.iter().map(|t| t.name.clone()).collect());
        let reported = messages
            .iter()
            .any(|m| matches!(&m.content, MessageContent::ToolResult(_)));
        let (content, stop_reason) = if reported {
            (
                MessageContent::Text("Reported.".into()),
                StopReason::EndTurn,
            )
        } else {
            (
                MessageContent::ToolCall(ToolCall {
                    id: "c1".into(),
                    name: "result_report".into(),
                    arguments: json!({
                        "summary": "Saturday is free on the harbour calendar.",
                        "artifacts": [{ "kind": "message", "value": "e2e-peer-evidence" }],
                    }),
                }),
                StopReason::ToolUse,
            )
        };
        ModelResponse {
            message: Message::stamped(Role::Assistant, content),
            usage: Usage::default(),
            stop_reason,
            text: None,
        }
    }
}

#[async_trait]
impl ModelProvider for NodeModel {
    fn name(&self) -> &str {
        "node-model"
    }
    async fn chat(
        &self,
        messages: &[Message],
        tools: &[ToolSchema],
    ) -> rustykrab_core::Result<ModelResponse> {
        Ok(self.respond(messages, tools))
    }
    async fn chat_with_choice(
        &self,
        messages: &[Message],
        tools: &[ToolSchema],
        _: ToolChoice,
    ) -> rustykrab_core::Result<ModelResponse> {
        Ok(self.respond(messages, tools))
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
    async fn execute(&self, _: Value) -> rustykrab_core::Result<Value> {
        Ok(json!({"ok": true}))
    }
}

struct Node {
    base: String,
    store: Store,
    model: Arc<NodeModel>,
}

async fn node() -> Node {
    let dir = std::env::temp_dir().join(format!("rk-peer-node-{}", Uuid::new_v4()));
    let store = Store::open(&dir, vec![5u8; 32]).expect("store opens");
    let model = Arc::new(NodeModel {
        seen: Mutex::new(Vec::new()),
    });
    let tools: Vec<Arc<dyn Tool>> = vec![
        Arc::new(Named("caldav")),
        Arc::new(Named("read")),
        Arc::new(Named("credential_read")),
    ];
    let runs = DelegatedRuns::new(
        "node",
        LocalWorker::default_definition("node"),
        model.clone(),
        tools.clone(),
        Arc::new(NoSandbox),
    )
    .with_machine(Some("m4max".into()));
    let state = AppState::new(store.clone(), tools, model.clone(), TOKEN.into())
        .with_delegation(Arc::new(runs));
    tokio::spawn(crate::run_task_worker(state.clone()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = crate::router(state);
    tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap();
    });
    Node {
        base: format!("http://{addr}"),
        store,
        model,
    }
}

fn peer(base: &str) -> PeerWorker {
    let mut config = PeerConfig::new(base, TOKEN);
    config.poll = Duration::from_millis(50);
    config.timeout = Duration::from_secs(30);
    PeerWorker::new("krabby", config)
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
        run: Some(Uuid::new_v4().to_string()),
        workspace: None,
        capability: None,
    }
}

#[tokio::test]
async fn a_peer_runs_a_brief_on_a_node_with_its_tools_up_front() {
    let node = node().await;
    let krabby = peer(&node.base);

    // The advertisement: the delegation ceiling, never a credential tool.
    assert!(!krabby.healthy());
    assert!(krabby.refresh().await);
    assert!(krabby.healthy(), "{:?}", krabby.unavailable());
    let caps = krabby.capabilities();
    assert!(caps.tools.contains(&"caldav".to_string()), "{caps:?}");
    assert!(!caps.tools.contains(&"credential_read".to_string()));
    assert_eq!(caps.machine.as_deref(), Some("m4max"));
    assert_eq!(caps.models, ["node-model"]);
    assert!(
        !krabby.refresh().await,
        "a fresh advertisement is not re-read"
    );

    // The brief runs with caldav active at the first model call and comes
    // back typed.
    let brief = brief(&["caldav"]);
    let run = brief.run.clone().unwrap();
    let report = krabby.run(brief).await.expect("a typed result");
    assert_eq!(report.summary, "Saturday is free on the harbour calendar.");
    assert_eq!(report.artifacts[0].value, "e2e-peer-evidence");
    let first = node.model.seen.lock().unwrap()[0].clone();
    assert!(first.contains(&"caldav".to_string()), "{first:?}");
    assert!(!first.contains(&"credential_read".to_string()));
    assert!(krabby.usage(&run).is_some_and(|u| u.wall_ms > 0));

    // The node kept the task with its typed half and result.
    let task = node
        .store
        .tasks()
        .find_by_run(&run)
        .await
        .unwrap()
        .expect("the task");
    assert_eq!(task.status, TaskStatus::Done);
    assert_eq!(task.work_item_id.as_deref(), Some("41"));
    assert_eq!(task.required_tools, ["caldav"]);
    assert_eq!(task.report.unwrap().summary, report.summary);
    assert_eq!(task.conversation_id.as_deref(), Some(run.as_str()));

    // After a restart of the controller, the run is found again by id.
    assert!(krabby.resumable(&run).await);
    assert!(!krabby.resumable("a-run-this-node-never-had").await);
}

#[tokio::test]
async fn a_tool_outside_the_nodes_ceiling_is_refused_typed() {
    let node = node().await;
    let krabby = peer(&node.base);
    let class = |err: &rustykrab_core::Error| {
        let e = classify(
            &run_failure_input(err),
            &Context {
                tool: None,
                worker_kind: Some(WorkerKind::Peer),
            },
        );
        (e.class, e.subclass)
    };

    let denied = krabby.run(brief(&["credential_read"])).await.unwrap_err();
    assert_eq!(class(&denied), (ErrorClass::Policy, ErrorSubclass::Ceiling));
    let missing = krabby.run(brief(&["teleport"])).await.unwrap_err();
    assert_eq!(
        class(&missing),
        (ErrorClass::CapabilityGap, ErrorSubclass::ToolGap)
    );
    assert!(
        node.store.tasks().list(10).await.unwrap().is_empty(),
        "a refused submission queues nothing"
    );
    assert!(node.model.seen.lock().unwrap().is_empty());
}

#[tokio::test]
async fn the_advertisement_needs_the_token() {
    let node = node().await;
    let client = reqwest::Client::new();
    let anonymous = client
        .get(format!("{}/api/node", node.base))
        .send()
        .await
        .unwrap();
    assert!(
        matches!(anonymous.status().as_u16(), 401 | 403),
        "{}",
        anonymous.status()
    );
    let wrong = PeerWorker::new("krabby", PeerConfig::new(&node.base, "not-the-token"));
    assert!(wrong.refresh().await);
    assert!(!wrong.healthy());
}
