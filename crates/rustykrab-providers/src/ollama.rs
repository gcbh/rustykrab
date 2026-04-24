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
    /// Explicit context-window size to send to the Ollama server as
    /// `options.num_ctx`. When `None` (the default), the value is omitted
    /// from the request so the server's own configuration is used — e.g.
    /// its `OLLAMA_CONTEXT_LENGTH` env var or the per-model default.
    /// Set `OLLAMA_NUM_CTX` on the client to force a specific override.
    pub num_ctx: Option<u32>,
    /// Number of parallel inference slots.
    pub num_parallel: u32,
    /// Top-p nucleus sampling threshold.
    pub top_p: f32,
    /// Number of tokens to predict (-1 = unlimited, 0 = fill context).
    pub num_predict: i32,
    /// Enable thinking mode for models that support it (e.g. Gemma 4).
    /// When enabled, the model produces `<think>…</think>` reasoning
    /// blocks before its answer, improving tool-calling accuracy.
    pub think: bool,
}

/// Read `num_ctx` from the environment. Checks `RUSTYKRAB_NUM_CTX` first
/// (the canonical RustyKrab-namespaced name), then falls back to
/// `OLLAMA_NUM_CTX` for backward compatibility. Returns `None` when
/// neither var is set or parseable, so the request omits `num_ctx` and
/// the Ollama server's own configuration (e.g. `OLLAMA_CONTEXT_LENGTH`)
/// wins.
fn num_ctx_from_env() -> Option<u32> {
    std::env::var("RUSTYKRAB_NUM_CTX")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .or_else(|| {
            std::env::var("OLLAMA_NUM_CTX")
                .ok()
                .and_then(|v| v.parse::<u32>().ok())
        })
}

/// Rough characters-per-token ratio used for client-side context budgeting.
/// Real tokenization varies by model (English prose ≈ 4, code ≈ 3, CJK ≈ 1-2);
/// 4 is a conservative middle ground that errs toward keeping more history.
const CHARS_PER_TOKEN: usize = 4;

/// Per-message overhead (role tag, framing) the server adds on top of content.
const PER_MESSAGE_OVERHEAD_TOKENS: u32 = 4;

/// Tokens reserved for tool schemas in the prompt and other framing the
/// client can't easily measure (chat template, system tool preamble, etc.).
const SAFETY_OVERHEAD_TOKENS: u32 = 2048;

impl Default for OllamaConfig {
    fn default() -> Self {
        Self {
            temperature: 0.1,
            num_ctx: num_ctx_from_env(),
            num_parallel: 6,
            top_p: 0.9,
            num_predict: 8192,
            think: true,
        }
    }
}

impl OllamaConfig {
    /// Configuration optimized for tool-calling tasks (low temperature).
    pub fn tool_calling() -> Self {
        Self {
            temperature: 0.0,
            num_ctx: num_ctx_from_env(),
            num_parallel: 6,
            top_p: 0.9,
            num_predict: 4096,
            think: true,
        }
    }

    /// Configuration for creative drafting (higher temperature).
    pub fn creative() -> Self {
        Self {
            temperature: 0.7,
            num_ctx: num_ctx_from_env(),
            num_parallel: 6,
            top_p: 0.95,
            num_predict: 16384,
            think: true,
        }
    }
}

/// Ollama provider for local models (Gemma, Qwen, Llama, Mistral, etc.).
pub struct OllamaProvider {
    client: reqwest::Client,
    base_url: String,
    model: String,
    config: OllamaConfig,
    /// Model's native context length discovered from `/api/show`.  Used
    /// only as a client-side prompt-trimming budget when the user hasn't
    /// set an explicit `num_ctx`; never sent to the server.
    detected_ctx: Option<u32>,
}

