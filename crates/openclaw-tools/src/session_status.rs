use std::sync::Arc;

use async_trait::async_trait;
use openclaw_core::types::ToolSchema;
use openclaw_core::{Result, Tool};
use serde_json::{json, Value};

use super::session_manager::SessionManager;

/// A tool that gets the status of a session.
pub struct SessionStatusTool {
    manager: Arc<dyn SessionManager>,
}

impl SessionStatusTool {
    pub fn new(manager: Arc<dyn SessionManager>) -> Self {
        Self { manager }
    }
}

#[async_trait]
impl Tool for SessionStatusTool {
    fn name(&self) -> &str {
        "session_status"
    }

    fn description(&self) -> &str {
        "Get the status of a session."
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
                        "description": "The ID of the session to check. Defaults to the current session if not provided."
                    }
                },
                "required": []
            }),
        }
    }

    async fn execute(&self, args: Value) -> Result<Value> {
        let session_id = args["session_id"]
            .as_str()
            .unwrap_or("current");

        self.manager.get_session_status(session_id).await
    }
}
