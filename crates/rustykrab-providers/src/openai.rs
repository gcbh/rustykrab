use crate::backoff::retry_delay;
use crate::line_buffer::LineBuffer;
use async_trait::async_trait;
use chrono::Utc;
use rustykrab_core::error::Result;
use rustykrab_core::model::{ModelProvider, ModelResponse, StopReason, StreamEvent, Usage};
use rustykrab_core::types::{ContentBlock, Message, MessageContent, Role, ToolCall, ToolSchema};
use rustykrab_core::Error;
use serde::{Deserialize, Serialize};
use std::time::Duration;
use uuid::Uuid;

/// Maximum number of retries for transient errors (429, 5xx).
const MAX_RETRIES: u32 = 3;
/// Base delay for exponential backoff (doubles each retry).
const RETRY_BASE_DELAY: Duration = Duration::from_secs(1);

/// Configuration for OpenAI-compatible inference.
#[derive(Debug, Clone)]
pub struct OpenAiConfig {
    /// Temperature for sampling (0.0 = deterministic, 0.7 = creative).
    pub temperature: f32,
    /// Top-p nucleus sampling threshold.
    pub top_p: f32,
    /// Maximum tokens to generate in the response.
    pub max_tokens: u32,
    /// Request token usage in the final stream chunk via `stream_options`.
    /// Supported by llama-server, mistral.rs, LM Studio and OpenAI itself;
    /// disable for servers that reject the field.
    pub include_usage: bool,
}

impl Default for OpenAiConfig {
    fn default() -> Self {
        Self {
            temperature: 0.1,
            top_p: 0.9,
            max_tokens: 8192,
            include_usage: true,
        }
    }
}

impl OpenAiConfig {
    /// Configuration optimized for tool-calling tasks (low temperature).
    pub fn tool_calling() -> Self {
        Self {
            temperature: 0.0,
            top_p: 0.9,
            max_tokens: 4096,
            include_usage: true,
        }
    }

    /// Configuration for creative drafting (higher temperature).
    pub fn creative() -> Self {
        Self {
            temperature: 0.7,
            top_p: 0.95,
            max_tokens: 16384,
            include_usage: true,
        }
    }
}

/// Provider for any server exposing an OpenAI-compatible `/v1/chat/completions`
/// endpoint.
///
/// This covers the local Apple Silicon serving stacks worth using in place of
/// Ollama — `llama-server` (llama.cpp), mistral.rs, vllm-mlx, `mlx_lm.server`,
/// LM Studio's headless daemon and exo — as well as OpenAI-compatible hosted
/// APIs. Point [`with_base_url`](Self::with_base_url) at the server and the
/// rest of the agent is unchanged.
pub struct OpenAiProvider {
    client: reqwest::Client,
    base_url: String,
    model: String,
    api_key: Option<String>,
    provider_name: String,
    config: OpenAiConfig,
    /// Whether the target server accepts image content parts. Unlike Ollama
    /// (which can probe `/api/show` for a model's capabilities), an arbitrary
    /// OpenAI-compatible server gives no reliable way to detect this, so it
    /// defaults to `false` — matching `ModelProvider::supports_vision`'s
    /// default — and callers opt in explicitly via `with_vision`.
    vision: bool,
}

