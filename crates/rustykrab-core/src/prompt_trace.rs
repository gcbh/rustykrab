//! Prompt tracing — correlate log lines with the prompts that produced them.
//!
//! Every agent invocation is tagged with a `trace_id` (UUID) that flows
//! through the call stack via a task-local. Logs decorated with `trace_id`
//! line up with rows in the prompt log file written by the registered
//! [`PromptSink`].
//!
//! The sink is opt-in: until [`set_sink`] is called the [`record_prompt`]
//! helper is a no-op. The CLI installs a file-backed sink when
//! `RUSTYKRAB_PROMPT_LOG=1` is set, keeping prompts out of the log
//! directory by default since they may contain user-secret material.

use std::sync::Arc;
use std::sync::OnceLock;

use chrono::{DateTime, Utc};
use serde::Serialize;
use uuid::Uuid;

use crate::types::{Message, ToolSchema};

tokio::task_local! {
    /// Trace id seeded at the entry point of an agent run.
    static TRACE_ID: Uuid;
}

/// Returns the trace id active for the current task, if any.
pub fn current_trace_id() -> Option<Uuid> {
    TRACE_ID.try_with(|id| *id).ok()
}

/// Run `fut` with `trace_id` available to [`current_trace_id`] inside its
/// task. Spawned child tasks do not inherit the value — re-scope inside the
/// spawned future if you need it there.
pub async fn with_trace_id<F>(trace_id: Uuid, fut: F) -> F::Output
where
    F: std::future::Future,
{
    TRACE_ID.scope(trace_id, fut).await
}

/// One row in the prompt log: a single submission to a model provider.
#[derive(Debug, Clone, Serialize)]
pub struct PromptRecord {
    pub trace_id: Uuid,
    pub timestamp: DateTime<Utc>,
    pub provider: String,
    pub model: String,
    /// `true` for streaming submissions, `false` otherwise.
    pub streaming: bool,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSchema>,
}

/// Sink that receives [`PromptRecord`] rows.
pub trait PromptSink: Send + Sync {
    fn record(&self, record: PromptRecord);
}

static SINK: OnceLock<Arc<dyn PromptSink>> = OnceLock::new();

/// Install the global prompt sink. Only the first call wins — subsequent
/// calls are silently ignored so re-init in tests doesn't panic.
pub fn set_sink(sink: Arc<dyn PromptSink>) {
    let _ = SINK.set(sink);
}

/// Write a prompt record to the global sink, tagged with the current
/// trace id. No-op when no sink is installed or no trace id is set —
/// callers don't need to guard the call.
pub fn record_prompt(
    provider: &str,
    model: &str,
    streaming: bool,
    messages: &[Message],
    tools: &[ToolSchema],
) {
    let Some(sink) = SINK.get() else { return };
    let Some(trace_id) = current_trace_id() else {
        return;
    };
    sink.record(PromptRecord {
        trace_id,
        timestamp: Utc::now(),
        provider: provider.to_string(),
        model: model.to_string(),
        streaming,
        messages: messages.to_vec(),
        tools: tools.to_vec(),
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct CapturingSink {
        records: Mutex<Vec<PromptRecord>>,
    }

    impl PromptSink for CapturingSink {
        fn record(&self, record: PromptRecord) {
            self.records.lock().unwrap().push(record);
        }
    }

    #[tokio::test]
    async fn current_trace_id_returns_none_without_scope() {
        assert!(current_trace_id().is_none());
    }

    #[tokio::test]
    async fn with_trace_id_makes_id_visible() {
        let id = Uuid::new_v4();
        with_trace_id(id, async move {
            assert_eq!(current_trace_id(), Some(id));
        })
        .await;
    }

    #[tokio::test]
    async fn record_prompt_is_noop_without_trace_id() {
        // No sink installed in this test, but we also can't install one
        // (OnceLock) without affecting other tests. So just check the
        // happy path: outside a scope the helper returns silently.
        record_prompt("test", "test-model", false, &[], &[]);
    }

    #[test]
    fn capturing_sink_collects_records() {
        let sink = Arc::new(CapturingSink {
            records: Mutex::new(Vec::new()),
        });
        let record = PromptRecord {
            trace_id: Uuid::new_v4(),
            timestamp: Utc::now(),
            provider: "test".into(),
            model: "test-model".into(),
            streaming: false,
            messages: Vec::new(),
            tools: Vec::new(),
        };
        sink.record(record.clone());
        let stored = sink.records.lock().unwrap();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].trace_id, record.trace_id);
    }
}
