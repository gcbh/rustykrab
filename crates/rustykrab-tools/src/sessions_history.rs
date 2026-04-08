use std::sync::Arc;

use async_trait::async_trait;
use rustykrab_core::types::ToolSchema;
use rustykrab_core::{Result, Tool};
use serde_json::{json, Value};

use super::session_manager::SessionManager;

/// A tool that retrieves the message history for a session.
pub struct SessionsHistoryTool {
    manager: Arc<dyn SessionManager>,
}

impl SessionsHistoryTool {
    pub fn new(manager: Arc<dyn SessionManager>) -> Self {
        Self { manager }
    }
}

#[async_trait]
impl Tool for SessionsHistoryTool {
    fn name(&self) -> &str {
        "sessions_history"
    }

    fn description(&self) -> &str {
        "Get the message history for a session."
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: self.name().to_string(),
            description: self.description().to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "session_id": {
                        "type": "string",
                        "description": "The ID of the session to retrieve history for"
                    }
                },
                "required": ["session_id"]
            }),
        }
    }

    async fn execute(&self, args: Value) -> Result<Value> {
        let session_id = args["session_id"]
            .as_str()
            .ok_or_else(|| rustykrab_core::Error::ToolExecution("missing session_id".into()))?;

        self.manager.get_session_history(session_id).await
    }
}
