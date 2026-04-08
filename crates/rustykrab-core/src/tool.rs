use async_trait::async_trait;
use serde_json::Value;

use crate::error::Result;
use crate::types::ToolSchema;

/// Trait implemented by every tool available to the agent.
#[async_trait]
pub trait Tool: Send + Sync {
    /// Unique name used to invoke this tool.
    fn name(&self) -> &str;

    /// Human-readable description shown to the model.
    fn description(&self) -> &str;

    /// JSON Schema describing the expected parameters.
    fn schema(&self) -> ToolSchema;

    /// Execute the tool with the given arguments, returning a JSON result.
    async fn execute(&self, args: Value) -> Result<Value>;
}
