use std::sync::Arc;

use async_trait::async_trait;
use openclaw_core::types::ToolSchema;
use openclaw_core::{Result, Tool};
use serde_json::{json, Value};

use super::session_manager::SessionManager;

/// Maximum recursion depth for nested agent spawning (H7).
const MAX_RECURSION_DEPTH: u64 = 5;

/// A tool that runs a task using a specific sub-agent.
pub struct SubagentsTool {
    manager: Arc<dyn SessionManager>,
}

impl SubagentsTool {
    pub fn new(manager: Arc<dyn SessionManager>) -> Self {
        Self { manager }
    }
}

#[async_trait]
impl Tool for SubagentsTool {
    fn name(&self) -> &str {
        "subagents"
    }

    fn description(&self) -> &str {
        "Run a task using a specific sub-agent."
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: self.name().to_string(),
            description: self.description().to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "agent_id": {
                        "type": "string",
                        "description": "The ID of the agent to run"
                    },
                    "task": {
                        "type": "string",
                        "description": "The task to execute"
                    }
                },
                "required": ["agent_id", "task"]
            }),
        }
    }

    async fn execute(&self, args: Value) -> Result<Value> {
        let agent_id = args["agent_id"]
            .as_str()
            .ok_or_else(|| openclaw_core::Error::ToolExecution("missing agent_id".into()))?;
        let task = args["task"]
            .as_str()
            .ok_or_else(|| openclaw_core::Error::ToolExecution("missing task".into()))?;

        // H7: Check current recursion depth before spawning
        let current_depth = args["_depth"].as_u64().unwrap_or(0);
        if current_depth >= MAX_RECURSION_DEPTH {
            return Err(openclaw_core::Error::ToolExecution(format!(
                "maximum agent recursion depth ({MAX_RECURSION_DEPTH}) exceeded"
            ).into()));
        }

        self.manager.run_subagent(agent_id, task).await
    }
}