impl OpenAiProvider {
    pub fn new(model: impl Into<String>) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(300))
            .connect_timeout(Duration::from_secs(10))
            .build()
            .expect("failed to build HTTP client");
        Self {
            client,
            // llama-server's default listen port.
            base_url: "http://localhost:8080".to_string(),
            model: model.into(),
            api_key: None,
            provider_name: "openai".to_string(),
            config: OpenAiConfig::default(),
            vision: false,
        }
    }

    /// Opt in to sending image content to this server. See the `vision`
    /// field doc for why this can't be auto-detected.
    pub fn with_vision(mut self, vision: bool) -> Self {
        self.vision = vision;
        self
    }

    pub fn with_base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = url.into();
        self
    }

    /// Bearer token. Optional — most local servers ignore it entirely.
    pub fn with_api_key(mut self, api_key: impl Into<String>) -> Self {
        let key = api_key.into();
        self.api_key = if key.is_empty() { None } else { Some(key) };
        self
    }

    /// Override the name reported by [`ModelProvider::name`] so logs identify
    /// the actual backend (e.g. "llama-server", "mistralrs").
    pub fn with_provider_name(mut self, name: impl Into<String>) -> Self {
        self.provider_name = name.into();
        self
    }

    pub fn with_config(mut self, config: OpenAiConfig) -> Self {
        self.config = config;
        self
    }

    pub fn with_temperature(mut self, temperature: f32) -> Self {
        self.config.temperature = temperature;
        self
    }

    pub fn with_max_tokens(mut self, max_tokens: u32) -> Self {
        self.config.max_tokens = max_tokens;
        self
    }

    /// Get the model name.
    pub fn model(&self) -> &str {
        &self.model
    }

    /// Build the chat-completions URL, tolerating a base URL given either with
    /// or without the `/v1` suffix — both spellings are common in the wild.
    fn chat_url(&self) -> String {
        let base = self.base_url.trim_end_matches('/');
        if base.ends_with("/v1") {
            format!("{base}/chat/completions")
        } else {
            format!("{base}/v1/chat/completions")
        }
    }

    fn build_messages(messages: &[Message], supports_vision: bool) -> Result<Vec<OpenAiMessage>> {
        let mut out = Vec::with_capacity(messages.len());
        for msg in messages {
            let role = match msg.role {
                Role::System => "system",
                Role::User => "user",
                Role::Assistant => "assistant",
                Role::Tool => "tool",
            };

            match &msg.content {
                MessageContent::Text(text) => out.push(OpenAiMessage {
                    role: role.to_string(),
                    content: Some(OpenAiContent::Text(text.clone())),
                    tool_calls: None,
                    tool_call_id: None,
                }),
                // Tool calls are always an assistant turn regardless of how
                // the message was tagged.
                MessageContent::ToolCall(call) => out.push(OpenAiMessage {
                    role: "assistant".to_string(),
                    content: None,
                    tool_calls: Some(vec![Self::wire_tool_call(call)?]),
                    tool_call_id: None,
                }),
                MessageContent::MultiToolCall(calls) => out.push(OpenAiMessage {
                    role: "assistant".to_string(),
                    content: None,
                    tool_calls: Some(
                        calls
                            .iter()
                            .map(Self::wire_tool_call)
                            .collect::<Result<Vec<_>>>()?,
                    ),
                    tool_call_id: None,
                }),
                // Unlike Ollama's native API, OpenAI requires tool results
                // to reference the originating call by id.
                MessageContent::ToolResult(result) => {
                    let content = match &result.output {
                        serde_json::Value::String(s) => s.clone(),
                        other => serde_json::to_string(other).map_err(Error::Serialization)?,
                    };
                    out.push(OpenAiMessage {
                        role: "tool".to_string(),
                        content: Some(OpenAiContent::Text(content)),
                        tool_calls: None,
                        tool_call_id: Some(result.call_id.clone()),
                    });
                    // The OpenAI `tool` role's content is a plain string — no
                    // image parts allowed there. Mirror Ollama's approach:
                    // surface tool-produced images (e.g. screenshots) as a
                    // follow-up user message, and only for vision-capable
                    // targets so a non-vision server isn't handed a payload
                    // it will reject or silently ignore.
                    if supports_vision && !result.images.is_empty() {
                        let parts = Self::image_parts(&result.images);
                        if !parts.is_empty() {
                            out.push(OpenAiMessage {
                                role: "user".to_string(),
                                content: Some(OpenAiContent::Parts(parts)),
                                tool_calls: None,
                                tool_call_id: None,
                            });
                        }
                    }
                }
                MessageContent::MultiPart(blocks) => {
                    if supports_vision {
                        let mut parts = Vec::new();
                        for b in blocks {
                            match b {
                                ContentBlock::Text { text } => {
                                    parts.push(OpenAiContentPart::Text { text: text.clone() })
                                }
                                ContentBlock::Image { media_type, data } => {
                                    parts.push(OpenAiContentPart::ImageUrl {
                                        image_url: OpenAiImageUrl {
                                            url: Self::data_uri(media_type, data),
                                        },
                                    })
                                }
                                // Nothing else belongs in an input message.
                                ContentBlock::ToolUse { .. } | ContentBlock::ToolResult { .. } => {}
                            }
                        }
                        out.push(OpenAiMessage {
                            role: role.to_string(),
                            content: Some(OpenAiContent::Parts(parts)),
                            tool_calls: None,
                            tool_call_id: None,
                        });
                    } else {
                        // Non-vision target: drop image blocks, keep the text
                        // so the turn still carries useful content instead of
                        // silently vanishing from the conversation.
                        let text = blocks
                            .iter()
                            .filter_map(|b| match b {
                                ContentBlock::Text { text } => Some(text.as_str()),
                                _ => None,
                            })
                            .collect::<Vec<_>>()
                            .join("\n");
                        out.push(OpenAiMessage {
                            role: role.to_string(),
                            content: Some(OpenAiContent::Text(text)),
                            tool_calls: None,
                            tool_call_id: None,
                        });
                    }
                }
            }
        }
        Ok(out)
    }

    fn data_uri(media_type: &str, data: &[u8]) -> String {
        use base64::engine::general_purpose::STANDARD;
        use base64::Engine;
        format!("data:{media_type};base64,{}", STANDARD.encode(data))
    }

    fn image_parts(blocks: &[ContentBlock]) -> Vec<OpenAiContentPart> {
        blocks
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Image { media_type, data } => Some(OpenAiContentPart::ImageUrl {
                    image_url: OpenAiImageUrl {
                        url: Self::data_uri(media_type, data),
                    },
                }),
                _ => None,
            })
            .collect()
    }

    /// OpenAI encodes tool-call arguments as a JSON *string*, not an object.
    fn wire_tool_call(call: &ToolCall) -> Result<OpenAiToolCall> {
        Ok(OpenAiToolCall {
            id: call.id.clone(),
            r#type: "function".to_string(),
            function: OpenAiFunctionCall {
                name: call.name.clone(),
                arguments: serde_json::to_string(&call.arguments).map_err(Error::Serialization)?,
            },
        })
    }

    fn build_tools(tools: &[ToolSchema]) -> Vec<OpenAiTool> {
        tools
            .iter()
            .map(|t| OpenAiTool {
                r#type: "function".to_string(),
                function: OpenAiToolDef {
                    name: t.name.clone(),
                    description: t.description.clone(),
                    parameters: t.parameters.clone(),
                },
            })
            .collect()
    }

    /// Decode tool-call arguments. Spec-compliant servers send a JSON-encoded
    /// string; servers that send a bare object are normalized to the same form
    /// on deserialization. An empty string is the no-argument call it
    /// represents.
    fn parse_arguments_str(s: &str) -> serde_json::Value {
        if s.trim().is_empty() {
            return serde_json::json!({});
        }
        serde_json::from_str(s).unwrap_or_else(|_| serde_json::Value::String(s.to_string()))
    }

    fn map_usage(usage: Option<OpenAiUsage>) -> Usage {
        let u = usage.unwrap_or_default();
        Usage {
            prompt_tokens: u.prompt_tokens,
            completion_tokens: u.completion_tokens,
            // Reported by servers with prompt-cache reuse (llama-server's
            // `--cache-reuse`, vLLM's prefix cache) — the headline metric for
            // agent loops that re-send a growing conversation each turn.
            cache_read_tokens: u
                .prompt_tokens_details
                .map(|d| d.cached_tokens)
                .unwrap_or(0),
            cache_creation_tokens: 0,
        }
    }

    /// Assemble the final response from collected tool calls or text.
    fn finish(
        tool_calls: Vec<ToolCall>,
        text: Option<String>,
        finish_reason: Option<&str>,
        usage: Usage,
    ) -> ModelResponse {
        if !tool_calls.is_empty() {
            let content = if tool_calls.len() == 1 {
                MessageContent::ToolCall(tool_calls.into_iter().next().unwrap())
            } else {
                MessageContent::MultiToolCall(tool_calls)
            };
            return ModelResponse {
                message: Message {
                    id: Uuid::new_v4(),
                    role: Role::Assistant,
                    content,
                    created_at: Utc::now(),
                    // Stamped by the caller that persists the message, not
                    // known at the provider layer.
                    agent_version: None,
                },
                usage,
                stop_reason: StopReason::ToolUse,
                // Preserve any reasoning text emitted alongside the calls.
                text,
            };
        }

        let stop_reason = match finish_reason {
            Some("length") => StopReason::MaxTokens,
            _ => StopReason::EndTurn,
        };

        ModelResponse {
            message: Message {
                id: Uuid::new_v4(),
                role: Role::Assistant,
                content: MessageContent::Text(text.unwrap_or_default()),
                created_at: Utc::now(),
                agent_version: None,
            },
            usage,
            stop_reason,
            text: None,
        }
    }

    fn parse_response(resp: OpenAiResponse) -> Result<ModelResponse> {
        let usage = Self::map_usage(resp.usage);
        let choice = resp.choices.into_iter().next().ok_or_else(|| {
            Error::ModelProvider("OpenAI-compatible API returned no choices".into())
        })?;

        let msg = choice.message;
        let text = msg.content.filter(|c| !c.is_empty());

        let tool_calls: Vec<ToolCall> = msg
            .tool_calls
            .unwrap_or_default()
            .into_iter()
            .map(|tc| ToolCall {
                id: if tc.id.is_empty() {
                    Uuid::new_v4().to_string()
                } else {
                    tc.id
                },
                name: tc.function.name,
                arguments: Self::parse_arguments_str(&tc.function.arguments),
            })
            .collect();

        Ok(Self::finish(
            tool_calls,
            text,
            choice.finish_reason.as_deref(),
            usage,
        ))
    }

    fn build_body(
        &self,
        messages: &[Message],
        tools: &[ToolSchema],
        stream: bool,
    ) -> Result<serde_json::Value> {
        let api_messages = Self::build_messages(messages, self.vision)?;

        if api_messages.is_empty() {
            return Err(Error::ModelBadRequest(
                "cannot call an OpenAI-compatible API with an empty message list".into(),
            ));
        }

        let mut body = serde_json::json!({
            "model": self.model,
            "messages": api_messages,
            "stream": stream,
            "temperature": self.config.temperature,
            "top_p": self.config.top_p,
            "max_tokens": self.config.max_tokens,
        });

        let api_tools = Self::build_tools(tools);
        if !api_tools.is_empty() {
            body["tools"] = serde_json::to_value(&api_tools).map_err(Error::Serialization)?;
        }

        if stream && self.config.include_usage {
            body["stream_options"] = serde_json::json!({ "include_usage": true });
        }

        Ok(body)
    }

    fn request(&self, body: &serde_json::Value) -> reqwest::RequestBuilder {
        let req = self.client.post(self.chat_url()).json(body);
        match &self.api_key {
            Some(key) => req.bearer_auth(key),
            None => req,
        }
    }

    /// Map an HTTP status code to a specific error variant.
    fn map_status_error(&self, status: reqwest::StatusCode, body: &str) -> Error {
        let name = &self.provider_name;
        match status.as_u16() {
            400 => Error::ModelBadRequest(format!("{name} API: {body}")),
            401 | 403 => Error::ModelAuthError(format!("{name} API: {body}")),
            429 => Error::ModelRateLimit(format!("{name} API: {body}")),
            503 => Error::ModelOverloaded(format!("{name} API: {body}")),
            _ => Error::ModelProvider(format!("{name} API returned {status}: {body}")),
        }
    }

    fn connect_error(&self, e: impl std::fmt::Display) -> Error {
        Error::ModelProvider(format!(
            "failed to connect to {} at {}: {e}. Is the server running?",
            self.provider_name, self.base_url
        ))
    }
}

