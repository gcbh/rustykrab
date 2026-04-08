use std::sync::Arc;

use async_trait::async_trait;
use rustykrab_core::types::ToolSchema;
use rustykrab_core::{Result, Tool};
use serde_json::{json, Value};

use super::session_manager::SessionManager;

/// A tool that yields control and returns a result to the parent session.
pub struct SessionsYieldTool {
    manager: Arc<dyn SessionManager>,
}

impl SessionsYieldTool {
    pub fn new(manager: Arc<dyn SessionManager>) -> Self {
        Self { manager }
    }
}

#[async_trait]
impl Tool for SessionsYieldTool {
    fn name(&self) -> &str {
        "sessions_yield"
    }

    fn description(&self) -> &str {
        "Yield control and return a result to the parent session."
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: self.name().to_string(),
            description: self.description().to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "result": {
                        "type": "string",
                        "description": "The result to return to the parent session"
                    }
                },
                "required": ["result"]
            }),
        }
    }

    async fn execute(&self, args: Value) -> Result<Value> {
        let result = args["result"]
            .as_str()
            .ok_or_else(|| rustykrab_core::Error::ToolExecution("missing result".into()))?;

        self.manager.yield_session(result).await
    }
}
