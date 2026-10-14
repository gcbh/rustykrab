//! A model provider that counts what a run spends: how [`crate::LocalWorker`]
//! reports a work item's token use to the controller (control-layer plan,
//! section 4).
//!
//! Every response's prompt and completion tokens are added to a [`Meter`]
//! shared with the worker. Nothing is refused: the meter counts the whole
//! prompt on every call, so a limit on it stopped healthy runs once their
//! history grew (6 of qwen's 30 real failures on 2026-10-12 to 14), while
//! keeping the prompt inside the window is compaction's job. Everything
//! passes straight through to the wrapped provider.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use rustykrab_core::model::{
    ModelCheck, ModelProvider, ModelResponse, StreamEvent, ToolChoice, Usage,
};
use rustykrab_core::types::{Message, ToolSchema};
use rustykrab_core::Result;

/// Tokens one run has consumed.
#[derive(Debug, Default)]
pub struct Meter {
    used: AtomicU64,
}

impl Meter {
    pub fn new() -> Meter {
        Meter::default()
    }

    pub fn used(&self) -> u64 {
        self.used.load(Ordering::Acquire)
    }

    fn add(&self, usage: &Usage) {
        let n = u64::from(usage.prompt_tokens) + u64::from(usage.completion_tokens);
        self.used.fetch_add(n, Ordering::AcqRel);
    }
}

/// `inner`, counted into a [`Meter`].
pub struct MeteredProvider {
    inner: Arc<dyn ModelProvider>,
    meter: Arc<Meter>,
}

impl MeteredProvider {
    pub fn new(inner: Arc<dyn ModelProvider>, meter: Arc<Meter>) -> MeteredProvider {
        MeteredProvider { inner, meter }
    }

    fn count(&self, response: Result<ModelResponse>) -> Result<ModelResponse> {
        if let Ok(r) = &response {
            self.meter.add(&r.usage);
        }
        response
    }
}

#[async_trait]
impl ModelProvider for MeteredProvider {
    fn name(&self) -> &str {
        self.inner.name()
    }

    async fn check_model(&self) -> ModelCheck {
        self.inner.check_model().await
    }

    fn context_limit(&self) -> Option<usize> {
        self.inner.context_limit()
    }

    fn context_limit_for_tools(&self, tools: &[ToolSchema]) -> Option<usize> {
        self.inner.context_limit_for_tools(tools)
    }

    fn total_context_window(&self) -> Option<usize> {
        self.inner.total_context_window()
    }

    fn output_reserve_tokens(&self) -> usize {
        self.inner.output_reserve_tokens()
    }

    fn supports_vision(&self) -> bool {
        self.inner.supports_vision()
    }

    fn requires_paired_tool_results(&self) -> bool {
        self.inner.requires_paired_tool_results()
    }

    // Without this the trait default (`false`) answered for every metered
    // run, so every worker run re-rendered its tools array on a late load
    // and paid a cold prefill, whatever its provider accepts.
    fn accepts_undeclared_tool_calls(&self) -> bool {
        self.inner.accepts_undeclared_tool_calls()
    }

    async fn chat(&self, messages: &[Message], tools: &[ToolSchema]) -> Result<ModelResponse> {
        self.count(self.inner.chat(messages, tools).await)
    }

    async fn chat_with_ctx(
        &self,
        messages: &[Message],
        tools: &[ToolSchema],
        num_ctx: u32,
    ) -> Result<ModelResponse> {
        self.count(self.inner.chat_with_ctx(messages, tools, num_ctx).await)
    }

    async fn chat_with_choice(
        &self,
        messages: &[Message],
        tools: &[ToolSchema],
        choice: ToolChoice,
    ) -> Result<ModelResponse> {
        self.count(self.inner.chat_with_choice(messages, tools, choice).await)
    }

    async fn chat_stream(
        &self,
        messages: &[Message],
        tools: &[ToolSchema],
        on_event: &(dyn Fn(StreamEvent) + Send + Sync),
    ) -> Result<ModelResponse> {
        self.count(self.inner.chat_stream(messages, tools, on_event).await)
    }

    async fn chat_stream_with_choice(
        &self,
        messages: &[Message],
        tools: &[ToolSchema],
        choice: ToolChoice,
        on_event: &(dyn Fn(StreamEvent) + Send + Sync),
    ) -> Result<ModelResponse> {
        self.count(
            self.inner
                .chat_stream_with_choice(messages, tools, choice, on_event)
                .await,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Appends;

    #[async_trait]
    impl ModelProvider for Appends {
        fn name(&self) -> &str {
            "appends"
        }
        fn accepts_undeclared_tool_calls(&self) -> bool {
            true
        }
        async fn chat(&self, _: &[Message], _: &[ToolSchema]) -> Result<ModelResponse> {
            unreachable!("capability data only")
        }
    }

    /// The runner reads late binding from the provider it holds, which in a
    /// worker run is this wrapper.
    #[test]
    fn forwards_the_late_binding_capability() {
        let metered = MeteredProvider::new(Arc::new(Appends), Arc::new(Meter::new()));
        assert!(metered.accepts_undeclared_tool_calls());
    }
}