#[async_trait]
impl ModelProvider for OpenAiProvider {
    fn name(&self) -> &str {
        &self.provider_name
    }

    fn supports_vision(&self) -> bool {
        self.vision
    }

    async fn chat(&self, messages: &[Message], tools: &[ToolSchema]) -> Result<ModelResponse> {
        let body = self.build_body(messages, tools, false)?;

        tracing::debug!(
            model = %self.model,
            base_url = %self.base_url,
            "calling OpenAI-compatible chat API"
        );

        let mut last_err = None;
        for attempt in 0..=MAX_RETRIES {
            if attempt > 0 {
                let delay = retry_delay(RETRY_BASE_DELAY, attempt);
                tracing::warn!(
                    attempt,
                    "retrying {} API after {delay:?}",
                    self.provider_name
                );
                tokio::time::sleep(delay).await;
            }

            let resp = match self.request(&body).send().await {
                Ok(r) => r,
                Err(e) => {
                    last_err = Some(self.connect_error(e));
                    continue;
                }
            };

            let status = resp.status();
            if status.is_success() {
                let parsed: OpenAiResponse = resp.json().await.map_err(|e| {
                    Error::ModelProvider(format!(
                        "failed to parse {} response: {e}",
                        self.provider_name
                    ))
                })?;
                return Self::parse_response(parsed);
            }

            let error_body = resp.text().await.unwrap_or_default();
            let is_retryable = matches!(status.as_u16(), 429 | 500 | 502 | 503 | 529);
            last_err = Some(self.map_status_error(status, &error_body));

            if !is_retryable {
                break;
            }
        }

        Err(last_err.unwrap_or_else(|| Error::ModelProvider("request failed".into())))
    }

