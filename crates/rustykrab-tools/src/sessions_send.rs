use std::sync::Arc;

use async_trait::async_trait;
use rustykrab_core::types::ToolSchema;
use rustykrab_core::{Result, Tool};
use serde_json::{json, Value};

use super::session_manager::SessionManager;

/// A tool that sends a message to another session.
pub struct SessionsSendTool {
    manager: Arc<dyn SessionManager>,
}

impl SessionsSendTool {
    pub fn new(manager: Arc<dyn SessionManager>) -> Self {
        Self { manager }
    }
}

#[async_trait]
impl Tool for SessionsSendTool {
    fn name(&self) -> &str {
        "sessions_send"
    }

    fn description(&self) -> &str {
        "Send a message to another session."
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
                        "description": "The ID of the session to send the message to"
                    },
                    "message": {
                        "type": "string",
                        "description": "The message to send"
                    }
                },
                "required": ["session_id", "message"]
            }),
        }
    }

    async fn execute(&self, args: Value) -> Result<Value> {
        let session_id = args["session_id"]
            .as_str()
            .ok_or_else(|| rustykrab_core::Error::ToolExecution("missing session_id".into()))?;
        let message = args["message"]
            .as_str()
            .ok_or_else(|| rustykrab_core::Error::ToolExecution("missing message".into()))?;

        self.manager.send_to_session(session_id, message).await
    }
}
