use async_trait::async_trait;
use chrono::Utc;
use rustykrab_core::error::Result;
use rustykrab_core::model::{ModelProvider, ModelResponse, StopReason, StreamEvent, Usage};
use rustykrab_core::types::{Message, MessageContent, Role, ToolCall, ToolSchema};
use rustykrab_core::Error;
use serde::{Deserialize, Serialize};
use std::time::Duration;
use uuid::Uuid;

/// Maximum number of retries for transient errors (429, 5xx).
const MAX_RETRIES: u32 = 3;
/// Base delay for exponential backoff (doubles each retry).
const RETRY_BASE_DELAY: Duration = Duration::from_secs(1);

/// Configuration for Ollama model inference.
#[derive(Debug, Clone)]
pub struct OllamaConfig {
    /// Temperature for sampling (0.0 = deterministic, 0.7 = creative).
    pub temperature: f32,
    /// Context window size in tokens.
    pub num_ctx: u32,
    /// Number of parallel inference slots.
    pub num_parallel: u32,
    /// Top-p nucleus sampling threshold.
    pub top_p: f32,
    /// Number of tokens to predict (-1 = unlimited, 0 = fill context).
    pub num_predict: i32,
}

impl Default for OllamaConfig {
    fn default() -> Self {
        Self {
            temperature: 0.1,
            num_ctx: 131_072,
            num_parallel: 6,
            top_p: 0.9,
            num_predict: 8192,
        }
    }
}

impl OllamaConfig {
    /// Configuration optimized for tool-calling tasks (low temperature).
    pub fn tool_calling() -> Self {
        Self {
            temperature: 0.0,
            num_ctx: 131_072,
            num_parallel: 6,
            top_p: 0.9,
            num_predict: 4096,
        }
    }

    /// Configuration for creative drafting (higher temperature).
    pub fn creative() -> Self {
        Self {
            temperature: 0.7,
            num_ctx: 131_072,
            num_parallel: 6,
            top_p: 0.95,
            num_predict: 16384,
        }
    }
}

/// Ollama provider for local models (Gemma, Qwen, Llama, Mistral, etc.).
pub struct OllamaProvider {
    client: reqwest::Client,
    base_url: String,
    model: String,
    config: OllamaConfig,
}