impl OllamaProvider {
    pub fn new(model: impl Into<String>) -> Self {
        // Large prompts can easily take more than five minutes for prompt
        // evaluation on a local GPU, so allow up to 15 minutes per request
        // before we give up.  Can be overridden via `OLLAMA_TIMEOUT_SECS`.
        let timeout_secs = std::env::var("OLLAMA_TIMEOUT_SECS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(900);
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(timeout_secs))
            .connect_timeout(Duration::from_secs(10))
            .build()
            .expect("failed to build HTTP client");
        Self {
            client,
            base_url: "http://localhost:11434".to_string(),
            model: model.into(),
            config: OllamaConfig::default(),
            detected_ctx: None,
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

    /// Get the explicit `num_ctx` that will be sent to the server, if any.
    /// `None` means the server's own configuration is used.
    pub fn num_ctx(&self) -> Option<u32> {
        self.config.num_ctx
    }

    /// Effective context window used for client-side prompt trimming.
    /// Prefers the user's explicit `num_ctx`, then the value detected from
    /// the model via `/api/show`, else `None` (no trimming).
    pub fn effective_ctx(&self) -> Option<u32> {
        self.config.num_ctx.or(self.detected_ctx)
    }

    /// Query Ollama's `/api/show` endpoint for the loaded model's native
    /// context length.  Returns `Ok(None)` if the response shape is
    /// unfamiliar (e.g. an architecture we don't recognize).  Network and
    /// HTTP errors are propagated so the caller can decide how to react.
    pub async fn detect_context_window(&self) -> Result<Option<u32>> {
        let url = format!("{}/api/show", self.base_url);
        let resp = self
            .client
            .post(&url)
            .json(&serde_json::json!({ "model": self.model }))
            .send()
            .await
            .map_err(|e| Error::ModelProvider(format!("failed to query Ollama /api/show: {e}")))?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(Self::map_status_error(status, &body));
        }

        let raw: serde_json::Value = resp.json().await.map_err(|e| {
            Error::ModelProvider(format!("failed to parse /api/show response: {e}"))
        })?;
        Ok(parse_context_length_from_show(&raw))
    }

    /// Detect the model's native context length and cache it for client-side
    /// prompt-trimming purposes.  If the user has set an explicit `num_ctx`
    /// that exceeds the detected value, clamp it down so we don't OOM the
    /// server.  On any failure the cached value stays `None` and a warning
    /// is logged — startup must not fail just because Ollama is momentarily
    /// unreachable.
    pub async fn with_detected_context_window(mut self) -> Self {
        match self.detect_context_window().await {
            Ok(Some(detected)) => {
                self.detected_ctx = Some(detected);
                if let Some(requested) = self.config.num_ctx {
                    if requested > detected {
                        tracing::info!(
                            model = %self.model,
                            requested_num_ctx = requested,
                            detected_num_ctx = detected,
                            "clamping explicit num_ctx to model's native context length"
                        );
                        self.config.num_ctx = Some(detected);
                    } else {
                        tracing::debug!(
                            model = %self.model,
                            num_ctx = requested,
                            detected_num_ctx = detected,
                            "explicit num_ctx fits within model's native context length"
                        );
                    }
                } else {
                    tracing::debug!(
                        model = %self.model,
                        detected_num_ctx = detected,
                        "no explicit num_ctx set; deferring to server while using detected value for client-side trimming"
                    );
                }
            }
            Ok(None) => {
                tracing::warn!(
                    model = %self.model,
                    num_ctx = ?self.config.num_ctx,
                    "could not detect model context length from /api/show"
                );
            }
            Err(e) => {
                tracing::warn!(
                    model = %self.model,
                    num_ctx = ?self.config.num_ctx,
                    error = %e,
                    "failed to query /api/show"
                );
            }
        }
        self
    }

    /// Strip `<think>…</think>` blocks from assistant content so that
    /// model thinking is not re-submitted in conversation history.
    /// Gemma 4 and other thinking models embed reasoning inside these tags;
    /// re-sending them degrades output quality (see model card best practices).
    fn strip_thinking(text: &str) -> String {
        let mut result = String::with_capacity(text.len());
        let mut rest = text;
        while let Some(start) = rest.find("<think>") {
            result.push_str(&rest[..start]);
            match rest[start..].find("</think>") {
                Some(end) => {
                    rest = &rest[start + end + "</think>".len()..];
                }
                None => {
                    // Unclosed <think> tag — drop everything from here.
                    rest = "";
                    break;
                }
            }
        }
        result.push_str(rest);
        result
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
                    MessageContent::Text(text) => {
                        // Strip <think> blocks from assistant messages so
                        // model thinking is not re-submitted in history.
                        let content = if msg.role == Role::Assistant && text.contains("<think>") {
                            let stripped = Self::strip_thinking(text);
                            tracing::debug!(
                                original_len = text.len(),
                                stripped_len = stripped.len(),
                                "stripped thinking blocks from assistant message"
                            );
                            stripped
                        } else {
                            text.clone()
                        };
                        OllamaMessage {
                            role: role.to_string(),
                            content: Some(content),
                            tool_calls: None,
                        }
                    }
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
                            other => serde_json::to_string(other).map_err(Error::Serialization)?,
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

    /// Trim oldest non-system messages until the estimated prompt fits the
    /// budget derived from `total_ctx` minus `num_predict` and a safety margin
    /// for tool schemas / chat-template framing.  Tool-result messages are
    /// dropped along with any preceding orphaned tool-call assistant turn so
    /// the request stays well-formed.  When `total_ctx` is `None` we have no
    /// budget to enforce so messages pass through unchanged.
    fn trim_to_budget(
        messages: Vec<OllamaMessage>,
        total_ctx: Option<u32>,
        num_predict: i32,
    ) -> Vec<OllamaMessage> {
        let Some(total_ctx) = total_ctx else {
            return messages;
        };
        let reserved_output = num_predict.max(0) as u32;
        let budget = total_ctx
            .saturating_sub(reserved_output)
            .saturating_sub(SAFETY_OVERHEAD_TOKENS);

        let total: u32 = messages.iter().map(estimate_message_tokens).sum();
        if total <= budget {
            return messages;
        }

        let system_count = messages.iter().take_while(|m| m.role == "system").count();
        let mut trimmed = messages;
        let mut current = total;
        let mut dropped = 0usize;

        while current > budget && trimmed.len() > system_count {
            let removed = trimmed.remove(system_count);
            current = current.saturating_sub(estimate_message_tokens(&removed));
            dropped += 1;
        }

        // If trimming left a leading orphan tool-result (no preceding
        // assistant tool_call), drop it so Ollama doesn't reject the request.
        while trimmed.len() > system_count && trimmed[system_count].role == "tool" {
            let removed = trimmed.remove(system_count);
            current = current.saturating_sub(estimate_message_tokens(&removed));
            dropped += 1;
        }

        tracing::warn!(
            num_ctx = total_ctx,
            budget,
            estimated_tokens_before = total,
            estimated_tokens_after = current,
            messages_dropped = dropped,
            "trimmed conversation history to fit Ollama context window"
        );

        trimmed
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

    fn context_limit(&self) -> Option<usize> {
        self.effective_ctx().map(|v| v as usize)
    }

    async fn chat(&self, messages: &[Message], tools: &[ToolSchema]) -> Result<ModelResponse> {
        let ollama_messages = Self::build_messages(messages)?;

        // Fix #200: validate non-empty messages.
        if ollama_messages.is_empty() {
            return Err(Error::ModelBadRequest(
                "cannot call Ollama API with an empty message list".into(),
            ));
        }

        let ollama_messages = Self::trim_to_budget(
            ollama_messages,
            self.effective_ctx(),
            self.config.num_predict,
        );

        let ollama_tools = Self::build_tools(tools);

        let mut options = serde_json::json!({
            "temperature": self.config.temperature,
            "top_p": self.config.top_p,
            "num_predict": self.config.num_predict,
        });
        // Only override the server's context length when the user has asked
        // for it explicitly — otherwise leave `num_ctx` out so `OLLAMA_CONTEXT_LENGTH`
        // (or the model default) wins.
        if let Some(num_ctx) = self.config.num_ctx {
            options["num_ctx"] = serde_json::json!(num_ctx);
        }

        let mut body = serde_json::json!({
            "model": self.model,
            "messages": ollama_messages,
            "stream": false,
            "think": self.config.think,
            "options": options,
        });

        if !ollama_tools.is_empty() {
            body["tools"] = serde_json::to_value(&ollama_tools).map_err(Error::Serialization)?;
        }

        tracing::debug!(
            model = %self.model,
            base_url = %self.base_url,
            num_messages = ollama_messages.len(),
            num_ctx = ?self.config.num_ctx,
            "calling Ollama chat API"
        );

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
                    // Don't retry timeouts: retrying the same 99K-token prompt
                    // would just burn another 15-minute budget for the same
                    // failure.  The caller (agent loop) needs to reduce context
                    // or abort before re-trying.
                    if e.is_timeout() {
                        tracing::warn!(
                            model = %self.model,
                            num_messages = ollama_messages.len(),
                            num_ctx = ?self.config.num_ctx,
                            "Ollama request timed out — not retrying (reduce context or raise OLLAMA_TIMEOUT_SECS)"
                        );
                        return Err(Error::ModelProvider(format!(
                            "Ollama request timed out after the configured HTTP timeout. \
                             Reduce prompt size or raise OLLAMA_TIMEOUT_SECS: {e}"
                        )));
                    }
                    last_err = Some(Error::ModelProvider(format!(
                        "failed to connect to Ollama at {}: {e}. Is Ollama running?",
                        self.base_url
                    )));
                    continue;
                }
            };

            let status = resp.status();
            if status.is_success() {
                let raw_body = resp.text().await.map_err(|e| {
                    Error::ModelProvider(format!("failed to read Ollama response body: {e}"))
                })?;
                let ollama_resp: OllamaResponse = serde_json::from_str(&raw_body).map_err(|e| {
                    Error::ModelProvider(format!("failed to parse Ollama response: {e}"))
                })?;
                let response = Self::parse_response(ollama_resp)?;

                // Debug: dump raw response when message text is empty
                // despite having completion tokens.
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
                        "empty message text with completion tokens — dumping raw response"
                    );
                    tracing::warn!(raw_body = %raw_body, "raw Ollama API response");
                }

                return Ok(response);
            }

