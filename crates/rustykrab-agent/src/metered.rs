//! A model provider that counts what a run spends and stops it at its
//! token budget: how [`crate::LocalWorker`] enforces a work item's
//! `budget.tokens` (control-layer plan, section 4).
//!
//! Every response's prompt and completion tokens are added to a
//! [`Meter`] shared with the worker. Once the total reaches the budget, the
//! next call is refused with a typed [`RunFailure::Budget`] instead of
//! reaching the model, so the run ends at its next model call with an error
//! the controller classifies as `budget/tokens`. A budget of zero is no
//! limit. Everything else passes straight through to the wrapped provider.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use rustykrab_control::errors::BudgetKind;
use rustykrab_control::worker::RunFailure;
use rustykrab_core::model::{ModelProvider, ModelResponse, StreamEvent, ToolChoice, Usage};
use rustykrab_core::types::{Message, ToolSchema};
use rustykrab_core::Result;

/// Tokens one run has consumed, and its limit.
#[derive(Debug, Default)]
pub struct Meter {
    used: AtomicU64,
    limit: u64,
}

impl Meter {
    /// A meter that refuses further calls once `limit` tokens are used;
    /// `0` never refuses.
    pub fn new(limit: u64) -> Meter {
        Meter {
            used: AtomicU64::new(0),
            limit,
        }
    }

    pub fn used(&self) -> u64 {
        self.used.load(Ordering::Acquire)
    }

    fn add(&self, usage: &Usage) {
        let n = u64::from(usage.prompt_tokens) + u64::from(usage.completion_tokens);
        self.used.fetch_add(n, Ordering::AcqRel);
    }

    /// The refusal for a call made after the budget ran out.
    fn check(&self) -> Result<()> {
        let used = self.used();
        if self.limit > 0 && used >= self.limit {
            return Err(RunFailure::Budget {
                budget: BudgetKind::Tokens,
                detail: format!("{used} of {} tokens used before result_report", self.limit),
            }
            .into_error());
        }
        Ok(())
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

    async fn chat(&self, messages: &[Message], tools: &[ToolSchema]) -> Result<ModelResponse> {
        self.meter.check()?;
        self.count(self.inner.chat(messages, tools).await)
    }

    async fn chat_with_ctx(
        &self,
        messages: &[Message],
        tools: &[ToolSchema],
        num_ctx: u32,
    ) -> Result<ModelResponse> {
        self.meter.check()?;
        self.count(self.inner.chat_with_ctx(messages, tools, num_ctx).await)
    }

    async fn chat_with_choice(
        &self,
        messages: &[Message],
        tools: &[ToolSchema],
        choice: ToolChoice,
    ) -> Result<ModelResponse> {
        self.meter.check()?;
        self.count(self.inner.chat_with_choice(messages, tools, choice).await)
    }

    async fn chat_stream(
        &self,
        messages: &[Message],
        tools: &[ToolSchema],
        on_event: &(dyn Fn(StreamEvent) + Send + Sync),
    ) -> Result<ModelResponse> {
        self.meter.check()?;
        self.count(self.inner.chat_stream(messages, tools, on_event).await)
    }

    async fn chat_stream_with_choice(
        &self,
        messages: &[Message],
        tools: &[ToolSchema],
        choice: ToolChoice,
        on_event: &(dyn Fn(StreamEvent) + Send + Sync),
    ) -> Result<ModelResponse> {
        self.meter.check()?;
        self.count(
            self.inner
                .chat_stream_with_choice(messages, tools, choice, on_event)
                .await,
        )
    }
}
