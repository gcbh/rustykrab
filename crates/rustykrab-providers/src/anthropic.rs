use async_trait::async_trait;
use chrono::Utc;
use rustykrab_core::error::Result;
use rustykrab_core::model::{ModelProvider, ModelResponse, StopReason, StreamEvent, Usage};
use rustykrab_core::types::{
    ContentBlock as CoreContentBlock, Message, MessageContent, Role, ToolCall, ToolSchema,
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
    /// Context window in tokens. Populated from `ANTHROPIC_CONTEXT_LENGTH`
    /// when set; otherwise falls back to Claude's standard 200k window.
    /// Unlike Ollama, Anthropic doesn't expose a per-model context length
    /// endpoint, so the env var is the operator's escape hatch (e.g. to
    /// force the 1M-token beta window on Claude 4/5 models).
    context_limit: usize,
}

/// Default context window for Claude models when no override is set.
/// All current production Claude models (3.5, 4, 4.5, 4.6, 4.7) ship with
/// at least a 200k-token context window.
const DEFAULT_ANTHROPIC_CONTEXT_TOKENS: usize = 200_000;

fn anthropic_context_limit_from_env() -> Option<usize> {
    std::env::var("ANTHROPIC_CONTEXT_LENGTH")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|&v| v > 0)
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
            context_limit: anthropic_context_limit_from_env()
                .unwrap_or(DEFAULT_ANTHROPIC_CONTEXT_TOKENS),
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

    /// Explicit context-window override. Takes precedence over
    /// `ANTHROPIC_CONTEXT_LENGTH` and the built-in default.
    pub fn with_context_limit(mut self, context_limit: usize) -> Self {
        self.context_limit = context_limit;
        self
    }

    /// Convert our internal messages to Anthropic API format.
    ///
    /// Returns an error if tool result serialization fails (#195).
    fn build_messages(messages: &[Message]) -> Result<(Option<String>, Vec<ApiMessage>)> {
        let mut system_prompt = None;
        let mut api_messages = Vec::new();

        for msg in messages {
            match msg.role {
                Role::System => {
                    if let MessageContent::Text(ref text) = msg.content {
                        system_prompt = Some(text.clone());
                    }
                }
                Role::User => match &msg.content {
                    MessageContent::Text(text) => {
                        api_messages.push(ApiMessage {
                            role: "user".to_string(),
                            content: ApiContent::Blocks(vec![ContentBlock::Text {
                                text: text.clone(),
                            }]),
                        });
                    }
                    MessageContent::MultiPart(blocks) => {
                        let api_blocks = blocks
                            .iter()
                            .filter_map(|b| match b {
                                CoreContentBlock::Text { text } => {
                                    Some(ContentBlock::Text { text: text.clone() })
                                }
                                CoreContentBlock::Image { media_type, data } => {
                                    use base64::engine::general_purpose::STANDARD;
                                    use base64::Engine;
                                    Some(ContentBlock::Image {
                                        source: ImageSource {
                                            source_type: "base64".to_string(),
                                            media_type: media_type.clone(),
                                            data: STANDARD.encode(data),
                                        },
                                    })
                                }
                                _ => None,
                            })
                            .collect::<Vec<_>>();
                        if !api_blocks.is_empty() {
                            api_messages.push(ApiMessage {
                                role: "user".to_string(),
                                content: ApiContent::Blocks(api_blocks),
                            });
                        }
                    }
                    _ => {}
                },
                Role::Assistant => match msg.content {
                    MessageContent::Text(ref text) => {
                        api_messages.push(ApiMessage {
                            role: "assistant".to_string(),
                            content: ApiContent::Blocks(vec![ContentBlock::Text {
                                text: text.clone(),
                            }]),
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
                        // Fix #182: avoid double-serialization of string values.
                        // Fix #195: propagate serialization errors instead of swallowing them.
                        let content = match &result.output {
                            serde_json::Value::String(s) => s.clone(),
                            other => serde_json::to_string(other).map_err(Error::Serialization)?,
                        };
                        api_messages.push(ApiMessage {
                            role: "user".to_string(),
                            content: ApiContent::Blocks(vec![ContentBlock::ToolResult {
                                tool_use_id: result.call_id.clone(),
                                content,
                                // Fix #207: pass is_error flag to model.
                                is_error: result.is_error,
                            }]),
                        });
                    }
                }
            }
        }

        Ok((system_prompt, api_messages))
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
    /// Fix #190: preserves text content alongside tool calls.
    fn parse_response(resp: &ApiResponse) -> Result<ModelResponse> {
        let usage = Usage {
            prompt_tokens: resp.usage.input_tokens,
            completion_tokens: resp.usage.output_tokens,
            // Fix #203: capture cache token fields from Anthropic response.
            cache_read_tokens: resp.usage.cache_read_input_tokens.unwrap_or(0),
            cache_creation_tokens: resp.usage.cache_creation_input_tokens.unwrap_or(0),
        };

        let stop_reason = match resp.stop_reason.as_deref() {
            Some("tool_use") => StopReason::ToolUse,
            Some("max_tokens") => StopReason::MaxTokens,
            Some("content_filter") => StopReason::ContentPolicy,
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

        // Fix #190: always extract text blocks, even when tool calls are present.
        let text: String = resp
            .content
            .iter()
            .filter_map(|b| match b {
                ResponseBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("");

        // If there are tool calls, return them (single or multi),
        // preserving any accompanying text in `response.text`.
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
                text: if text.is_empty() { None } else { Some(text) },
            });
        }

        Ok(ModelResponse {
            message: Message {
                id: Uuid::new_v4(),
                role: Role::Assistant,
                content: MessageContent::Text(text),
                created_at: Utc::now(),
            },
            usage,
            stop_reason,
            text: None,
        })
    }

    /// Map an HTTP status code to a specific error variant (#186).
    fn map_status_error(status: reqwest::StatusCode, body: &str) -> Error {
        match status.as_u16() {
            400 => Error::ModelBadRequest(format!("Anthropic API: {body}")),
            401 | 403 => Error::ModelAuthError(format!("Anthropic API: {body}")),
            429 => Error::ModelRateLimit(format!("Anthropic API: {body}")),
            529 => Error::ModelOverloaded(format!("Anthropic API: {body}")),
            _ => Error::ModelProvider(format!("Anthropic API returned {status}: {body}")),
        }
    }
}

#[async_trait]
impl ModelProvider for AnthropicProvider {
    fn name(&self) -> &str {
        "anthropic"
    }

    fn context_limit(&self) -> Option<usize> {
        Some(self.context_limit)
    }

    fn supports_vision(&self) -> bool {
        true
    }

    fn requires_paired_tool_results(&self) -> bool {
        true
    }

    async fn chat(&self, messages: &[Message], tools: &[ToolSchema]) -> Result<ModelResponse> {
        let (system, api_messages) = Self::build_messages(messages)?;

        // Fix #200: validate that the message list is not empty before calling the API.
        if api_messages.is_empty() {
            return Err(Error::ModelBadRequest(
                "cannot call Anthropic API with an empty message list".into(),
            ));
        }

        let api_tools = Self::build_tools(tools);

        let mut body = serde_json::json!({
            "model": self.model,
            "max_tokens": self.max_tokens,
            "messages": api_messages,
        });

        // Fix #178: use array format for system parameter to enable prompt caching.
        if let Some(sys) = system {
            body["system"] = serde_json::json!([{"type": "text", "text": sys}]);
        }
        if !api_tools.is_empty() {
            body["tools"] = serde_json::to_value(&api_tools).map_err(Error::Serialization)?;
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
                let raw_body = resp.text().await.map_err(|e| {
                    Error::ModelProvider(format!("failed to read response body: {e}"))
                })?;
                let api_resp: ApiResponse = serde_json::from_str(&raw_body)
                    .map_err(|e| Error::ModelProvider(format!("failed to parse response: {e}")))?;
                let response = Self::parse_response(&api_resp)?;

                // Debug: dump raw response when message text is empty
                // despite having completion tokens (likely unknown content block types).
                if response.usage.completion_tokens > 0
                    && !response.message.content.has_tool_calls()
                    && response
                        .message
                        .content
                        .as_text()
                        .is_none_or(|t| t.is_empty())
                {
                    tracing::warn!(
                        completion_tokens = response.usage.completion_tokens,
                        ?response.stop_reason,
                        content_blocks = api_resp.content.len(),
                        block_types = ?api_resp.content.iter().map(|b| match b {
                            ResponseBlock::Text { .. } => "text",
                            ResponseBlock::ToolUse { .. } => "tool_use",
                            ResponseBlock::Unknown => "unknown",
                        }).collect::<Vec<_>>(),
                        "empty message text with completion tokens — dumping raw response"
                    );
                    tracing::warn!(raw_body = %raw_body, "raw Anthropic API response");
                }

                return Ok(response);
            }

            let error_body = resp.text().await.unwrap_or_default();
            let is_retryable = matches!(status.as_u16(), 429 | 500 | 502 | 503 | 529);
            // Fix #186: map status codes to specific error variants.
            last_err = Some(Self::map_status_error(status, &error_body));

            if !is_retryable {
                break;
            }
        }

        Err(last_err.unwrap_or_else(|| Error::ModelProvider("request failed".into())))
    }

    /// Fix #175: streaming implementation using Anthropic SSE.
    async fn chat_stream(
        &self,
        messages: &[Message],
        tools: &[ToolSchema],
        on_event: &(dyn Fn(StreamEvent) + Send + Sync),
    ) -> Result<ModelResponse> {
        let (system, api_messages) = Self::build_messages(messages)?;

        if api_messages.is_empty() {
            return Err(Error::ModelBadRequest(
                "cannot call Anthropic API with an empty message list".into(),
            ));
        }

        let api_tools = Self::build_tools(tools);

        let mut body = serde_json::json!({
            "model": self.model,
            "max_tokens": self.max_tokens,
            "messages": api_messages,
            "stream": true,
        });

        if let Some(sys) = system {
            body["system"] = serde_json::json!([{"type": "text", "text": sys}]);
        }
        if !api_tools.is_empty() {
            body["tools"] = serde_json::to_value(&api_tools).map_err(Error::Serialization)?;
        }

        tracing::debug!(model = %self.model, "calling Anthropic Messages API (streaming)");

        // Retry the initial connection with the same backoff as the non-streaming path.
        let mut last_err = None;
        let mut resp = None;
        for attempt in 0..=MAX_RETRIES {
            if attempt > 0 {
                let delay = RETRY_BASE_DELAY * 2u32.pow(attempt - 1);
                tracing::warn!(attempt, "retrying Anthropic streaming API after {delay:?}");
                tokio::time::sleep(delay).await;
            }

            let r = match self
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

            let status = r.status();
            if !status.is_success() {
                let error_body = r.text().await.unwrap_or_default();
                let is_retryable = matches!(status.as_u16(), 429 | 500 | 502 | 503 | 529);
                last_err = Some(Self::map_status_error(status, &error_body));
                if !is_retryable {
                    break;
                }
                continue;
            }

            resp = Some(r);
            break;
        }
        let resp = resp.ok_or_else(|| {
            last_err.unwrap_or_else(|| Error::ModelProvider("request failed".into()))
        })?;

        // Parse SSE events from the response body.
        let mut buffer = String::new();
        let mut current_event_type = String::new();
        let mut full_text = String::new();
        let mut tool_calls: Vec<ToolCall> = Vec::new();
        // Per-block accumulators for tool input JSON (keyed by block index).
        let mut tool_input_bufs: std::collections::HashMap<usize, String> =
            std::collections::HashMap::new();
        let mut tool_meta: std::collections::HashMap<usize, (String, String)> =
            std::collections::HashMap::new();
        let mut input_tokens: u32 = 0;
        let mut output_tokens: u32 = 0;
        let mut cache_read_tokens: u32 = 0;
        let mut cache_creation_tokens: u32 = 0;
        let mut stop_reason = StopReason::EndTurn;

        let mut response = resp;
        let mut stream_interrupted = false;
        let mut chunks_received: u64 = 0;
        let mut bytes_received: u64 = 0;
        let stream_start = std::time::Instant::now();
        loop {
            let chunk = match response.chunk().await {
                Ok(Some(chunk)) => chunk,
                Ok(None) => break, // stream finished normally
                Err(e) => {
                    let elapsed = stream_start.elapsed();
                    tracing::warn!(
                        error = %e,
                        error_debug = ?e,
                        model = %self.model,
                        chunks_received,
                        bytes_received,
                        elapsed_ms = elapsed.as_millis() as u64,
                        accumulated_text_len = full_text.len(),
                        pending_tool_calls = tool_input_bufs.len(),
                        last_event_type = %current_event_type,
                        buffer_len = buffer.len(),
                        "stream read error mid-stream, returning partial response"
                    );
                    stream_interrupted = true;
                    break;
                }
            };
            chunks_received += 1;
            bytes_received += chunk.len() as u64;
            buffer.push_str(&String::from_utf8_lossy(&chunk));

            while let Some(newline_pos) = buffer.find('\n') {
                let line = buffer[..newline_pos].trim_end().to_string();
                buffer = buffer[newline_pos + 1..].to_string();

                if let Some(event_type) = line.strip_prefix("event: ") {
                    current_event_type = event_type.to_string();
                } else if let Some(data) = line.strip_prefix("data: ") {
                    match current_event_type.as_str() {
                        "message_start" => {
                            if let Ok(evt) = serde_json::from_str::<SseMessageStart>(data) {
                                input_tokens = evt.message.usage.input_tokens;
                                cache_read_tokens =
                                    evt.message.usage.cache_read_input_tokens.unwrap_or(0);
                                cache_creation_tokens =
                                    evt.message.usage.cache_creation_input_tokens.unwrap_or(0);
                            }
                        }
                        "content_block_start" => {
                            if let Ok(evt) = serde_json::from_str::<SseContentBlockStart>(data) {
                                match evt.content_block {
                                    SseContentBlock::ToolUse { id, name } => {
                                        tool_meta.insert(evt.index, (id, name));
                                        tool_input_bufs.insert(evt.index, String::new());
                                    }
                                    SseContentBlock::Text { .. } => {}
                                    SseContentBlock::Unknown => {
                                        tracing::warn!(
                                            index = evt.index,
                                            raw_data = %data,
                                            "streaming: unhandled content block type"
                                        );
                                    }
                                }
                            }
                        }
                        "content_block_delta" => {
                            if let Ok(evt) = serde_json::from_str::<SseContentBlockDelta>(data) {
                                match evt.delta {
                                    SseDelta::TextDelta { text } => {
                                        full_text.push_str(&text);
                                        on_event(StreamEvent::TextDelta(text));
                                    }
                                    SseDelta::InputJsonDelta { partial_json } => {
                                        if let Some(buf) = tool_input_bufs.get_mut(&evt.index) {
                                            buf.push_str(&partial_json);
                                        }
                                    }
                                    SseDelta::Unknown => {
                                        tracing::debug!(
                                            index = evt.index,
                                            raw_data = %data,
                                            "streaming: unhandled delta type"
                                        );
                                    }
                                }
                            }
                        }
                        "content_block_stop" => {
                            // Finalize any tool call whose input is now complete.
                            if let Ok(evt) = serde_json::from_str::<SseContentBlockStop>(data) {
                                if let Some(json_buf) = tool_input_bufs.remove(&evt.index) {
                                    if let Some((id, name)) = tool_meta.remove(&evt.index) {
                                        let input: serde_json::Value = serde_json::from_str(
                                            &json_buf,
                                        )
                                        .unwrap_or(serde_json::Value::Object(Default::default()));
                                        tool_calls.push(ToolCall {
                                            id,
                                            name,
                                            arguments: input,
                                        });
                                    }
                                }
                            }
                        }
                        "message_delta" => {
                            if let Ok(evt) = serde_json::from_str::<SseMessageDelta>(data) {
                                output_tokens = evt.usage.output_tokens;
                                stop_reason = match evt.delta.stop_reason.as_deref() {
                                    Some("tool_use") => StopReason::ToolUse,
                                    Some("max_tokens") => StopReason::MaxTokens,
                                    Some("content_filter") => StopReason::ContentPolicy,
                                    _ => StopReason::EndTurn,
                                };
                            }
                        }
                        _ => {} // message_stop, ping, etc.
                    }
                }
                // Blank lines and other lines are ignored.
            }
        }

        let stream_elapsed = stream_start.elapsed();
        if stream_interrupted {
            tracing::info!(
                chunks_received,
                bytes_received,
                elapsed_ms = stream_elapsed.as_millis() as u64,
                completed_tool_calls = tool_calls.len(),
                discarded_partial_tool_calls = tool_input_bufs.len(),
                text_len = full_text.len(),
                "stream interrupted — returning partial Anthropic response"
            );
            // Discard any in-progress tool calls that never received a
            // `content_block_stop` event (their JSON is incomplete and
            // would fail to parse downstream).
            tool_input_bufs.clear();
            tool_meta.clear();
        } else {
            tracing::debug!(
                chunks_received,
                bytes_received,
                elapsed_ms = stream_elapsed.as_millis() as u64,
                "Anthropic stream completed normally"
            );
        }

        let usage = Usage {
            prompt_tokens: input_tokens,
            completion_tokens: output_tokens,
            cache_read_tokens,
            cache_creation_tokens,
        };

        let (content, text_field) = if !tool_calls.is_empty() {
            let tc = if tool_calls.len() == 1 {
                MessageContent::ToolCall(tool_calls.into_iter().next().unwrap())
            } else {
                MessageContent::MultiToolCall(tool_calls)
            };
            (
                tc,
                if full_text.is_empty() {
                    None
                } else {
                    Some(full_text)
                },
            )
        } else {
            (MessageContent::Text(full_text), None)
        };

        let response = ModelResponse {
            message: Message {
                id: Uuid::new_v4(),
                role: Role::Assistant,
                content,
                created_at: Utc::now(),
            },
            usage,
            stop_reason,
            text: text_field,
        };

        // Debug: dump response info when message text is empty
        // despite having completion tokens (likely unknown content block types).
        if response.usage.completion_tokens > 0
            && !response.message.content.has_tool_calls()
            && response
                .message
                .content
                .as_text()
                .is_none_or(|t| t.is_empty())
        {
            tracing::warn!(
                completion_tokens = response.usage.completion_tokens,
                ?response.stop_reason,
                "streaming: empty message text with completion tokens — \
                 response may contain unhandled content block types"
            );
        }

        on_event(StreamEvent::Done(response.clone()));
        Ok(response)
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
    Blocks(Vec<ContentBlock>),
}

#[derive(Serialize)]
#[serde(tag = "type")]
enum ContentBlock {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "image")]
    Image { source: ImageSource },
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
        #[serde(skip_serializing_if = "is_false")]
        is_error: bool,
    },
}