            let error_body = resp.text().await.unwrap_or_default();
            tracing::warn!(
                status = %status,
                num_ctx = ?self.config.num_ctx,
                num_messages = ollama_messages.len(),
                error_body = %error_body,
                "Ollama API error"
            );
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

        let ollama_messages = Self::trim_to_budget(
            ollama_messages,
            self.effective_ctx(),
            self.config.num_predict,
        );

        let ollama_tools = Self::build_tools(tools);

        let mut options = serde_json::json!({
            "temperature": self.config.temperature,
            "top_p": self.config.top_p,
            "num_predict": self.config.num_predict,
        });
        if let Some(num_ctx) = self.config.num_ctx {
            options["num_ctx"] = serde_json::json!(num_ctx);
        }

        let mut body = serde_json::json!({
            "model": self.model,
            "messages": ollama_messages,
            "stream": true,
            "think": self.config.think,
            "options": options,
        });

        if !ollama_tools.is_empty() {
            body["tools"] = serde_json::to_value(&ollama_tools).map_err(Error::Serialization)?;
        }

        tracing::debug!(
            model = %self.model,
            base_url = %self.base_url,
            num_messages = ollama_messages.len(),
            num_ctx = ?self.config.num_ctx,
            "calling Ollama chat API (streaming)"
        );

