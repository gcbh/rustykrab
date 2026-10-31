//! A model provider that counts what a run spends: how [`crate::LocalWorker`]
//! reports a work item's token use to the controller (control-layer plan,
//! section 4).
//!
//! Counts prompt and completion tokens for observations. Before requests,
//! only the no-progress recovery boundary is enforced; task token allowances
//! are retired. Provider context windows remain separate physical limits.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use rustykrab_control::{errors::ProviderProblem, progress::StepLedger, worker::RunFailure};
use rustykrab_core::model::{
    CallOptions, ModelCheck, ModelProvider, ModelResponse, StreamEvent, ToolChoice, Usage,
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
    progress: Arc<Mutex<StepLedger>>,
}

impl MeteredProvider {
    pub fn new(
        inner: Arc<dyn ModelProvider>,
        meter: Arc<Meter>,
        progress: Arc<Mutex<StepLedger>>,
    ) -> MeteredProvider {
        MeteredProvider {
            inner,
            meter,
            progress,
        }
    }

    fn before_request(&self) -> Result<()> {
        if let Some(stall) = self
            .progress
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .recovery_due()
        {
            return Err(RunFailure::Model {
                problem: ProviderProblem::Loop,
                detail: stall.detail(),
            }
            .into_error());
        }
        Ok(())
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

    fn local_model_tag(&self) -> Option<&str> {
        self.inner.local_model_tag()
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
        self.before_request()?;
        self.count(self.inner.chat(messages, tools).await)
    }

    async fn chat_with_options(
        &self,
        messages: &[Message],
        tools: &[ToolSchema],
        options: CallOptions,
    ) -> Result<ModelResponse> {
        self.before_request()?;
        self.count(self.inner.chat_with_options(messages, tools, options).await)
    }

    async fn chat_with_ctx(
        &self,
        messages: &[Message],
        tools: &[ToolSchema],
        num_ctx: u32,
    ) -> Result<ModelResponse> {
        self.before_request()?;
        self.count(self.inner.chat_with_ctx(messages, tools, num_ctx).await)
    }

    async fn chat_with_choice(
        &self,
        messages: &[Message],
        tools: &[ToolSchema],
        choice: ToolChoice,
    ) -> Result<ModelResponse> {
        self.before_request()?;
        self.count(self.inner.chat_with_choice(messages, tools, choice).await)
    }

    async fn chat_stream(
        &self,
        messages: &[Message],
        tools: &[ToolSchema],
        on_event: &(dyn Fn(StreamEvent) + Send + Sync),
    ) -> Result<ModelResponse> {
        self.before_request()?;
        self.count(self.inner.chat_stream(messages, tools, on_event).await)
    }

    async fn chat_stream_with_choice(
        &self,
        messages: &[Message],
        tools: &[ToolSchema],
        choice: ToolChoice,
        on_event: &(dyn Fn(StreamEvent) + Send + Sync),
    ) -> Result<ModelResponse> {
        self.before_request()?;
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
            Ok(ModelResponse {
                message: Message::stamped(
                    rustykrab_core::types::Role::Assistant,
                    rustykrab_core::types::MessageContent::Text("measured".into()),
                ),
                usage: Usage {
                    prompt_tokens: 90,
                    completion_tokens: 10,
                    ..Default::default()
                },
                stop_reason: rustykrab_core::model::StopReason::EndTurn,
                text: None,
            })
        }
    }

    /// The runner reads late binding from the provider it holds, which in a
    /// worker run is this wrapper.
    #[test]
    fn forwards_the_late_binding_capability() {
        let metered = MeteredProvider::new(
            Arc::new(Appends),
            Arc::new(Meter::new()),
            Arc::new(Mutex::new(StepLedger::new(6))),
        );
        assert!(metered.accepts_undeclared_tool_calls());
    }

    #[tokio::test]
    async fn all_request_paths_keep_counting_after_historical_usage() {
        let meter = Arc::new(Meter::new());
        meter.add(&Usage {
            prompt_tokens: 90,
            completion_tokens: 10,
            ..Default::default()
        });
        let provider = MeteredProvider::new(
            Arc::new(Appends),
            meter.clone(),
            Arc::new(Mutex::new(StepLedger::new(6))),
        );
        let sink = |_: StreamEvent| {};
        let outcomes = [
            provider.chat(&[], &[]).await,
            provider
                .chat_with_options(&[], &[], CallOptions::default())
                .await,
            provider.chat_with_ctx(&[], &[], 4096).await,
            provider.chat_with_choice(&[], &[], ToolChoice::Auto).await,
            provider.chat_stream(&[], &[], &sink).await,
            provider
                .chat_stream_with_choice(&[], &[], ToolChoice::Auto, &sink)
                .await,
        ];
        for result in outcomes {
            assert!(result.is_ok(), "{result:?}");
        }
        assert_eq!(
            meter.used(),
            700,
            "all six calls count; old usage stops none"
        );
    }

    #[test]
    fn worker_autonomy_zero_token_limit_remains_unbounded_and_progress_resets() {
        let meter = Arc::new(Meter::new());
        meter.add(&Usage {
            prompt_tokens: 100_000,
            ..Default::default()
        });
        let ledger = Arc::new(Mutex::new(StepLedger::new(1)));
        let provider = MeteredProvider::new(Arc::new(Appends), meter, ledger.clone());
        assert!(provider.before_request().is_ok());
        for _ in 0..3 {
            ledger
                .lock()
                .unwrap()
                .record("read", &serde_json::json!({}), "same");
        }
        assert!(matches!(
            RunFailure::from_error(&provider.before_request().unwrap_err()),
            Some(RunFailure::Model {
                problem: ProviderProblem::Loop,
                ..
            })
        ));
        ledger
            .lock()
            .unwrap()
            .record("read", &serde_json::json!({"path":"new"}), "new evidence");
        assert!(provider.before_request().is_ok());
    }
}