impl OllamaProvider {
    pub fn new(model: impl Into<String>) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(300))
            .connect_timeout(Duration::from_secs(10))
            .build()
            .expect("failed to build HTTP client");
        Self {
            client,
            base_url: "http://localhost:11434".to_string(),
            model: model.into(),
            config: OllamaConfig::default(),
        }
    }

    pub fn with_base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = url.into();
        self
    }

    pub fn with_config(mut self, config: OllamaConfig) -> Self {
        self.config = config;
        self
    }

    pub fn with_temperature(mut self, temperature: f32) -> Self {
        self.config.temperature = temperature;
        self
    }

    /// Get the model name.
    pub fn model(&self) -> &str {
        &self.model
    }

    /// Fix #195: returns Result to propagate serialization errors.
    fn build_messages(messages: &[Message]) -> Result<Vec<OllamaMessage>> {
        messages
            .iter()
            .map(|msg| {
                let role = match msg.role {
                    Role::System => "system",
                    Role::User => "user",
                    Role::Assistant => "assistant",
                    Role::Tool => "tool",
                };

                Ok(match &msg.content {
                    MessageContent::Text(text) => OllamaMessage {
                        role: role.to_string(),
                        content: Some(text.clone()),
                        tool_calls: None,
                    },
                    MessageContent::ToolCall(call) => OllamaMessage {
                        role: role.to_string(),
                        content: None,
                        tool_calls: Some(vec![OllamaToolCall {
                            function: OllamaFunction {
                                name: call.name.clone(),
                                arguments: call.arguments.clone(),
                            },
                        }]),
                    },
                    MessageContent::MultiToolCall(calls) => OllamaMessage {
                        role: role.to_string(),
                        content: None,
                        tool_calls: Some(
                            calls
                                .iter()
                                .map(|c| OllamaToolCall {
                                    function: OllamaFunction {
                                        name: c.name.clone(),
                                        arguments: c.arguments.clone(),
                                    },
                                })
                                .collect(),
                        ),
                    },
                    MessageContent::ToolResult(result) => {
                        // Fix #182: avoid double-serialization of string values.
                        // Fix #195: propagate serialization errors.
                        let content = match &result.output {
                            serde_json::Value::String(s) => s.clone(),
                            other => {
                                serde_json::to_string(other).map_err(Error::Serialization)?
                            }
                        };
                        OllamaMessage {
                            role: role.to_string(),
                            content: Some(content),
                            tool_calls: None,
                        }
                    }
                })
            })
            .collect()
    }

    fn build_tools(tools: &[ToolSchema]) -> Vec<OllamaTool> {
        tools
            .iter()
            .map(|t| OllamaTool {
                r#type: "function".to_string(),
                function: OllamaToolDef {
                    name: t.name.clone(),
                    description: t.description.clone(),
                    parameters: t.parameters.clone(),
                },
            })
            .collect()
    }

    /// Normalize tool-call arguments: some models (notably Gemma) return
    /// arguments as a JSON-encoded string rather than an object. Detect
    /// that case and parse it into a proper `Value::Object`.
    fn normalize_arguments(args: serde_json::Value) -> serde_json::Value {
        if let serde_json::Value::String(ref s) = args {
            if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(s) {
                if parsed.is_object() {
                    return parsed;
                }
            }
        }
        args
    }

    fn parse_response(resp: OllamaResponse) -> Result<ModelResponse> {
        let msg = resp.message;

        // Fix #192: parse done_reason to detect truncation.
        let stop_reason = match resp.done_reason.as_deref() {
            Some("length") => StopReason::MaxTokens,
            _ => StopReason::EndTurn, // "stop" or absent → normal end
        };

        // Collect all tool calls.
        if let Some(tool_calls) = msg.tool_calls {
            if !tool_calls.is_empty() {
                let calls: Vec<ToolCall> = tool_calls
                    .into_iter()
                    .map(|tc| ToolCall {
                        id: Uuid::new_v4().to_string(),
                        name: tc.function.name,
                        arguments: Self::normalize_arguments(tc.function.arguments),
                    })
                    .collect();

                let content = if calls.len() == 1 {
                    MessageContent::ToolCall(calls.into_iter().next().unwrap())
                } else {
                    MessageContent::MultiToolCall(calls)
                };

                return Ok(ModelResponse {
                    message: Message {
                        id: Uuid::new_v4(),
                        role: Role::Assistant,
                        content,
                        created_at: Utc::now(),
                    },
                    usage: Usage {
                        prompt_tokens: resp.prompt_eval_count.unwrap_or(0),
                        completion_tokens: resp.eval_count.unwrap_or(0),
                        ..Default::default()
                    },
                    stop_reason: StopReason::ToolUse,
                    text: None,
                });
            }
        }

        Ok(ModelResponse {
            message: Message {
                id: Uuid::new_v4(),
                role: Role::Assistant,
                content: MessageContent::Text(msg.content.unwrap_or_default()),
                created_at: Utc::now(),
            },
            usage: Usage {
                prompt_tokens: resp.prompt_eval_count.unwrap_or(0),
                completion_tokens: resp.eval_count.unwrap_or(0),
                ..Default::default()
            },
            stop_reason,
            text: None,
        })
    }

    /// Map an HTTP status code to a specific error variant (#186).
    fn map_status_error(status: reqwest::StatusCode, body: &str) -> Error {
        match status.as_u16() {
            400 => Error::ModelBadRequest(format!("Ollama API: {body}")),
            401 | 403 => Error::ModelAuthError(format!("Ollama API: {body}")),
            429 => Error::ModelRateLimit(format!("Ollama API: {body}")),
            _ => Error::ModelProvider(format!("Ollama API returned {status}: {body}")),
        }
    }
}

