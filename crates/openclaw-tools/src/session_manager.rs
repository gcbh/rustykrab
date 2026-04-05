use async_trait::async_trait;
use openclaw_core::Result;
use serde_json::Value;

/// Trait for session management operations, implemented by the gateway/runtime.
#[async_trait]
pub trait SessionManager: Send + Sync {
    async fn list_sessions(&self) -> Result<Value>;
    async fn get_session_history(&self, session_id: &str) -> Result<Value>;
    async fn send_to_session(&self, session_id: &str, message: &str) -> Result<Value>;
    async fn spawn_session(
        &self,
        system_prompt: Option<&str>,
        capabilities: Option<Vec<String>>,
    ) -> Result<Value>;
    async fn yield_session(&self, result: &str) -> Result<Value>;
    async fn get_session_status(&self, session_id: &str) -> Result<Value>;
    async fn list_agents(&self) -> Result<Value>;
    async fn run_subagent(&self, agent_id: &str, task: &str) -> Result<Value>;
}