    async fn chat_stream(
        &self,
        messages: &[Message],
        tools: &[ToolSchema],
        on_event: &(dyn Fn(StreamEvent) + Send + Sync),
    ) -> Result<ModelResponse> {
        let body = self.build_body(messages, tools, true)?;

        tracing::debug!(
            model = %self.model,
            base_url = %self.base_url,
            "calling OpenAI-compatible chat API (streaming)"
        );

        let resp = self
            .request(&body)
            .send()
            .await
            .map_err(|e| self.connect_error(e))?;

        let status = resp.status();
        if !status.is_success() {
            let error_body = resp.text().await.unwrap_or_default();
            return Err(self.map_status_error(status, &error_body));
        }

        let mut buffer = LineBuffer::new();
        let mut full_text = String::new();
        let mut partials: Vec<PartialToolCall> = Vec::new();
        let mut finish_reason: Option<String> = None;
        let mut usage = Usage::default();

        let mut response = resp;
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|e| Error::ModelProvider(format!("stream read error: {e}")))?
        {
            buffer.push_chunk(&chunk);

            while let Some(line) = buffer.next_line() {
                let line = line.trim_end();
                let Some(data) = line.strip_prefix("data: ") else {
                    // Blank separator lines and any `event:`/comment lines.
                    continue;
                };
                let data = data.trim();
                if data == "[DONE]" {
                    continue;
                }

                let stream_chunk: OpenAiStreamChunk = serde_json::from_str(data).map_err(|e| {
                    Error::ModelProvider(format!(
                        "failed to parse {} stream chunk: {e}",
                        self.provider_name
                    ))
                })?;

                // The usage-only trailer chunk carries no choices.
                if stream_chunk.usage.is_some() {
                    usage = Self::map_usage(stream_chunk.usage);
                }

                let Some(choice) = stream_chunk.choices.into_iter().next() else {
                    continue;
                };

                if let Some(reason) = choice.finish_reason {
                    finish_reason = Some(reason);
                }

                if let Some(content) = choice.delta.content {
                    if !content.is_empty() {
                        full_text.push_str(&content);
                        on_event(StreamEvent::TextDelta(content));
                    }
                }

                // Tool-call arguments arrive as fragments keyed by index and
                // must be concatenated before they parse as JSON.
                for delta in choice.delta.tool_calls.unwrap_or_default() {
                    let slot = Self::slot_for(&mut partials, &delta);
                    if let Some(id) = delta.id {
                        if !id.is_empty() {
                            slot.id = id;
                        }
                    }
                    if let Some(func) = delta.function {
                        if let Some(name) = func.name {
                            if !name.is_empty() {
                                slot.name = name;
                            }
                        }
                        if let Some(args) = func.arguments {
                            slot.arguments.push_str(&args);
                        }
                    }
                }
            }
        }

