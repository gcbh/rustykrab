//! The append path (plan `docs/plans/control-layer-and-worker-fleet.md`,
//! section 12, Phase 2): a run's tools array is the one its first request
//! carried, late tools arrive as text in a tool result, calls to them are
//! dispatched, and compaction folds them into the array.
//!
//! A child module of `runner` in its own file, so it reaches the runner's
//! private items without growing `runner.rs`.

use super::*;
use async_trait::async_trait;
use rustykrab_core::capability::CapabilitySet;
use rustykrab_core::model::{ModelResponse, StopReason, ToolChoice, Usage};
use rustykrab_tools::{TaskCompleteTool, ToolsListTool, ToolsLoadTool};
use serde_json::{json, Value};

use crate::sandbox::NoSandbox;

/// Plays a script and records each request's declared tool names and
/// messages. Whether it takes calls to undeclared tools is a flag, the way
/// real providers carry it as capability data.
struct Recording {
    accepts_undeclared: bool,
    script: Mutex<Vec<ModelResponse>>,
    requests: Mutex<Vec<(Vec<String>, Vec<Message>)>>,
}

impl Recording {
    fn new(accepts_undeclared: bool, script: Vec<ModelResponse>) -> Arc<Self> {
        Arc::new(Self {
            accepts_undeclared,
            script: Mutex::new(script),
            requests: Mutex::new(Vec::new()),
        })
    }

    fn declared(&self) -> Vec<Vec<String>> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .map(|(names, _)| names.clone())
            .collect()
    }

    fn next(&self, messages: &[Message], tools: &[ToolSchema]) -> ModelResponse {
        let mut names: Vec<String> = tools.iter().map(|t| t.name.clone()).collect();
        names.sort();
        self.requests
            .lock()
            .unwrap()
            .push((names, messages.to_vec()));
        let mut script = self.script.lock().unwrap();
        if script.is_empty() {
            return respond(
                MessageContent::Text("- summarized".into()),
                StopReason::EndTurn,
            );
        }
        script.remove(0)
    }
}

#[async_trait]
impl ModelProvider for Recording {
    fn name(&self) -> &str {
        "append-recording"
    }
    fn accepts_undeclared_tool_calls(&self) -> bool {
        self.accepts_undeclared
    }
    async fn chat(&self, messages: &[Message], tools: &[ToolSchema]) -> Result<ModelResponse> {
        Ok(self.next(messages, tools))
    }
    async fn chat_with_choice(
        &self,
        messages: &[Message],
        tools: &[ToolSchema],
        _: ToolChoice,
    ) -> Result<ModelResponse> {
        Ok(self.next(messages, tools))
    }
}

/// A catalog tool that answers with a fixed payload.
struct Catalog(&'static str, &'static str);

#[async_trait]
impl Tool for Catalog {
    fn name(&self) -> &str {
        self.0
    }
    fn description(&self) -> &str {
        self.1
    }
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: self.0.into(),
            description: self.1.into(),
            parameters: json!({
                "type": "object",
                "properties": { "city": { "type": "string" } },
                "required": ["city"]
            }),
        }
    }
    async fn execute(&self, args: Value) -> Result<Value> {
        Ok(json!({ "tool": self.0, "city": args["city"], "temperature_c": 17 }))
    }
}

fn respond(content: MessageContent, stop_reason: StopReason) -> ModelResponse {
    ModelResponse {
        message: Message::stamped(Role::Assistant, content),
        usage: Usage::default(),
        stop_reason,
        text: None,
    }
}

fn call(name: &str, args: Value) -> ModelResponse {
    respond(
        MessageContent::ToolCall(ToolCall {
            id: Uuid::new_v4().to_string(),
            name: name.into(),
            arguments: args,
        }),
        StopReason::ToolUse,
    )
}

fn text(t: &str) -> ModelResponse {
    respond(MessageContent::Text(t.into()), StopReason::EndTurn)
}

fn tools() -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(ToolsListTool::new()),
        Arc::new(ToolsLoadTool::new()),
        Arc::new(TaskCompleteTool::new()),
        Arc::new(Catalog(
            "get_forecast",
            "Get a multi-day weather forecast for a city.",
        )),
        Arc::new(Catalog(
            "get_weather",
            "Get the current weather for a city.",
        )),
        Arc::new(Catalog("memory_search", "Search long-term memory.")),
    ]
}

/// A runner over [`tools`], and a session that may call all of them.
fn setup(provider: Arc<Recording>) -> (AgentRunner, Session, Arc<ActiveToolsRegistry>) {
    let active = Arc::new(ActiveToolsRegistry::new());
    let runner =
        AgentRunner::new(provider, tools(), Arc::new(NoSandbox)).with_active_tools(active.clone());
    let names: Vec<String> = tools().iter().map(|t| t.name().to_string()).collect();
    let names: Vec<&str> = names.iter().map(String::as_str).collect();
    let session =
        Session::with_capabilities(Uuid::new_v4(), CapabilitySet::for_tools_permissive(&names));
    (runner, session, active)
}

fn conversation(id: Uuid) -> Conversation {
    Conversation {
        id,
        messages: vec![
            Message::stamped(Role::System, MessageContent::Text("You help.".into())),
            Message::stamped(
                Role::User,
                MessageContent::Text("What's the weather in Lisbon right now?".into()),
            ),
        ],
        created_at: Utc::now(),
        updated_at: Utc::now(),
        title: None,
        summary: None,
        detected_profile: None,
        channel_source: None,
        channel_id: None,
        channel_thread_id: None,
    }
}