        let url = format!("{}/api/chat", self.base_url);

        // Retry the initial connection with the same backoff as the non-streaming path.
        let mut last_err = None;
        let mut resp = None;
        for attempt in 0..=MAX_RETRIES {
            if attempt > 0 {
                let delay = RETRY_BASE_DELAY * 2u32.pow(attempt - 1);
                tracing::warn!(attempt, "retrying Ollama streaming API after {delay:?}");
                tokio::time::sleep(delay).await;
            }

            let r = match self.client.post(&url).json(&body).send().await {
                Ok(r) => r,
                Err(e) => {
                    // Don't retry timeouts — see `chat` for rationale.
                    if e.is_timeout() {
                        tracing::warn!(
                            model = %self.model,
                            num_messages = ollama_messages.len(),
                            num_ctx = ?self.config.num_ctx,
                            "Ollama streaming request timed out — not retrying"
                        );
                        return Err(Error::ModelProvider(format!(
                            "Ollama streaming request timed out after the configured HTTP timeout. \
                             Reduce prompt size or raise OLLAMA_TIMEOUT_SECS: {e}"
                        )));
                    }
                    last_err = Some(Error::ModelProvider(format!(
                        "failed to connect to Ollama at {}: {e}. Is Ollama running?",
                        self.base_url
                    )));
                    continue;
                }
            };

            let status = r.status();
            if !status.is_success() {
                let error_body = r.text().await.unwrap_or_default();
                tracing::warn!(
                    status = %status,
                    num_ctx = ?self.config.num_ctx,
                    num_messages = ollama_messages.len(),
                    error_body = %error_body,
                    "Ollama streaming API error"
                );
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

        // Parse newline-delimited JSON chunks.
        let mut buffer = String::new();
        let mut full_text = String::new();
        let mut tool_calls: Vec<ToolCall> = Vec::new();
        let mut prompt_eval_count: u32 = 0;
        let mut eval_count: u32 = 0;
        let mut done_reason: Option<String> = None;

        let mut response = resp;
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
                        base_url = %self.base_url,
                        chunks_received,
                        bytes_received,
                        elapsed_ms = elapsed.as_millis() as u64,
                        accumulated_text_len = full_text.len(),
                        buffer_len = buffer.len(),
                        "stream read error mid-stream, returning partial response"
                    );
                    break;
                }
            };
            chunks_received += 1;
            bytes_received += chunk.len() as u64;
            buffer.push_str(&String::from_utf8_lossy(&chunk));

            while let Some(newline_pos) = buffer.find('\n') {
                let line = buffer[..newline_pos].trim().to_string();
                buffer = buffer[newline_pos + 1..].to_string();

                if line.is_empty() {
                    continue;
                }

                let stream_chunk: OllamaStreamChunk = serde_json::from_str(&line).map_err(|e| {
                    Error::ModelProvider(format!("failed to parse Ollama stream chunk: {e}"))
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

        let stream_elapsed = stream_start.elapsed();
        tracing::debug!(
            chunks_received,
            bytes_received,
            elapsed_ms = stream_elapsed.as_millis() as u64,
            text_len = full_text.len(),
            "Ollama stream completed"
        );

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

        // Debug: dump response info when message text is empty
        // despite having completion tokens.
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
                "streaming: empty message text with completion tokens"
            );
        }

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

/// Approximate the number of prompt tokens an `OllamaMessage` will cost.
/// Errs on the high side so trimming converges instead of oscillating.
fn estimate_message_tokens(msg: &OllamaMessage) -> u32 {
    let mut tokens = PER_MESSAGE_OVERHEAD_TOKENS;
    if let Some(c) = &msg.content {
        tokens = tokens.saturating_add(estimate_text_tokens(c));
    }
    if let Some(tcs) = &msg.tool_calls {
        for tc in tcs {
            tokens = tokens.saturating_add(8);
            tokens = tokens.saturating_add(estimate_text_tokens(&tc.function.name));
            tokens =
                tokens.saturating_add(estimate_text_tokens(&tc.function.arguments.to_string()));
        }
    }
    tokens
}

fn estimate_text_tokens(s: &str) -> u32 {
    // chars().count() (not len()) so multibyte characters aren't over-counted.
    let chars = s.chars().count();
    chars.div_ceil(CHARS_PER_TOKEN) as u32
}

/// Pull a context-length value out of a `/api/show` response.  Ollama reports
/// it under `model_info` keyed by architecture (e.g. `llama.context_length`,
/// `qwen3.context_length`).  We accept any key suffixed `.context_length`.
fn parse_context_length_from_show(raw: &serde_json::Value) -> Option<u32> {
    let info = raw.get("model_info")?.as_object()?;

    let arch = raw
        .get("model_info")
        .and_then(|m| m.get("general.architecture"))
        .and_then(|v| v.as_str());

    if let Some(arch) = arch {
        let key = format!("{arch}.context_length");
        if let Some(v) = info.get(&key).and_then(|v| v.as_u64()) {
            return Some(v.min(u32::MAX as u64) as u32);
        }
    }

    // Fallback: any `*.context_length` field.
    for (k, v) in info {
        if k.ends_with(".context_length") {
            if let Some(n) = v.as_u64() {
                return Some(n.min(u32::MAX as u64) as u32);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user_msg(content: &str) -> OllamaMessage {
        OllamaMessage {
            role: "user".to_string(),
            content: Some(content.to_string()),
            tool_calls: None,
        }
    }

    fn system_msg(content: &str) -> OllamaMessage {
        OllamaMessage {
            role: "system".to_string(),
            content: Some(content.to_string()),
            tool_calls: None,
        }
    }

    fn tool_msg(content: &str) -> OllamaMessage {
        OllamaMessage {
            role: "tool".to_string(),
            content: Some(content.to_string()),
            tool_calls: None,
        }
    }

    #[test]
    fn parses_context_length_from_architecture_keyed_field() {
        let raw = serde_json::json!({
            "model_info": {
                "general.architecture": "llama",
                "llama.context_length": 32768u64,
            }
        });
        assert_eq!(parse_context_length_from_show(&raw), Some(32768));
    }

    #[test]
    fn parses_context_length_from_unknown_architecture_via_suffix_match() {
        let raw = serde_json::json!({
            "model_info": {
                "general.architecture": "novel-arch",
                "novel-arch.context_length": 16384u64,
            }
        });
        assert_eq!(parse_context_length_from_show(&raw), Some(16384));
    }

    #[test]
    fn returns_none_when_no_context_length_present() {
        let raw = serde_json::json!({
            "model_info": {
                "general.architecture": "llama",
            }
        });
        assert_eq!(parse_context_length_from_show(&raw), None);
    }

    #[test]
    fn returns_none_when_model_info_missing() {
        let raw = serde_json::json!({});
        assert_eq!(parse_context_length_from_show(&raw), None);
    }

    #[test]
    fn trim_returns_unchanged_when_under_budget() {
        let msgs = vec![system_msg("sys"), user_msg("hi")];
        let original_len = msgs.len();
        let trimmed = OllamaProvider::trim_to_budget(msgs, Some(8192), 1024);
        assert_eq!(trimmed.len(), original_len);
    }

    #[test]
    fn trim_is_noop_when_num_ctx_is_none() {
        // No budget means defer entirely to the server; the client leaves
        // the history untouched.
        let big = "x".repeat(40_000);
        let msgs = vec![system_msg("sys"), user_msg(&big), user_msg("latest")];
        let original_len = msgs.len();
        let trimmed = OllamaProvider::trim_to_budget(msgs, None, 256);
        assert_eq!(trimmed.len(), original_len);
    }

    #[test]
    fn trim_drops_oldest_messages_first_and_preserves_system() {
        // Build a conversation where each user message is ~1000 chars (~250 tokens).
        let big = "x".repeat(4000); // ~1000 tokens
        let msgs = vec![
            system_msg("you are an agent"),
            user_msg(&big),
            user_msg(&big),
            user_msg(&big),
            user_msg(&big),
            user_msg("latest"),
        ];
        // budget = 4096 - 256 - SAFETY_OVERHEAD_TOKENS(2048) = 1792 tokens
        let trimmed = OllamaProvider::trim_to_budget(msgs, Some(4096), 256);
        // System message must survive.
        assert_eq!(trimmed[0].role, "system");
        // Latest message must survive.
        assert_eq!(trimmed.last().unwrap().content.as_deref(), Some("latest"));
        // Some big messages were dropped.
        assert!(trimmed.len() < 6);
    }

    #[test]
    fn trim_drops_orphan_tool_results_after_truncation() {
        let big = "y".repeat(20_000); // ~5000 tokens
        let msgs = vec![
            system_msg("sys"),
            user_msg(&big),
            tool_msg("orphaned tool result"),
            user_msg("latest"),
        ];
        let trimmed = OllamaProvider::trim_to_budget(msgs, Some(4096), 256);
        // Orphan tool result must not become the first non-system message.
        assert_ne!(trimmed.get(1).map(|m| m.role.as_str()), Some("tool"));
        // System and latest user message survive.
        assert_eq!(trimmed[0].role, "system");
        assert_eq!(trimmed.last().unwrap().content.as_deref(), Some("latest"));
    }

    #[test]
    fn default_config_omits_num_ctx_when_env_unset() {
        // Guard: when neither RUSTYKRAB_NUM_CTX nor OLLAMA_NUM_CTX is set,
        // constructors leave num_ctx as None so the server's own
        // OLLAMA_CONTEXT_LENGTH wins.
        //
        // std::env is process-global; restore it after the test so we don't
        // contaminate sibling tests that may set it themselves.
        let saved_ollama = std::env::var("OLLAMA_NUM_CTX").ok();
        let saved_rk = std::env::var("RUSTYKRAB_NUM_CTX").ok();
        // SAFETY: single-threaded section of this test. `cargo test` runs
        // tests on separate threads by default but we're only reading/writing
        // our own var and restoring it.
        unsafe {
            std::env::remove_var("OLLAMA_NUM_CTX");
            std::env::remove_var("RUSTYKRAB_NUM_CTX");
        }
        assert_eq!(OllamaConfig::default().num_ctx, None);
        assert_eq!(OllamaConfig::tool_calling().num_ctx, None);
        assert_eq!(OllamaConfig::creative().num_ctx, None);
        unsafe {
            match saved_ollama {
                Some(v) => std::env::set_var("OLLAMA_NUM_CTX", v),
                None => std::env::remove_var("OLLAMA_NUM_CTX"),
            }
            match saved_rk {
                Some(v) => std::env::set_var("RUSTYKRAB_NUM_CTX", v),
                None => std::env::remove_var("RUSTYKRAB_NUM_CTX"),
            }
        }
    }

    #[test]
    fn estimate_handles_multibyte_characters() {
        // 4 multibyte chars should count as ceil(4/4) = 1 token, not 12 (their byte length).
        let tokens = estimate_text_tokens("日本語x");
        assert_eq!(tokens, 1);
    }
}