        let tool_calls: Vec<ToolCall> = partials
            .into_iter()
            .filter(|p| !p.name.is_empty())
            .map(|p| ToolCall {
                id: if p.id.is_empty() {
                    Uuid::new_v4().to_string()
                } else {
                    p.id
                },
                name: p.name,
                arguments: Self::parse_arguments_str(&p.arguments),
            })
            .collect();

        let text = if full_text.is_empty() {
            None
        } else {
            Some(full_text)
        };

        let response = Self::finish(tool_calls, text, finish_reason.as_deref(), usage);

        on_event(StreamEvent::Done(response.clone()));
        Ok(response)
    }
}

impl OpenAiProvider {
    /// Resolve which accumulator a streamed tool-call delta belongs to.
    ///
    /// `index` is the key per the OpenAI spec. Servers that omit it all report
    /// index 0, so a delta announcing a different call id is treated as a new
    /// call rather than corrupting the first one.
    fn slot_for<'a>(
        partials: &'a mut Vec<PartialToolCall>,
        delta: &OpenAiToolCallDelta,
    ) -> &'a mut PartialToolCall {
        let index = delta.index.unwrap_or(0);

        if let Some(existing) = partials.get(index) {
            let is_different_call = match (&delta.id, existing.id.is_empty()) {
                (Some(id), false) => !id.is_empty() && id != &existing.id,
                _ => false,
            };
            if is_different_call {
                partials.push(PartialToolCall::default());
                return partials.last_mut().unwrap();
            }
        }

        while partials.len() <= index {
            partials.push(PartialToolCall::default());
        }
        &mut partials[index]
    }
}

/// Accumulator for a tool call assembled across streaming deltas.
#[derive(Default)]
struct PartialToolCall {
    id: String,
    name: String,
    arguments: String,
}

// --- OpenAI API wire types (private) ---

