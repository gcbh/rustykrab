//! Sub-agent delegation runtime.
//!
//! [`SubagentRunner`] implements [`rustykrab_tools::SessionManager`] and
//! is wired up via `rustykrab_tools::session_tools(...)`. The model calls
//! the `agents_list` and `subagents` tools; this struct turns those calls
//! into a fresh nested [`AgentRunner`] invocation against a
//! [`AgentDefinition`] from the registry.
//!
//! The full multi-session API (`sessions_spawn` / `sessions_send` /
//! `sessions_yield` / etc.) is intentionally stubbed — those tools are
//! a separate feature and will be wired up in a follow-up.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use rustykrab_core::active_tools::SESSION_TOOL_CONTEXT;
use rustykrab_core::model::ModelProvider;
use rustykrab_core::types::{Conversation, Message, MessageContent, Role};
use rustykrab_core::{AgentRegistry, CapabilitySet, Error, Result, Session, Tool};
use rustykrab_tools::SessionManager;
use serde_json::{json, Value};
use tokio::sync::Semaphore;
use uuid::Uuid;

use crate::harness::HarnessProfile;
use crate::runner::AgentRunner;
use crate::sandbox::Sandbox;

/// Runs sub-agents against an [`AgentRegistry`] using a fresh
/// [`AgentRunner`] per call.
///
/// A semaphore serialises concurrent calls so that a model emitting
/// parallel `subagents` tool calls in one turn does not stack multiple
/// nested loops on top of a local Ollama provider. The default permit
/// count is taken from `OrchestrationConfig::max_concurrent_tasks`.
pub struct SubagentRunner {
    provider: Arc<dyn ModelProvider>,
    tools: Vec<Arc<dyn Tool>>,
    sandbox: Arc<dyn Sandbox>,
    registry: Arc<AgentRegistry>,
    concurrency: Arc<Semaphore>,
}

impl SubagentRunner {
    pub fn new(
        provider: Arc<dyn ModelProvider>,
        tools: Vec<Arc<dyn Tool>>,
        sandbox: Arc<dyn Sandbox>,
        registry: Arc<AgentRegistry>,
        concurrency: usize,
    ) -> Self {
        let permits = concurrency.max(1);
        Self {
            provider,
            tools,
            sandbox,
            registry,
            concurrency: Arc::new(Semaphore::new(permits)),
        }
    }

    /// Build a session whose capabilities are the intersection of the
    /// parent session's capabilities and the agent definition's
    /// `allowed_tools` list. If `allowed_tools` is `None`, the parent's
    /// capabilities pass through unchanged.
    fn derive_capabilities(&self, allowed_tools: Option<&[String]>) -> CapabilitySet {
        let parent = SESSION_TOOL_CONTEXT
            .try_with(|ctx| (*ctx.capabilities).clone())
            .ok();

        match (parent, allowed_tools) {
            (Some(parent_caps), Some(allowed)) => {
                let names: Vec<&str> = allowed
                    .iter()
                    .filter(|name| parent_caps.can_use_tool(name))
                    .map(|s| s.as_str())
                    .collect();
                CapabilitySet::for_tools(&names)
            }
            (Some(parent_caps), None) => parent_caps,
            (None, Some(allowed)) => {
                let names: Vec<&str> = allowed.iter().map(|s| s.as_str()).collect();
                CapabilitySet::for_tools(&names)
            }
            (None, None) => CapabilitySet::default_safe(),
        }
    }
}

#[async_trait]
impl SessionManager for SubagentRunner {
    async fn list_sessions(&self) -> Result<Value> {
        Err(Error::ToolExecution(
            "sessions_list is not implemented yet".into(),
        ))
    }

    async fn get_session_history(&self, _session_id: &str) -> Result<Value> {
        Err(Error::ToolExecution(
            "sessions_history is not implemented yet".into(),
        ))
    }

    async fn send_to_session(&self, _session_id: &str, _message: &str) -> Result<Value> {
        Err(Error::ToolExecution(
            "sessions_send is not implemented yet".into(),
        ))
    }

    async fn spawn_session(
        &self,
        _system_prompt: Option<&str>,
        _capabilities: Option<Vec<String>>,
    ) -> Result<Value> {
        Err(Error::ToolExecution(
            "sessions_spawn is not implemented yet".into(),
        ))
    }

    async fn yield_session(&self, _result: &str) -> Result<Value> {
        Err(Error::ToolExecution(
            "sessions_yield is not implemented yet".into(),
        ))
    }

    async fn get_session_status(&self, _session_id: &str) -> Result<Value> {
        Err(Error::ToolExecution(
            "session_status is not implemented yet".into(),
        ))
    }

    async fn list_agents(&self) -> Result<Value> {
        let defs = self.registry.list();
        let agents: Vec<Value> = defs
            .iter()
            .map(|d| {
                json!({
                    "id": d.id,
                    "description": d.description,
                    "profile": d.profile,
                    "allowed_tools": d.allowed_tools,
                })
            })
            .collect();
        Ok(json!({ "agents": agents }))
    }

