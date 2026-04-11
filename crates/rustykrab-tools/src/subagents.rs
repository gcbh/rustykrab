use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use rustykrab_core::types::ToolSchema;
use rustykrab_core::{Result, Tool};
use serde_json::{json, Value};

use super::session_manager::SessionManager;

/// Maximum recursion depth for nested agent spawning (H7).
const MAX_RECURSION_DEPTH: u64 = 5;

/// A tool that runs a task using a specific sub-agent.
///
/// Depth is tracked server-side via an atomic counter so clients cannot
/// bypass the fork-bomb guard by omitting or resetting the `_depth` arg.
pub struct SubagentsTool {
    manager: Arc<dyn SessionManager>,
    depth: Arc<AtomicU64>,
}

impl SubagentsTool {
    pub fn new(manager: Arc<dyn SessionManager>) -> Self {
        Self {
            manager,
            depth: Arc::new(AtomicU64::new(0)),
        }
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
            .ok_or_else(|| rustykrab_core::Error::ToolExecution("missing agent_id".into()))?;
        let task = args["task"]
            .as_str()
            .ok_or_else(|| rustykrab_core::Error::ToolExecution("missing task".into()))?;

        // H7: Server-side depth tracking — ignore client-provided _depth.
        let current_depth = self.depth.load(Ordering::Acquire);
        if current_depth >= MAX_RECURSION_DEPTH {
            return Err(rustykrab_core::Error::ToolExecution(
                format!("maximum agent recursion depth ({MAX_RECURSION_DEPTH}) exceeded").into(),
            ));
        }

        self.depth.fetch_add(1, Ordering::AcqRel);
        let result = self.manager.run_subagent(agent_id, task).await;
        self.depth.fetch_sub(1, Ordering::AcqRel);

        result
    }
}
