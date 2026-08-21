use async_trait::async_trait;

use crate::error::Result;
use crate::types::{Message, ToolSchema};

/// Response from a model provider.
#[derive(Debug, Clone)]
pub struct ModelResponse {
    pub message: Message,
    pub usage: Usage,
    /// The stop reason from the model — tells us if there are more tool calls.
    pub stop_reason: StopReason,
    /// Text content returned alongside tool calls in a mixed response.
    /// When the model returns both reasoning text and tool_use blocks,
    /// the tool calls go into `message.content` and the text is preserved here.
    pub text: Option<String>,
}

/// Why the model stopped generating.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopReason {
    /// Model finished with a text response.
    EndTurn,
    /// Model wants to use one or more tools.
    ToolUse,
    /// Model hit the max token limit (response may be truncated).
    MaxTokens,
    /// Model refused to generate due to content policy violation.
    ContentPolicy,
}

/// Token usage for a single request.
#[derive(Debug, Clone, Default)]
pub struct Usage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    /// Tokens read from the prompt cache (Anthropic).
    pub cache_read_tokens: u32,
    /// Tokens written into the prompt cache (Anthropic).
    pub cache_creation_tokens: u32,
}

/// Server-reported timing breakdown for a single request.
///
/// Only providers that report it populate this — currently Ollama, whose
/// `/api/chat` response carries nanosecond durations for each phase. The
/// split matters on local deployments: prompt evaluation is the phase that
/// prefix-cache reuse can skip, and `load` is non-zero only when the model
/// had to be pulled back into memory after an idle eviction.
///
/// All values are milliseconds, converted from the provider's native units
/// at parse time so consumers don't have to know which unit a provider uses.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProviderTiming {
    /// Wall time for the whole request as the server measured it.
    pub total_ms: u64,
    /// Time spent loading the model into memory. Non-zero indicates the
    /// model was evicted and reloaded — on a large local model this can
    /// dominate everything else.
    pub load_ms: u64,
    /// Time spent evaluating the prompt. This is the phase that a prefix
    /// cache hit skips.
    pub prompt_eval_ms: u64,
    /// Time spent generating the response tokens.
    pub eval_ms: u64,
}

/// How long one model call took, from both vantage points.
///
/// `wall_ms` is what the client measured around the request — it includes
/// network time and any queuing on the server. `server` is the provider's
/// own breakdown of what it did with that time, when it reports one. The
/// gap between them is where the time went that the provider doesn't
/// account for.
#[derive(Debug, Clone, Copy, Default)]
pub struct RequestTiming {
    pub wall_ms: u64,
    pub server: Option<ProviderTiming>,
}

impl ProviderTiming {
    /// Build from nanosecond fields, the unit Ollama reports. Returns
    /// `None` when the provider omitted every field, so a response with no
    /// timing at all stays distinguishable from one that reported zeros.
    pub fn from_nanos(
        total: Option<u64>,
        load: Option<u64>,
        prompt_eval: Option<u64>,
        eval: Option<u64>,
    ) -> Option<Self> {
        if total.is_none() && load.is_none() && prompt_eval.is_none() && eval.is_none() {
            return None;
        }
        const NANOS_PER_MILLI: u64 = 1_000_000;
        Some(Self {
            total_ms: total.unwrap_or(0) / NANOS_PER_MILLI,
            load_ms: load.unwrap_or(0) / NANOS_PER_MILLI,
            prompt_eval_ms: prompt_eval.unwrap_or(0) / NANOS_PER_MILLI,
            eval_ms: eval.unwrap_or(0) / NANOS_PER_MILLI,
        })
    }
}

/// A chunk of a streaming response.
#[derive(Debug, Clone)]
pub enum StreamEvent {
    /// A partial text token.
    TextDelta(String),
    /// Streaming is complete; here's the full response.
    Done(ModelResponse),
}

/// Constraint on whether the model must call a tool on this turn.
///
/// Providers that don't support a native tool_choice parameter fall back
/// to the unconstrained behavior — the runner can still rely on a normal
/// response and re-prompt on the next iteration if it wanted tool use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ToolChoice {
    /// Model decides whether to call a tool.
    #[default]
    Auto,
    /// Model MUST call at least one of the supplied tools (Anthropic: `{"type":"any"}`).
    Any,
}

/// Trait implemented by every model provider (e.g. Anthropic, OpenAI).
#[async_trait]
pub trait ModelProvider: Send + Sync {
    /// Human-readable name of the provider.
    fn name(&self) -> &str;

    /// Model's context window in tokens, when known.
    ///
    /// Serves as the single source of truth for downstream budgets —
    /// compaction thresholds, prompt trimming, memory retrieval sizing.
    /// Providers should derive this from the backing model (e.g. Ollama
    /// reads `/api/show`) or an explicit env-var override. Returning
    /// `None` means "unknown" and callers fall back to their own default.
    fn context_limit(&self) -> Option<usize> {
        None
    }

    /// Whether this provider supports image content in messages.
    fn supports_vision(&self) -> bool {
        false
    }

    /// Whether this provider requires every tool_use block to have a
    /// matching tool_result before the next chat call.
    fn requires_paired_tool_results(&self) -> bool {
        true
    }

    /// Send a conversation to the model and get back the next message.
    async fn chat(&self, messages: &[Message], tools: &[ToolSchema]) -> Result<ModelResponse>;

    /// Send a conversation to the model with an explicit tool-choice constraint.
    ///
    /// Default implementation ignores the constraint and calls [`chat`].
    /// Providers that natively support `tool_choice` (e.g. Anthropic)
    /// should override this to forward the constraint to the API.
    async fn chat_with_choice(
        &self,
        messages: &[Message],
        tools: &[ToolSchema],
        _choice: ToolChoice,
    ) -> Result<ModelResponse> {
        self.chat(messages, tools).await
    }

    /// Stream a response, sending chunks through the callback.
    ///
    /// Default implementation falls back to non-streaming `chat()`.
    /// Providers that support streaming should override this.
    async fn chat_stream(
        &self,
        messages: &[Message],
        tools: &[ToolSchema],
        on_event: &(dyn Fn(StreamEvent) + Send + Sync),
    ) -> Result<ModelResponse> {
        let response = self.chat(messages, tools).await?;
        // Emit the full text as a single event, then done.
        if let Some(text) = response.message.content.as_text() {
            on_event(StreamEvent::TextDelta(text.to_string()));
        }
        on_event(StreamEvent::Done(response.clone()));
        Ok(response)
    }

    /// Stream a response with an explicit tool-choice constraint.
    ///
    /// Default implementation ignores the constraint and calls [`chat_stream`].
    async fn chat_stream_with_choice(
        &self,
        messages: &[Message],
        tools: &[ToolSchema],
        _choice: ToolChoice,
        on_event: &(dyn Fn(StreamEvent) + Send + Sync),
    ) -> Result<ModelResponse> {
        self.chat_stream(messages, tools, on_event).await
    }
}
