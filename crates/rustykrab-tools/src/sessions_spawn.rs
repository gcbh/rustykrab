use std::sync::Arc;

use async_trait::async_trait;
use rustykrab_core::types::ToolSchema;
use rustykrab_core::{Result, Tool};
use serde_json::{json, Value};

use super::session_manager::SessionManager;

/// Maximum depth for nested session spawning (H7).
const MAX_SPAWN_DEPTH: u64 = 5;

/// A tool that spawns a new sub-session with an optional system prompt.
pub struct SessionsSpawnTool {
    manager: Arc<dyn SessionManager>,
}

impl SessionsSpawnTool {
    pub fn new(manager: Arc<dyn SessionManager>) -> Self {
        Self { manager }
    }
}

#[async_trait]
impl Tool for SessionsSpawnTool {
    fn name(&self) -> &str {
        "sessions_spawn"
    }

    fn description(&self) -> &str {
        "Spawn a new sub-session with an optional system prompt."
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: self.name().to_string(),
            description: self.description().to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "system_prompt": {
                        "type": "string",
                        "description": "Optional system prompt for the new session"
                    },
                    "capabilities": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Optional list of capabilities to enable for the new session"
                    }
                },
                "required": []
            }),
        }
    }

    async fn execute(&self, args: Value) -> Result<Value> {
        // H7: Check current recursion depth before spawning
        let current_depth = args["_depth"].as_u64().unwrap_or(0);
        if current_depth >= MAX_SPAWN_DEPTH {
            return Err(rustykrab_core::Error::ToolExecution(format!(
                "maximum session spawn depth ({MAX_SPAWN_DEPTH}) exceeded"
            ).into()));
        }

        let system_prompt = args["system_prompt"].as_str();
        let capabilities = args["capabilities"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect::<Vec<String>>()
            });

        self.manager.spawn_session(system_prompt, capabilities).await
    }
}