    async fn run_subagent(&self, agent_id: &str, task: &str) -> Result<Value> {
        let def = self
            .registry
            .get(agent_id)
            .ok_or_else(|| Error::ToolExecution(format!("unknown agent_id: {agent_id}").into()))?;

        // Serialise nested runs so a parallel-tool-call burst from the
        // model doesn't fan out into multiple concurrent local-model
        // loops.
        let _permit = self
            .concurrency
            .acquire()
            .await
            .map_err(|_| Error::Internal("subagent semaphore closed".into()))?;

        let profile = match def.profile.as_str() {
            "coding" => HarnessProfile::coding(),
            "research" => HarnessProfile::research(),
            "creative" => HarnessProfile::creative(),
            _ => HarnessProfile::default(),
        };

        let caps = self.derive_capabilities(def.allowed_tools.as_deref());
        let conv_id = Uuid::new_v4();
        let session = Session::with_capabilities(conv_id, caps);

        let now = Utc::now();
        let mut conv = Conversation {
            id: conv_id,
            messages: vec![
                Message {
                    id: Uuid::new_v4(),
                    role: Role::System,
                    content: MessageContent::Text(def.system_prompt.clone()),
                    created_at: now,
                },
                Message {
                    id: Uuid::new_v4(),
                    role: Role::User,
                    content: MessageContent::Text(task.to_string()),
                    created_at: now,
                },
            ],
            created_at: now,
            updated_at: now,
            summary: None,
            detected_profile: Some(def.profile.clone()),
            channel_source: Some("subagent".into()),
            channel_id: Some(def.id.clone()),
            channel_thread_id: None,
        };

        let runner = AgentRunner::new(
            self.provider.clone(),
            self.tools.clone(),
            self.sandbox.clone(),
        )
        .with_config(profile.to_agent_config());

        runner.run(&mut conv, &session).await.map_err(|e| {
            Error::ToolExecution(format!("subagent '{}' failed: {e}", def.id).into())
        })?;

        let answer = conv
            .messages
            .iter()
            .rev()
            .find(|m| m.role == Role::Assistant && m.content.as_text().is_some())
            .and_then(|m| m.content.as_text())
            .unwrap_or("")
            .to_string();

        let iterations = conv
            .messages
            .iter()
            .filter(|m| m.role == Role::Assistant)
            .count();

        Ok(json!({
            "agent_id": def.id,
            "result": answer,
            "iterations": iterations,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use async_trait::async_trait;
    use rustykrab_core::model::{ModelResponse, StopReason, StreamEvent, Usage};
    use rustykrab_core::types::ToolSchema;
    use std::sync::Mutex;

    use crate::sandbox::NoSandbox;

    struct ScriptedProvider {
        responses: Mutex<Vec<ModelResponse>>,
    }

    impl ScriptedProvider {
        fn new(responses: Vec<ModelResponse>) -> Self {
            Self {
                responses: Mutex::new(responses),
            }
        }
    }

    #[async_trait]
    impl ModelProvider for ScriptedProvider {
        fn name(&self) -> &str {
            "scripted"
        }

        async fn chat(
            &self,
            _messages: &[Message],
            _tools: &[ToolSchema],
        ) -> Result<ModelResponse> {
            let mut q = self.responses.lock().unwrap();
            if q.is_empty() {
                Ok(text_response("(no more responses)"))
            } else {
                Ok(q.remove(0))
            }
        }

        async fn chat_stream(
            &self,
            messages: &[Message],
            tools: &[ToolSchema],
            _on_event: &(dyn Fn(StreamEvent) + Send + Sync),
        ) -> Result<ModelResponse> {
            self.chat(messages, tools).await
        }
    }

    fn text_response(text: &str) -> ModelResponse {
        ModelResponse {
            message: Message {
                id: Uuid::new_v4(),
                role: Role::Assistant,
                content: MessageContent::Text(text.to_string()),
                created_at: Utc::now(),
            },
            usage: Usage {
                prompt_tokens: 1,
                completion_tokens: 1,
                ..Default::default()
            },
            stop_reason: StopReason::EndTurn,
            text: Some(text.to_string()),
        }
    }

    fn make_runner(provider: Arc<dyn ModelProvider>) -> SubagentRunner {
        SubagentRunner::new(
            provider,
            Vec::new(),
            Arc::new(NoSandbox),
            Arc::new(AgentRegistry::with_defaults()),
            1,
        )
    }

    #[tokio::test]
    async fn list_agents_returns_registered_defs() {
        let provider = Arc::new(ScriptedProvider::new(vec![]));
        let runner = make_runner(provider);

        let v = runner.list_agents().await.unwrap();
        let agents = v.get("agents").and_then(|a| a.as_array()).unwrap();
        let ids: Vec<&str> = agents
            .iter()
            .filter_map(|a| a.get("id").and_then(|i| i.as_str()))
            .collect();
        assert_eq!(ids, vec!["coder", "planner", "researcher"]);
    }

    #[tokio::test]
    async fn run_subagent_unknown_id_errors() {
        let provider = Arc::new(ScriptedProvider::new(vec![]));
        let runner = make_runner(provider);

        let err = runner.run_subagent("nope", "do a thing").await.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("unknown agent_id"), "got: {msg}");
    }

    #[tokio::test]
    async fn run_subagent_returns_assistant_text() {
        let provider = Arc::new(ScriptedProvider::new(vec![text_response("done")]));
        let runner = make_runner(provider);

        let v = runner
            .run_subagent("planner", "plan something")
            .await
            .unwrap();
        assert_eq!(v.get("agent_id").and_then(|i| i.as_str()), Some("planner"));
        assert_eq!(v.get("result").and_then(|r| r.as_str()), Some("done"));
    }

    #[tokio::test]
    async fn sessions_apis_return_not_implemented() {
        let provider = Arc::new(ScriptedProvider::new(vec![]));
        let runner = make_runner(provider);

        assert!(runner.list_sessions().await.is_err());
        assert!(runner.spawn_session(None, None).await.is_err());
        assert!(runner.yield_session("x").await.is_err());
        assert!(runner.get_session_status("x").await.is_err());
        assert!(runner.send_to_session("x", "y").await.is_err());
        assert!(runner.get_session_history("x").await.is_err());
    }
}