#[derive(Serialize)]
struct OpenAiMessage {
    role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<OpenAiContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<OpenAiToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
}

/// A message's `content` field is either a plain string or an array of
/// typed parts (text/image_url) — the OpenAI vision wire format. Untagged so
/// a text-only message serializes exactly as before.
#[derive(Serialize)]
#[serde(untagged)]
enum OpenAiContent {
    Text(String),
    Parts(Vec<OpenAiContentPart>),
}

#[derive(Serialize)]
#[serde(tag = "type")]
enum OpenAiContentPart {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "image_url")]
    ImageUrl { image_url: OpenAiImageUrl },
}

#[derive(Serialize)]
struct OpenAiImageUrl {
    url: String,
}

#[derive(Serialize, Deserialize)]
struct OpenAiToolCall {
    #[serde(default)]
    id: String,
    #[serde(default, rename = "type")]
    r#type: String,
    function: OpenAiFunctionCall,
}

#[derive(Serialize)]
struct OpenAiFunctionCall {
    name: String,
    /// Serialized as a JSON-encoded string on the wire.
    arguments: String,
}

/// Lenient inbound form: spec says string, some servers send an object.
impl<'de> Deserialize<'de> for OpenAiFunctionCall {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Raw {
            #[serde(default)]
            name: String,
            #[serde(default)]
            arguments: serde_json::Value,
        }
        let raw = Raw::deserialize(deserializer)?;
        let arguments = match raw.arguments {
            serde_json::Value::String(s) => s,
            serde_json::Value::Null => String::new(),
            other => other.to_string(),
        };
        Ok(OpenAiFunctionCall {
            name: raw.name,
            arguments,
        })
    }
}

#[derive(Serialize)]
struct OpenAiTool {
    r#type: String,
    function: OpenAiToolDef,
}

#[derive(Serialize)]
struct OpenAiToolDef {
    name: String,
    description: String,
    parameters: serde_json::Value,
}

#[derive(Deserialize)]
struct OpenAiResponse {
    #[serde(default)]
    choices: Vec<OpenAiChoice>,
    #[serde(default)]
    usage: Option<OpenAiUsage>,
}

#[derive(Deserialize)]
struct OpenAiChoice {
    message: OpenAiResponseMessage,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
struct OpenAiResponseMessage {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<OpenAiToolCall>>,
}

#[derive(Deserialize, Default)]
struct OpenAiUsage {
    #[serde(default)]
    prompt_tokens: u32,
    #[serde(default)]
    completion_tokens: u32,
    #[serde(default)]
    prompt_tokens_details: Option<OpenAiPromptTokensDetails>,
}

#[derive(Deserialize)]
struct OpenAiPromptTokensDetails {
    #[serde(default)]
    cached_tokens: u32,
}

#[derive(Deserialize)]
struct OpenAiStreamChunk {
    #[serde(default)]
    choices: Vec<OpenAiStreamChoice>,
    #[serde(default)]
    usage: Option<OpenAiUsage>,
}

#[derive(Deserialize)]
struct OpenAiStreamChoice {
    #[serde(default)]
    delta: OpenAiDelta,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Deserialize, Default)]
struct OpenAiDelta {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<OpenAiToolCallDelta>>,
}

#[derive(Deserialize)]
struct OpenAiToolCallDelta {
    #[serde(default)]
    index: Option<usize>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<OpenAiFunctionDelta>,
}

#[derive(Deserialize)]
struct OpenAiFunctionDelta {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustykrab_core::types::ToolResult;

    fn msg(role: Role, content: MessageContent) -> Message {
        Message {
            id: Uuid::new_v4(),
            role,
            content,
            created_at: Utc::now(),
            agent_version: None,
        }
    }

    fn tool_result(call_id: &str, output: serde_json::Value) -> ToolResult {
        ToolResult {
            call_id: call_id.to_string(),
            output,
            is_error: false,
            images: Vec::new(),
        }
    }

    #[test]
    fn chat_url_tolerates_both_base_url_spellings() {
        let bare = OpenAiProvider::new("m").with_base_url("http://localhost:8080");
        assert_eq!(bare.chat_url(), "http://localhost:8080/v1/chat/completions");

        let versioned = OpenAiProvider::new("m").with_base_url("http://localhost:1234/v1");
        assert_eq!(
            versioned.chat_url(),
            "http://localhost:1234/v1/chat/completions"
        );

        let trailing = OpenAiProvider::new("m").with_base_url("http://mac.tailnet:8080/");
        assert_eq!(
            trailing.chat_url(),
            "http://mac.tailnet:8080/v1/chat/completions"
        );
    }

