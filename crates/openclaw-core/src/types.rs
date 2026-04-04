use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// A single message in a conversation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub id: Uuid,
    pub role: Role,
    pub content: MessageContent,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MessageContent {
    Text(String),
    ToolCall(ToolCall),
    ToolResult(ToolResult),
    /// Multiple tool calls in a single assistant turn.
    /// Enables parallel tool execution — the model can request
    /// several tools at once and receive all results before continuing.
    MultiToolCall(Vec<ToolCall>),
}

impl MessageContent {
    /// Extract all tool calls from this content (single or multi).
    pub fn tool_calls(&self) -> Vec<&ToolCall> {
        match self {
            MessageContent::ToolCall(tc) => vec![tc],
            MessageContent::MultiToolCall(tcs) => tcs.iter().collect(),
            _ => vec![],
        }
    }

    /// Check if this content contains any tool calls.
    pub fn has_tool_calls(&self) -> bool {
        matches!(
            self,
            MessageContent::ToolCall(_) | MessageContent::MultiToolCall(_)
        )
    }

    /// Extract text content if present.
    pub fn as_text(&self) -> Option<&str> {
        match self {
            MessageContent::Text(t) => Some(t),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResult {
    pub call_id: String,
    pub output: serde_json::Value,
}

/// A conversation is an ordered sequence of messages.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Conversation {
    pub id: Uuid,
    pub messages: Vec<Message>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// Optional summary of earlier messages for context compression.
    #[serde(default)]
    pub summary: Option<String>,
}

/// JSON-Schema-style description of a tool parameter.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSchema {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}
