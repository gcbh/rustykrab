use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// A single part of a multi-modal content payload.
///
/// Used at the event boundary (inbound events from channels) and in
/// conversation messages that contain non-text content. Messages can
/// contain multiple parts (e.g. an image with a caption).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ContentPart {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "image")]
    Image {
        media_type: String,
        #[serde(with = "base64_bytes")]
        data: Vec<u8>,
    },
    #[serde(rename = "audio")]
    Audio {
        media_type: String,
        #[serde(with = "base64_bytes")]
        data: Vec<u8>,
    },
    #[serde(rename = "file_ref")]
    FileRef { name: String, path: PathBuf },
}

/// Content block in the Anthropic multi-modal format.
/// Maps directly to what providers accept in their API.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ContentBlock {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "image")]
    Image {
        media_type: String,
        #[serde(with = "base64_bytes")]
        data: Vec<u8>,
    },
    #[serde(rename = "tool_use")]
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    #[serde(rename = "tool_result")]
    ToolResult {
        tool_use_id: String,
        content: String,
        #[serde(default, skip_serializing_if = "is_false")]
        is_error: bool,
    },
}

fn is_false(v: &bool) -> bool {
    !v
}

mod base64_bytes {
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(data: &[u8], ser: S) -> Result<S::Ok, S::Error> {
        ser.serialize_str(&STANDARD.encode(data))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(de: D) -> Result<Vec<u8>, D::Error> {
        let s = String::deserialize(de)?;
        STANDARD.decode(&s).map_err(serde::de::Error::custom)
    }
}

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

/// Message content with migration support.
///
/// Current format uses `#[serde(tag = "type", content = "data")]`.
/// Previous format used `#[serde(untagged)]`. The custom Deserialize
/// implementation tries the current format first, then falls back to
/// the old untagged format so persisted conversations still load.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", content = "data")]
pub enum MessageContent {
    #[serde(rename = "text")]
    Text(String),
    #[serde(rename = "tool_call")]
    ToolCall(ToolCall),
    #[serde(rename = "tool_result")]
    ToolResult(ToolResult),
    /// Multiple tool calls in a single assistant turn.
    /// Enables parallel tool execution — the model can request
    /// several tools at once and receive all results before continuing.
    #[serde(rename = "multi_tool_call")]
    MultiToolCall(Vec<ToolCall>),
    /// Multi-modal content (text + images, etc.).
    /// Produced when channels deliver non-text content, or when the
    /// event loop maps `ContentPart`s to provider-facing blocks.
    #[serde(rename = "multi_part")]
    MultiPart(Vec<ContentBlock>),
}

impl<'de> Deserialize<'de> for MessageContent {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        // Buffer the raw value so we can try multiple formats.
        let raw = serde_json::Value::deserialize(deserializer)?;

        // Try current tagged format first.
        #[derive(Deserialize)]
        #[serde(tag = "type", content = "data")]
        enum Tagged {
            #[serde(rename = "text")]
            Text(String),
            #[serde(rename = "tool_call")]
            ToolCall(ToolCall),
            #[serde(rename = "tool_result")]
            ToolResult(ToolResult),
            #[serde(rename = "multi_tool_call")]
            MultiToolCall(Vec<ToolCall>),
            #[serde(rename = "multi_part")]
            MultiPart(Vec<ContentBlock>),
        }

        if let Ok(tagged) = serde_json::from_value::<Tagged>(raw.clone()) {
            return Ok(match tagged {
                Tagged::Text(s) => MessageContent::Text(s),
                Tagged::ToolCall(tc) => MessageContent::ToolCall(tc),
                Tagged::ToolResult(tr) => MessageContent::ToolResult(tr),
                Tagged::MultiToolCall(tcs) => MessageContent::MultiToolCall(tcs),
                Tagged::MultiPart(blocks) => MessageContent::MultiPart(blocks),
            });
        }

        // Fall back to old untagged format for migration.
        // In the untagged format, a plain string is Text, an object with
        // "name" + "arguments" is ToolCall, an object with "call_id" + "output"
        // is ToolResult, and an array of tool calls is MultiToolCall.
        if let Some(s) = raw.as_str() {
            return Ok(MessageContent::Text(s.to_string()));
        }
        if let Ok(tcs) = serde_json::from_value::<Vec<ToolCall>>(raw.clone()) {
            if !tcs.is_empty() {
                return Ok(MessageContent::MultiToolCall(tcs));
            }
        }
        if let Ok(tr) = serde_json::from_value::<ToolResult>(raw.clone()) {
            return Ok(MessageContent::ToolResult(tr));
        }
        if let Ok(tc) = serde_json::from_value::<ToolCall>(raw.clone()) {
            return Ok(MessageContent::ToolCall(tc));
        }