    #[test]
    fn tool_results_carry_the_call_id() {
        let messages = vec![
            msg(
                Role::Assistant,
                MessageContent::ToolCall(ToolCall {
                    id: "call_abc".into(),
                    name: "read".into(),
                    arguments: serde_json::json!({"path": "/tmp/x"}),
                }),
            ),
            msg(
                Role::Tool,
                MessageContent::ToolResult(tool_result(
                    "call_abc",
                    serde_json::Value::String("contents".into()),
                )),
            ),
        ];

        let built = OpenAiProvider::build_messages(&messages, false).unwrap();

        assert_eq!(built[0].role, "assistant");
        let calls = built[0].tool_calls.as_ref().unwrap();
        assert_eq!(calls[0].id, "call_abc");
        // Arguments must be a JSON-encoded string, not an object.
        assert_eq!(calls[0].function.arguments, r#"{"path":"/tmp/x"}"#);

        assert_eq!(built[1].role, "tool");
        assert_eq!(built[1].tool_call_id.as_deref(), Some("call_abc"));
        assert!(matches!(&built[1].content, Some(OpenAiContent::Text(t)) if t == "contents"));
    }

    #[test]
    fn multipart_becomes_text_and_image_parts_when_vision_enabled() {
        let messages = vec![msg(
            Role::User,
            MessageContent::MultiPart(vec![
                ContentBlock::Text {
                    text: "what is this?".into(),
                },
                ContentBlock::Image {
                    media_type: "image/png".into(),
                    data: vec![1, 2, 3],
                },
            ]),
        )];

        let built = OpenAiProvider::build_messages(&messages, true).unwrap();
        assert_eq!(built.len(), 1);
        let Some(OpenAiContent::Parts(parts)) = &built[0].content else {
            panic!(
                "expected multi-part content, got {:?}",
                built[0].content.is_some()
            );
        };
        assert_eq!(parts.len(), 2);
        assert!(matches!(&parts[0], OpenAiContentPart::Text { text } if text == "what is this?"));
        assert!(matches!(
            &parts[1],
            OpenAiContentPart::ImageUrl { image_url }
                if image_url.url.starts_with("data:image/png;base64,")
        ));
    }

    #[test]
    fn multipart_drops_images_but_keeps_text_when_vision_disabled() {
        let messages = vec![msg(
            Role::User,
            MessageContent::MultiPart(vec![
                ContentBlock::Text {
                    text: "see attached".into(),
                },
                ContentBlock::Image {
                    media_type: "image/png".into(),
                    data: vec![1, 2, 3],
                },
            ]),
        )];

        let built = OpenAiProvider::build_messages(&messages, false).unwrap();
        assert_eq!(built.len(), 1);
        assert!(
            matches!(&built[0].content, Some(OpenAiContent::Text(t)) if t == "see attached"),
            "non-vision target should keep the text rather than drop the whole message"
        );
    }

    #[test]
    fn tool_result_images_become_a_followup_user_message_only_when_vision_enabled() {
        let mut result = tool_result("call_1", serde_json::json!("done"));
        result.images.push(ContentBlock::Image {
            media_type: "image/png".into(),
            data: vec![9, 9, 9],
        });
        let messages = vec![msg(Role::Tool, MessageContent::ToolResult(result.clone()))];

        // Non-vision: only the text tool-result message, image silently dropped
        // rather than sent to a server that can't use it.
        let built = OpenAiProvider::build_messages(&messages, false).unwrap();
        assert_eq!(built.len(), 1);
        assert_eq!(built[0].role, "tool");

        // Vision-enabled: a follow-up user message carries the image.
        let built = OpenAiProvider::build_messages(&messages, true).unwrap();
        assert_eq!(built.len(), 2);
        assert_eq!(built[0].role, "tool");
        assert_eq!(built[1].role, "user");
        let Some(OpenAiContent::Parts(parts)) = &built[1].content else {
            panic!("expected image parts in the follow-up message");
        };
        assert_eq!(parts.len(), 1);
    }

    #[test]
    fn empty_message_list_is_rejected() {
        let provider = OpenAiProvider::new("m");
        assert!(provider.build_body(&[], &[], false).is_err());
    }

    #[test]
    fn parse_arguments_accepts_string_object_and_empty() {
        // Spec-compliant: JSON-encoded string.
        assert_eq!(
            OpenAiProvider::parse_arguments_str(r#"{"a":1}"#),
            serde_json::json!({"a": 1})
        );
        // Lenient: a bare object from a non-compliant server is normalized to
        // the string form during deserialization, then parses back cleanly.
        let func: OpenAiFunctionCall =
            serde_json::from_value(serde_json::json!({"name": "t", "arguments": {"a": 1}}))
                .unwrap();
        assert_eq!(
            OpenAiProvider::parse_arguments_str(&func.arguments),
            serde_json::json!({"a": 1})
        );
        // Zero-argument calls commonly stream as an empty string.
        assert_eq!(
            OpenAiProvider::parse_arguments_str(""),
            serde_json::json!({})
        );
        // Unparseable input is preserved rather than dropped.
        assert_eq!(
            OpenAiProvider::parse_arguments_str("not json"),
            serde_json::Value::String("not json".into())
        );
    }

    #[test]
    fn parses_tool_calls_and_preserves_accompanying_text() {
        let body = serde_json::json!({
            "choices": [{
                "message": {
                    "content": "Let me check that.",
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": {"name": "read", "arguments": "{\"path\":\"/tmp/x\"}"}
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": {
                "prompt_tokens": 120,
                "completion_tokens": 8,
                "prompt_tokens_details": {"cached_tokens": 100}
            }
        });

        let resp = OpenAiProvider::parse_response(serde_json::from_value(body).unwrap()).unwrap();

        assert_eq!(resp.stop_reason, StopReason::ToolUse);
        assert_eq!(resp.text.as_deref(), Some("Let me check that."));
        let calls = resp.message.content.tool_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "read");
        assert_eq!(calls[0].arguments, serde_json::json!({"path": "/tmp/x"}));
        // Prompt-cache hits are surfaced for agent-loop cache tuning.
        assert_eq!(resp.usage.cache_read_tokens, 100);
        assert_eq!(resp.usage.prompt_tokens, 120);
    }

    #[test]
    fn truncated_response_maps_to_max_tokens() {
        let body = serde_json::json!({
            "choices": [{
                "message": {"content": "partial"},
                "finish_reason": "length"
            }]
        });
        let resp = OpenAiProvider::parse_response(serde_json::from_value(body).unwrap()).unwrap();
        assert_eq!(resp.stop_reason, StopReason::MaxTokens);
        assert_eq!(resp.message.content.as_text(), Some("partial"));
    }

    #[test]
    fn streaming_deltas_accumulate_by_index() {
        let mut partials: Vec<PartialToolCall> = Vec::new();

        let deltas = vec![
            serde_json::json!({"index": 0, "id": "call_1", "function": {"name": "read", "arguments": "{\"pa"}}),
            serde_json::json!({"index": 0, "function": {"arguments": "th\":\"/tmp/x\"}"}}),
            serde_json::json!({"index": 1, "id": "call_2", "function": {"name": "write", "arguments": "{}"}}),
        ];

        for d in deltas {
            let delta: OpenAiToolCallDelta = serde_json::from_value(d).unwrap();
            let slot = OpenAiProvider::slot_for(&mut partials, &delta);
            if let Some(id) = delta.id {
                if !id.is_empty() {
                    slot.id = id;
                }
            }
            if let Some(func) = delta.function {
                if let Some(name) = func.name {
                    if !name.is_empty() {
                        slot.name = name;
                    }
                }
                if let Some(args) = func.arguments {
                    slot.arguments.push_str(&args);
                }
            }
        }

        assert_eq!(partials.len(), 2);
        assert_eq!(partials[0].name, "read");
        assert_eq!(
            OpenAiProvider::parse_arguments_str(&partials[0].arguments),
            serde_json::json!({"path": "/tmp/x"})
        );
        assert_eq!(partials[1].id, "call_2");
        assert_eq!(partials[1].name, "write");
    }

    #[test]
    fn stream_options_only_sent_when_streaming_and_enabled() {
        let messages = vec![msg(Role::User, MessageContent::Text("hi".into()))];

        let provider = OpenAiProvider::new("m");
        let streaming = provider.build_body(&messages, &[], true).unwrap();
        assert_eq!(streaming["stream_options"]["include_usage"], true);

        let blocking = provider.build_body(&messages, &[], false).unwrap();
        assert!(blocking.get("stream_options").is_none());

        let opted_out = OpenAiProvider::new("m").with_config(OpenAiConfig {
            include_usage: false,
            ..Default::default()
        });
        let body = opted_out.build_body(&messages, &[], true).unwrap();
        assert!(body.get("stream_options").is_none());
    }
}