/// The output of every tool result for calls to `tool`, as text.
fn results_of(conv: &Conversation, tool: &str) -> Vec<(bool, String)> {
    let ids: Vec<String> = conv
        .messages
        .iter()
        .flat_map(|m| m.content.tool_calls())
        .filter(|c| c.name == tool)
        .map(|c| c.id.clone())
        .collect();
    conv.messages
        .iter()
        .filter_map(|m| match &m.content {
            MessageContent::ToolResult(r) if ids.contains(&r.call_id) => Some((
                r.is_error,
                r.output
                    .as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| r.output.to_string()),
            )),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn under_append_the_tools_array_never_changes_during_a_run() {
    let provider = Recording::new(
        true,
        vec![
            call("tools_list", json!({ "query": "current weather" })),
            // gemma4's redundant load: a no-op that answers "already".
            call("tools_load", json!({ "names": ["get_weather"] })),
            call("get_weather", json!({ "city": "Lisbon" })),
            text("It is 17C in Lisbon."),
            call(
                "task_complete",
                json!({ "summary": "It is 17C in Lisbon." }),
            ),
        ],
    );
    let (runner, session, active) = setup(provider.clone());
    let conv_id = session.conversation_id;
    let mut conv = conversation(conv_id);

    runner.run(&mut conv, &session).await.unwrap();

    let declared = provider.declared();
    assert_eq!(declared.len(), 5, "one request per scripted turn");
    assert!(
        declared.iter().all(|d| d == &declared[0]),
        "the tools array changed mid-run: {declared:?}"
    );
    assert!(!declared[0].contains(&"get_weather".to_string()));
    assert!(!declared[0].contains(&"task_complete".to_string()));
    assert_eq!(active.late_binding(conv_id), LateToolBinding::Append);

    // The search delivered the definition as text, and only the match.
    let (failed, found) = results_of(&conv, "tools_list").remove(0);
    assert!(!failed);
    assert!(found.contains("\"name\":\"get_weather\""), "{found}");
    assert!(!found.contains("get_forecast"), "{found}");
    // The redundant load changed nothing and said so.
    let (_, load) = results_of(&conv, "tools_load").remove(0);
    assert!(load.contains("Already callable"), "{load}");
    // The appended tool was dispatched like any other.
    let (failed, weather) = results_of(&conv, "get_weather").remove(0);
    assert!(!failed, "{weather}");
    assert!(weather.contains("17"), "{weather}");
    // task_complete arrived by append too, its definition on the reminder.
    let reminder = conv
        .messages
        .iter()
        .find(|m| is_completion_reminder(m))
        .and_then(|m| m.content.as_text())
        .expect("the text answer was followed by a reminder");
    assert!(
        reminder.contains("\"name\":\"task_complete\""),
        "{reminder}"
    );
    assert_eq!(
        active.appended_for(conv_id),
        ["get_weather", "task_complete"]
    );
    assert_eq!(
        conv.messages.last().and_then(|m| m.content.as_text()),
        Some("It is 17C in Lisbon.")
    );
}

#[tokio::test]
async fn without_the_capability_a_search_rerenders_the_array() {
    let provider = Recording::new(
        false,
        vec![
            call("tools_list", json!({ "query": "current weather" })),
            call("task_complete", json!({ "summary": "done" })),
        ],
    );
    let (runner, session, active) = setup(provider.clone());
    let mut conv = conversation(session.conversation_id);

    runner.run(&mut conv, &session).await.unwrap();

    let declared = provider.declared();
    assert!(!declared[0].contains(&"get_weather".to_string()));
    assert!(declared[1].contains(&"get_weather".to_string()));
    assert!(declared[1].contains(&"task_complete".to_string()));
    assert_eq!(
        active.late_binding(session.conversation_id),
        LateToolBinding::Rerender
    );
}

#[tokio::test]
async fn a_profile_override_beats_the_providers_capability() {
    let provider = Recording::new(
        true,
        vec![
            call("tools_list", json!({ "query": "current weather" })),
            call("task_complete", json!({ "summary": "done" })),
        ],
    );
    let (runner, session, _) = setup(provider.clone());
    let runner = runner.with_config(AgentConfig {
        late_tool_binding: Some(LateToolBinding::Rerender),
        ..AgentConfig::default()
    });
    let mut conv = conversation(session.conversation_id);

    runner.run(&mut conv, &session).await.unwrap();

    assert!(provider.declared()[1].contains(&"get_weather".to_string()));
}

#[tokio::test]
async fn compaction_folds_appended_tools_into_the_array() {
    let provider = Recording::new(true, Vec::new());
    let (runner, session, active) = setup(provider.clone());
    let conv_id = session.conversation_id;
    active.set_late_binding(conv_id, LateToolBinding::Append);
    active.make_callable(conv_id, ["get_weather"]);
    let before = active.version(conv_id);
    let mut conv = conversation(conv_id);
    conv.messages.push(Message::stamped(
        Role::Assistant,
        MessageContent::Text("Checking. ".repeat(40)),
    ));

    runner.compact_history(&mut conv, &[]).await.unwrap();

    assert!(active.is_active(conv_id, "get_weather"));
    assert!(active.appended_for(conv_id).is_empty());
    assert!(
        active.version(conv_id) > before,
        "the next request re-renders"
    );
}