        Err(serde::de::Error::custom(
            "failed to deserialize MessageContent in both tagged and untagged formats",
        ))
    }
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
            MessageContent::MultiPart(blocks) => {
                // Return the first text block, if any.
                blocks.iter().find_map(|b| match b {
                    ContentBlock::Text { text } => Some(text.as_str()),
                    _ => None,
                })
            }
            _ => None,
        }
    }

    /// Build `MultiPart` content from a set of `ContentPart`s, using
    /// provider capabilities to decide what to include.
    pub fn from_parts(parts: &[ContentPart], supports_vision: bool) -> Self {
        let mut blocks = Vec::new();
        for part in parts {
            match part {
                ContentPart::Text { text } => {
                    blocks.push(ContentBlock::Text { text: text.clone() });
                }
                ContentPart::Image { media_type, data } if supports_vision => {
                    blocks.push(ContentBlock::Image {
                        media_type: media_type.clone(),
                        data: data.clone(),
                    });
                }
                ContentPart::Image { .. } => {
                    blocks.push(ContentBlock::Text {
                        text:
                            "[User sent an image, but the current model does not support vision.]"
                                .to_string(),
                    });
                }
                ContentPart::Audio { .. } => {
                    blocks.push(ContentBlock::Text {
                        text: "[User sent an audio message. Audio transcription is not yet supported.]"
                            .to_string(),
                    });
                }
                ContentPart::FileRef { name, .. } => {
                    blocks.push(ContentBlock::Text {
                        text: format!("[User attached file: {name}]"),
                    });
                }
            }
        }

        if blocks.len() == 1 {
            if let ContentBlock::Text { text } = blocks.remove(0) {
                return MessageContent::Text(text);
            }
        }

        MessageContent::MultiPart(blocks)
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
    /// Whether the tool execution failed. Sent as `is_error` to providers
    /// so the model knows to interpret the output as an error message.
    #[serde(default)]
    pub is_error: bool,
    /// Images produced by the tool (e.g. screenshots). Surfaced to
    /// vision-capable models as image content blocks alongside the textual
    /// output; ignored by providers/models without vision. Empty for the
    /// vast majority of tools.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<ContentBlock>,
}

/// Reserved JSON key a tool may set on its (object) output to attach binary
/// images — e.g. a screenshot — to its result. The agent runner extracts
/// these into [`ToolResult::images`] and strips the key, so the raw base64
/// never reaches the model as text.
pub const TOOL_RESULT_IMAGES_KEY: &str = "_images";

/// Split any [`TOOL_RESULT_IMAGES_KEY`] array out of a tool's JSON output.
///
/// Each entry must be an object with a `media_type` string and a base64
/// `data` string; malformed entries are skipped. The key is always removed
/// from the returned output so large base64 blobs never leak into the text
/// the model reads. Returns the cleaned output and the parsed image blocks.
pub fn split_tool_result_images(
    mut output: serde_json::Value,
) -> (serde_json::Value, Vec<ContentBlock>) {
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine;

    let Some(obj) = output.as_object_mut() else {
        return (output, Vec::new());
    };
    let Some(serde_json::Value::Array(entries)) = obj.remove(TOOL_RESULT_IMAGES_KEY) else {
        return (output, Vec::new());
    };

    let mut images = Vec::new();
    for entry in entries {
        let media_type = entry.get("media_type").and_then(|v| v.as_str());
        let data = entry.get("data").and_then(|v| v.as_str());
        if let (Some(media_type), Some(data)) = (media_type, data) {
            if let Ok(bytes) = STANDARD.decode(data) {
                images.push(ContentBlock::Image {
                    media_type: media_type.to_string(),
                    data: bytes,
                });
            }
        }
    }
    (output, images)
}

/// A conversation is an ordered sequence of messages.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Conversation {
    pub id: Uuid,
    pub messages: Vec<Message>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// Optional human-readable title. Set by external clients (e.g. Apollo)
    /// at create time; otherwise `None`.
    #[serde(default)]
    pub title: Option<String>,
    /// Optional summary of earlier messages for context compression.
    #[serde(default)]
    pub summary: Option<String>,
    /// Self-classified profile from the model's latest response.
    /// Updated every turn — the model always tags its response.
    #[serde(default)]
    pub detected_profile: Option<String>,
    /// Which channel created this conversation (e.g. "telegram", "signal", "web").
    #[serde(default)]
    pub channel_source: Option<String>,
    /// Channel-specific identifier (e.g. Telegram chat_id as string).
    #[serde(default)]
    pub channel_id: Option<String>,
    /// Channel-specific thread/topic identifier (e.g. Telegram forum thread_id).
    #[serde(default)]
    pub channel_thread_id: Option<String>,
}

/// JSON-Schema-style description of a tool parameter.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSchema {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine;

    #[test]
    fn split_tool_result_images_extracts_and_strips_key() {
        let png = vec![0x89u8, 0x50, 0x4e, 0x47];
        let output = serde_json::json!({
            "text": "captured",
            "_images": [
                { "media_type": "image/png", "data": STANDARD.encode(&png) }
            ]
        });

        let (cleaned, images) = split_tool_result_images(output);
        // The reserved key is removed so base64 never leaks into model text.
        assert!(cleaned.get(TOOL_RESULT_IMAGES_KEY).is_none());
        assert_eq!(cleaned["text"], "captured");
        assert_eq!(images.len(), 1);
        match &images[0] {
            ContentBlock::Image { media_type, data } => {
                assert_eq!(media_type, "image/png");
                assert_eq!(data, &png);
            }
            other => panic!("expected image block, got {other:?}"),
        }
    }

    #[test]
    fn split_tool_result_images_skips_malformed_entries_but_strips_key() {
        let output = serde_json::json!({
            "_images": [
                { "media_type": "image/png" },                 // missing data
                { "data": "not-base64-$$$" },                   // missing media_type + bad b64
                "garbage"                                        // not an object
            ]
        });
        let (cleaned, images) = split_tool_result_images(output);
        assert!(cleaned.get(TOOL_RESULT_IMAGES_KEY).is_none());
        assert!(images.is_empty());
    }

    #[test]
    fn split_tool_result_images_passes_through_non_object_and_missing_key() {
        let (cleaned, images) = split_tool_result_images(serde_json::json!("plain string"));
        assert_eq!(cleaned, serde_json::json!("plain string"));
        assert!(images.is_empty());

        let (cleaned, images) = split_tool_result_images(serde_json::json!({ "ok": true }));
        assert_eq!(cleaned, serde_json::json!({ "ok": true }));
        assert!(images.is_empty());
    }
}