#[async_trait]
impl ModelProvider for OllamaProvider {
    fn name(&self) -> &str {
        "ollama"
    }

    async fn chat(&self, messages: &[Message], tools: &[ToolSchema]) -> Result<ModelResponse> {
        let ollama_messages = Self::build_messages(messages)?;

        // Fix #200: validate non-empty messages.
        if ollama_messages.is_empty() {
            return Err(Error::ModelBadRequest(
                "cannot call Ollama API with an empty message list".into(),
            ));
        }

        let ollama_tools = Self::build_tools(tools);

        let mut body = serde_json::json!({
            "model": self.model,
            "messages": ollama_messages,
            "stream": false,
            "options": {
                "temperature": self.config.temperature,
                "num_ctx": self.config.num_ctx,
                "top_p": self.config.top_p,
                "num_predict": self.config.num_predict,
            },
        });

        if !ollama_tools.is_empty() {
            body["tools"] = serde_json::to_value(&ollama_tools).map_err(Error::Serialization)?;
        }

        tracing::debug!(model = %self.model, base_url = %self.base_url, "calling Ollama chat API");

        let url = format!("{}/api/chat", self.base_url);

        let mut last_err = None;
        for attempt in 0..=MAX_RETRIES {
            if attempt > 0 {
                let delay = RETRY_BASE_DELAY * 2u32.pow(attempt - 1);
                tracing::warn!(attempt, "retrying Ollama API after {delay:?}");
                tokio::time::sleep(delay).await;
            }

            let resp = match self.client.post(&url).json(&body).send().await {
                Ok(r) => r,
                Err(e) => {
                    last_err = Some(Error::ModelProvider(format!(
                        "failed to connect to Ollama at {}: {e}. Is Ollama running?",
                        self.base_url
                    )));
                    continue;
                }
            };

            let status = resp.status();
            if status.is_success() {
                let ollama_resp: OllamaResponse = resp.json().await.map_err(|e| {
                    Error::ModelProvider(format!("failed to parse Ollama response: {e}"))
                })?;
                return Self::parse_response(ollama_resp);
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

    /// Fix #175: streaming implementation using Ollama NDJSON.
    async fn chat_stream(
        &self,
        messages: &[Message],
        tools: &[ToolSchema],
        on_event: &(dyn Fn(StreamEvent) + Send + Sync),
    ) -> Result<ModelResponse> {
        let ollama_messages = Self::build_messages(messages)?;

        if ollama_messages.is_empty() {
            return Err(Error::ModelBadRequest(
                "cannot call Ollama API with an empty message list".into(),
            ));
        }

        let ollama_tools = Self::build_tools(tools);

        let mut body = serde_json::json!({
            "model": self.model,
            "messages": ollama_messages,
            "stream": true,
            "options": {
                "temperature": self.config.temperature,
                "num_ctx": self.config.num_ctx,
                "top_p": self.config.top_p,
                "num_predict": self.config.num_predict,
            },
        });

        if !ollama_tools.is_empty() {
            body["tools"] = serde_json::to_value(&ollama_tools).map_err(Error::Serialization)?;
        }

        tracing::debug!(model = %self.model, base_url = %self.base_url, "calling Ollama chat API (streaming)");

        let url = format!("{}/api/chat", self.base_url);

        let resp = self
            .client
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| {
                Error::ModelProvider(format!(
                    "failed to connect to Ollama at {}: {e}. Is Ollama running?",
                    self.base_url
                ))
            })?;

        let status = resp.status();
        if !status.is_success() {
            let error_body = resp.text().await.unwrap_or_default();
            return Err(Self::map_status_error(status, &error_body));
        }

        // Parse newline-delimited JSON chunks.
        let mut buffer = String::new();
        let mut full_text = String::new();
        let mut tool_calls: Vec<ToolCall> = Vec::new();
        let mut prompt_eval_count: u32 = 0;
        let mut eval_count: u32 = 0;
        let mut done_reason: Option<String> = None;

        let mut response = resp;
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|e| Error::ModelProvider(format!("stream read error: {e}")))?
        {
            buffer.push_str(&String::from_utf8_lossy(&chunk));

            while let Some(newline_pos) = buffer.find('\n') {
                let line = buffer[..newline_pos].trim().to_string();
                buffer = buffer[newline_pos + 1..].to_string();

                if line.is_empty() {
                    continue;
                }

                let stream_chunk: OllamaStreamChunk =
                    serde_json::from_str(&line).map_err(|e| {
                        Error::ModelProvider(format!(
                            "failed to parse Ollama stream chunk: {e}"
                        ))
                    })?;

                if let Some(ref content) = stream_chunk.message.content {
                    if !content.is_empty() {
                        full_text.push_str(content);
                        on_event(StreamEvent::TextDelta(content.clone()));
                    }
                }

                // Collect tool calls from the final chunk.
                if let Some(tcs) = stream_chunk.message.tool_calls {
                    for tc in tcs {
                        tool_calls.push(ToolCall {
                            id: Uuid::new_v4().to_string(),
                            name: tc.function.name,
                            arguments: Self::normalize_arguments(tc.function.arguments),
                        });
                    }
                }

                if stream_chunk.done {
                    prompt_eval_count = stream_chunk.prompt_eval_count.unwrap_or(0);
                    eval_count = stream_chunk.eval_count.unwrap_or(0);
                    done_reason = stream_chunk.done_reason;
                }
            }
        }

        let stop_reason = if !tool_calls.is_empty() {
            StopReason::ToolUse
        } else {
            match done_reason.as_deref() {
                Some("length") => StopReason::MaxTokens,
                _ => StopReason::EndTurn,
            }
        };

        let content = if !tool_calls.is_empty() {
            if tool_calls.len() == 1 {
                MessageContent::ToolCall(tool_calls.into_iter().next().unwrap())
            } else {
                MessageContent::MultiToolCall(tool_calls)
            }
        } else {
            MessageContent::Text(full_text)
        };

        let response = ModelResponse {
            message: Message {
                id: Uuid::new_v4(),
                role: Role::Assistant,
                content,
                created_at: Utc::now(),
            },
            usage: Usage {
                prompt_tokens: prompt_eval_count,
                completion_tokens: eval_count,
                ..Default::default()
            },
            stop_reason,
            text: None,
        };

        on_event(StreamEvent::Done(response.clone()));
        Ok(response)
    }
}

// --- Ollama API wire types (private) ---

#[derive(Serialize)]
struct OllamaMessage {
    role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<OllamaToolCall>>,
}

#[derive(Serialize, Deserialize)]
struct OllamaToolCall {
    function: OllamaFunction,
}

#[derive(Serialize, Deserialize)]
struct OllamaFunction {
    name: String,
    arguments: serde_json::Value,
}

#[derive(Serialize)]
struct OllamaTool {
    r#type: String,
    function: OllamaToolDef,
}

#[derive(Serialize)]
struct OllamaToolDef {
    name: String,
    description: String,
    parameters: serde_json::Value,
}

#[derive(Deserialize)]
struct OllamaResponse {
    message: OllamaResponseMessage,
    prompt_eval_count: Option<u32>,
    eval_count: Option<u32>,
    /// Fix #192: parse done_reason to detect truncation.
    #[serde(default)]
    done_reason: Option<String>,
}

#[derive(Deserialize)]
struct OllamaResponseMessage {
    content: Option<String>,
    tool_calls: Option<Vec<OllamaToolCall>>,
}

/// Streaming chunk from Ollama's NDJSON response.
#[derive(Deserialize)]
struct OllamaStreamChunk {
    message: OllamaStreamMessage,
    done: bool,
    #[serde(default)]
    done_reason: Option<String>,
    #[serde(default)]
    prompt_eval_count: Option<u32>,
    #[serde(default)]
    eval_count: Option<u32>,
}

#[derive(Deserialize)]
struct OllamaStreamMessage {
    content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<OllamaToolCall>>,
}
