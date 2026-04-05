use async_trait::async_trait;
use openclaw_core::types::ToolSchema;
use openclaw_core::{Result, Tool};
use serde_json::{json, Value};
use std::sync::Arc;

use crate::message_backend::MessageBackend;

/// A tool that sends a message to a specified channel.
pub struct MessageTool {
    backend: Arc<dyn MessageBackend>,
}

impl MessageTool {
    pub fn new(backend: Arc<dyn MessageBackend>) -> Self {
        Self { backend }
    }
}

#[async_trait]
impl Tool for MessageTool {
    fn name(&self) -> &str {
        "message"
    }

    fn description(&self) -> &str {
        "Send a message to a specified channel."
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: self.name().to_string(),
            description: self.description().to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "channel": {
                        "type": "string",
                        "description": "The channel to send the message to (e.g. \"telegram\", \"signal\", \"webchat\")"
                    },
                    "text": {
                        "type": "string",
                        "description": "The message text to send"
                    },
                    "chat_id": {
                        "type": "string",
                        "description": "Optional chat identifier"
                    }
                },
                "required": ["channel", "text"]
            }),
        }
    }

    async fn execute(&self, args: Value) -> Result<Value> {
        let channel = args["channel"]
            .as_str()
            .ok_or_else(|| openclaw_core::Error::ToolExecution("missing channel".into()))?;

        let text = args["text"]
            .as_str()
            .ok_or_else(|| openclaw_core::Error::ToolExecution("missing text".into()))?;

        let chat_id = args["chat_id"].as_str();

        self.backend
            .send_message(channel, text, chat_id)
            .await
            .map_err(|e| openclaw_core::Error::ToolExecution(e.to_string()))?;

        Ok(json!({
            "sent": true,
            "channel": channel,
        }))
    }
}
