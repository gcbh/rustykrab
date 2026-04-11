use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use rustykrab_core::types::ToolSchema;
use rustykrab_core::{Result, Tool};
use serde_json::{json, Value};

use super::session_manager::SessionManager;

/// Maximum depth for nested session spawning (H7).
const MAX_SPAWN_DEPTH: u64 = 5;

/// A tool that spawns a new sub-session with an optional system prompt.
///
/// Depth is tracked server-side via an atomic counter so clients cannot
/// bypass the fork-bomb guard by omitting or resetting the `_depth` arg.
pub struct SessionsSpawnTool {
    manager: Arc<dyn SessionManager>,
    depth: Arc<AtomicU64>,
}

impl SessionsSpawnTool {
    pub fn new(manager: Arc<dyn SessionManager>) -> Self {
        Self {
            manager,
            depth: Arc::new(AtomicU64::new(0)),
        }
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
        // H7: Server-side depth tracking — ignore client-provided _depth.
        let current_depth = self.depth.load(Ordering::Acquire);
        if current_depth >= MAX_SPAWN_DEPTH {
            return Err(rustykrab_core::Error::ToolExecution(
                format!("maximum session spawn depth ({MAX_SPAWN_DEPTH}) exceeded").into(),
            ));
        }

        self.depth.fetch_add(1, Ordering::AcqRel);
        let result = async {
            let system_prompt = args["system_prompt"].as_str();
            let capabilities = args["capabilities"].as_array().map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect::<Vec<String>>()
            });

            self.manager
                .spawn_session(system_prompt, capabilities)
                .await
        }
        .await;
        self.depth.fetch_sub(1, Ordering::AcqRel);

        result
    }
}
