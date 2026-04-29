use async_trait::async_trait;
use rustykrab_core::types::ToolSchema;
use rustykrab_core::{Result, Tool};
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
                        "description": "The channel to send the message to (e.g. \"telegram\", \"slack\", \"signal\", \"webchat\")"
                    },
                    "text": {
                        "type": "string",
                        "description": "The message text to send"
                    },
                    "chat_id": {
                        "type": "string",
                        "description": "Channel-specific chat identifier (Telegram chat id, Slack channel id, Signal phone number)"
                    },
                    "thread_id": {
                        "type": "string",
                        "description": "Channel-specific thread identifier. Telegram: forum topic thread_id (numeric string). Slack: thread_ts (e.g. \"1700000000.000100\"). Omit to post at the channel's top level."
                    }
                },
                "required": ["channel", "text"]
            }),
        }
    }

    async fn execute(&self, args: Value) -> Result<Value> {
        let channel = args["channel"]
            .as_str()
            .ok_or_else(|| rustykrab_core::Error::ToolExecution("missing channel".into()))?;

        let text = args["text"]
            .as_str()
            .ok_or_else(|| rustykrab_core::Error::ToolExecution("missing text".into()))?;

        let chat_id = args["chat_id"].as_str();
        let thread_id = args["thread_id"].as_str();

        self.backend
            .send_message(channel, text, chat_id, thread_id)
            .await
            .map_err(|e| rustykrab_core::Error::ToolExecution(e.to_string().into()))?;

        Ok(json!({
            "sent": true,
            "channel": channel,
        }))
    }
}
