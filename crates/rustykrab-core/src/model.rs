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

/// A chunk of a streaming response.
#[derive(Debug, Clone)]
pub enum StreamEvent {
    /// A partial text token.
    TextDelta(String),
    /// Streaming is complete; here's the full response.
    Done(ModelResponse),
}

/// Trait implemented by every model provider (e.g. Anthropic, OpenAI).
#[async_trait]
pub trait ModelProvider: Send + Sync {
    /// Human-readable name of the provider.
    fn name(&self) -> &str;

    /// Send a conversation to the model and get back the next message.
    async fn chat(&self, messages: &[Message], tools: &[ToolSchema]) -> Result<ModelResponse>;

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
}
