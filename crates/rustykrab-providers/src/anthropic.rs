use async_trait::async_trait;
use chrono::Utc;
use rustykrab_core::error::Result;
use rustykrab_core::model::{ModelProvider, ModelResponse, StopReason, Usage};
use rustykrab_core::types::{
    Message, MessageContent, Role, ToolCall, ToolSchema,
};
use rustykrab_core::Error;
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use std::time::Duration;
use uuid::Uuid;

/// Maximum number of retries for transient errors (429, 5xx).
const MAX_RETRIES: u32 = 3;
/// Base delay for exponential backoff (doubles each retry, with jitter).
const RETRY_BASE_DELAY: Duration = Duration::from_secs(1);

/// Anthropic Claude API provider.
///
/// Calls the Messages API at `https://api.anthropic.com/v1/messages`.
/// Supports tool use natively — Claude is the recommended model for
/// agentic workloads due to superior prompt injection resistance.
pub struct AnthropicProvider {
    client: reqwest::Client,
    api_key: SecretString,
    model: String,
    max_tokens: u32,
}

impl AnthropicProvider {
    pub fn new(api_key: String) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(300))
            .connect_timeout(Duration::from_secs(10))
            .build()
            .expect("failed to build HTTP client");
        Self {
            client,
            api_key: SecretString::from(api_key),
            model: "claude-sonnet-4-20250514".to_string(),
            max_tokens: 4096,
        }
    }

    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = model.into();
        self
    }

    pub fn with_max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = max_tokens;
        self
    }

    /// Convert our internal messages to Anthropic API format.
    fn build_messages(messages: &[Message]) -> (Option<String>, Vec<ApiMessage>) {
        let mut system_prompt = None;
        let mut api_messages = Vec::new();

        for msg in messages {
            match msg.role {
                Role::System => {
                    if let MessageContent::Text(ref text) = msg.content {
                        system_prompt = Some(text.clone());
                    }
                }
                Role::User => {
                    if let MessageContent::Text(ref text) = msg.content {
                        api_messages.push(ApiMessage {
                            role: "user".to_string(),
                            content: ApiContent::Text(text.clone()),
                        });
                    }
                }
                Role::Assistant => match msg.content {
                    MessageContent::Text(ref text) => {
                        api_messages.push(ApiMessage {
                            role: "assistant".to_string(),
                            content: ApiContent::Text(text.clone()),
                        });
                    }
                    MessageContent::ToolCall(ref call) => {
                        api_messages.push(ApiMessage {
                            role: "assistant".to_string(),
                            content: ApiContent::Blocks(vec![ContentBlock::ToolUse {
                                id: call.id.clone(),
                                name: call.name.clone(),
                                input: call.arguments.clone(),
                            }]),
                        });
                    }
                    MessageContent::MultiToolCall(ref calls) => {
                        let blocks = calls
                            .iter()
                            .map(|c| ContentBlock::ToolUse {
                                id: c.id.clone(),
                                name: c.name.clone(),
                                input: c.arguments.clone(),
                            })
                            .collect();
                        api_messages.push(ApiMessage {
                            role: "assistant".to_string(),
                            content: ApiContent::Blocks(blocks),
                        });
                    }
                    _ => {}
                },
                Role::Tool => {
                    if let MessageContent::ToolResult(ref result) = msg.content {
                        api_messages.push(ApiMessage {
                            role: "user".to_string(),
                            content: ApiContent::Blocks(vec![ContentBlock::ToolResult {
                                tool_use_id: result.call_id.clone(),
                                content: serde_json::to_string(&result.output)
                                    .unwrap_or_default(),
                            }]),
                        });
                    }
                }
            }
        }

        (system_prompt, api_messages)
    }

    /// Convert tool schemas to Anthropic's tool format.
    fn build_tools(tools: &[ToolSchema]) -> Vec<ApiTool> {
        tools
            .iter()
            .map(|t| ApiTool {
                name: t.name.clone(),
                description: t.description.clone(),
                input_schema: t.parameters.clone(),
            })
            .collect()
    }

    /// Parse the API response into our internal types.
    /// Supports multiple tool calls in a single response (parallel tool use).
    fn parse_response(resp: ApiResponse) -> Result<ModelResponse> {
        let usage = Usage {
            prompt_tokens: resp.usage.input_tokens,
            completion_tokens: resp.usage.output_tokens,
        };

        let stop_reason = match resp.stop_reason.as_deref() {
            Some("tool_use") => StopReason::ToolUse,
            Some("max_tokens") => StopReason::MaxTokens,
            _ => StopReason::EndTurn,
        };

        // Collect all tool use blocks.
        let tool_calls: Vec<ToolCall> = resp
            .content
            .iter()
            .filter_map(|block| match block {
                ResponseBlock::ToolUse { id, name, input } => Some(ToolCall {
                    id: id.clone(),
                    name: name.clone(),
                    arguments: input.clone(),
                }),
                _ => None,
            })
            .collect();

        // If there are tool calls, return them (single or multi).
        if !tool_calls.is_empty() {
            let content = if tool_calls.len() == 1 {
                MessageContent::ToolCall(tool_calls.into_iter().next().unwrap())
            } else {
                MessageContent::MultiToolCall(tool_calls)
            };

            return Ok(ModelResponse {
                message: Message {
                    id: Uuid::new_v4(),
                    role: Role::Assistant,
                    content,
                    created_at: Utc::now(),
                },
                usage,
                stop_reason,
            });
        }

        // Otherwise, extract text.
        let text = resp
            .content
            .iter()
            .filter_map(|b| match b {
                ResponseBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("");

        Ok(ModelResponse {
            message: Message {
                id: Uuid::new_v4(),
                role: Role::Assistant,
                content: MessageContent::Text(text),
                created_at: Utc::now(),
            },
            usage,
            stop_reason,
        })
    }
}

#[async_trait]
impl ModelProvider for AnthropicProvider {
    fn name(&self) -> &str {
        "anthropic"
    }

    async fn chat(
        &self,
        messages: &[Message],
        tools: &[ToolSchema],
    ) -> Result<ModelResponse> {
        let (system, api_messages) = Self::build_messages(messages);
        let api_tools = Self::build_tools(tools);

        let mut body = serde_json::json!({
            "model": self.model,
            "max_tokens": self.max_tokens,
            "messages": api_messages,
        });

        if let Some(sys) = system {
            body["system"] = serde_json::json!(sys);
        }
        if !api_tools.is_empty() {
            body["tools"] = serde_json::to_value(&api_tools)
                .map_err(|e| Error::Serialization(e))?;
        }

        tracing::debug!(model = %self.model, "calling Anthropic Messages API");

        let mut last_err = None;
        for attempt in 0..=MAX_RETRIES {
            if attempt > 0 {
                let delay = RETRY_BASE_DELAY * 2u32.pow(attempt - 1);
                tracing::warn!(attempt, "retrying Anthropic API after {delay:?}");
                tokio::time::sleep(delay).await;
            }

            let resp = match self
                .client
                .post("https://api.anthropic.com/v1/messages")
                .header("x-api-key", self.api_key.expose_secret())
                .header("anthropic-version", "2023-06-01")
                .header("content-type", "application/json")
                .json(&body)
                .send()
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    last_err = Some(Error::ModelProvider(e.to_string()));
                    continue;
                }
            };

            let status = resp.status();
            if status.is_success() {
                let api_resp: ApiResponse = resp
                    .json()
                    .await
                    .map_err(|e| Error::ModelProvider(format!("failed to parse response: {e}")))?;
                return Self::parse_response(api_resp);
            }

            let error_body = resp.text().await.unwrap_or_default();
            let is_retryable = matches!(status.as_u16(), 429 | 500 | 502 | 503 | 529);
            last_err = Some(Error::ModelProvider(format!(
                "Anthropic API returned {status}: {error_body}"
            )));

            if !is_retryable {
                break;
            }
        }

        Err(last_err.unwrap_or_else(|| Error::ModelProvider("request failed".into())))
    }
}

// --- Anthropic API wire types (private) ---

#[derive(Serialize)]
struct ApiMessage {
    role: String,
    content: ApiContent,
}

#[derive(Serialize)]
#[serde(untagged)]
enum ApiContent {
    Text(String),
    Blocks(Vec<ContentBlock>),
}

#[derive(Serialize)]
#[serde(tag = "type")]
enum ContentBlock {
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
    },
}

#[derive(Serialize)]
struct ApiTool {
    name: String,
    description: String,
    input_schema: serde_json::Value,
}

#[derive(Deserialize)]
struct ApiResponse {
    content: Vec<ResponseBlock>,
    usage: ApiUsage,
    #[serde(default)]
    stop_reason: Option<String>,
}

#[derive(Deserialize)]
#[serde(tag = "type")]
enum ResponseBlock {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "tool_use")]
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
}

#[derive(Deserialize)]
struct ApiUsage {
    input_tokens: u32,
    output_tokens: u32,
}
