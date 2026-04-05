use std::sync::Arc;

use async_trait::async_trait;
use openclaw_core::types::ToolSchema;
use openclaw_core::{Result, Tool};
use serde_json::{json, Value};

use super::session_manager::SessionManager;

/// A tool that lists all available agents and their capabilities.
pub struct AgentsListTool {
    manager: Arc<dyn SessionManager>,
}

impl AgentsListTool {
    pub fn new(manager: Arc<dyn SessionManager>) -> Self {
        Self { manager }
    }
}

#[async_trait]
impl Tool for AgentsListTool {
    fn name(&self) -> &str {
        "agents_list"
    }

    fn description(&self) -> &str {
        "List all available agents and their capabilities."
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: self.name().to_string(),
            description: self.description().to_string(),
            parameters: json!({
                "type": "object",
                "properties": {},
                "required": []
            }),
        }
    }

    async fn execute(&self, _args: Value) -> Result<Value> {
        self.manager.list_agents().await
    }
}