#[derive(Serialize)]
struct ImageSource {
    #[serde(rename = "type")]
    source_type: String,
    media_type: String,
    data: String,
}

fn is_false(v: &bool) -> bool {
    !v
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

/// Fix #206: added Unknown catch-all so new block types don't crash deserialization.
#[derive(Debug, Deserialize)]
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
    /// Catch-all for any block type not yet handled (e.g. future API additions).
    #[serde(other)]
    Unknown,
}

/// Fix #203: added cache token fields.
#[derive(Deserialize)]
struct ApiUsage {
    input_tokens: u32,
    output_tokens: u32,
    #[serde(default)]
    cache_read_input_tokens: Option<u32>,
    #[serde(default)]
    cache_creation_input_tokens: Option<u32>,
}

// --- Streaming SSE types ---

#[derive(Deserialize)]
struct SseMessageStart {
    message: SseMessageMeta,
}

#[derive(Deserialize)]
struct SseMessageMeta {
    usage: ApiUsage,
}

#[derive(Deserialize)]
struct SseContentBlockStart {
    index: usize,
    content_block: SseContentBlock,
}

#[derive(Deserialize)]
#[serde(tag = "type")]
#[allow(dead_code)]
enum SseContentBlock {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "tool_use")]
    ToolUse { id: String, name: String },
    #[serde(other)]
    Unknown,
}

#[derive(Deserialize)]
struct SseContentBlockDelta {
    index: usize,
    delta: SseDelta,
}

#[derive(Deserialize)]
#[serde(tag = "type")]
enum SseDelta {
    #[serde(rename = "text_delta")]
    TextDelta { text: String },
    #[serde(rename = "input_json_delta")]
    InputJsonDelta { partial_json: String },
    #[serde(other)]
    Unknown,
}

#[derive(Deserialize)]
struct SseContentBlockStop {
    index: usize,
}

#[derive(Deserialize)]
struct SseMessageDelta {
    delta: SseMessageDeltaInfo,
    usage: SseDeltaUsage,
}

#[derive(Deserialize)]
struct SseMessageDeltaInfo {
    stop_reason: Option<String>,
}

#[derive(Deserialize)]
struct SseDeltaUsage {
    output_tokens: u32,
}
