//! Prompt and response tracing — correlate log lines with the prompts that
//! produced them and the responses they returned.
//!
//! Every agent invocation is tagged with a `trace_id` (UUID) that flows
//! through the call stack via a task-local. Logs decorated with `trace_id`
//! line up with rows in the trace log file written by the registered
//! [`TraceSink`].
//!
//! The sink is opt-in: until [`set_sink`] is called the [`record_prompt`]
//! and [`record_response`] helpers are no-ops. The CLI installs a
//! file-backed sink when `RUSTYKRAB_PROMPT_LOG=1` is set, keeping prompts
//! and responses out of the log directory by default since they may
//! contain user-secret material.

use std::sync::Arc;
use std::sync::OnceLock;

use chrono::{DateTime, Utc};
use serde::Serialize;
use uuid::Uuid;

use crate::model::{StopReason, Usage};
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

/// One row in the trace log. Internally tagged via the `kind` field so a
/// reader can distinguish prompt rows from response rows.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TraceRecord {
    /// Outbound submission to the model.
    Prompt {
        trace_id: Uuid,
        timestamp: DateTime<Utc>,
        provider: String,
        model: String,
        /// `true` for streaming submissions, `false` otherwise.
        streaming: bool,
        messages: Vec<Message>,
        tools: Vec<ToolSchema>,
    },
    /// Successful response from the model.
    Response {
        trace_id: Uuid,
        timestamp: DateTime<Utc>,
        provider: String,
        model: String,
        streaming: bool,
        message: Message,
        prompt_tokens: u32,
        completion_tokens: u32,
        cache_read_tokens: u32,
        cache_creation_tokens: u32,
        /// Stringified [`StopReason`] so consumers don't need the core
        /// enum to parse the log.
        stop_reason: String,
        duration_ms: u64,
    },
}

/// Sink that receives [`TraceRecord`] rows.
pub trait TraceSink: Send + Sync {
    fn record(&self, record: TraceRecord);
}

static SINK: OnceLock<Arc<dyn TraceSink>> = OnceLock::new();

/// Install the global trace sink. Only the first call wins — subsequent
/// calls are silently ignored so re-init in tests doesn't panic.
pub fn set_sink(sink: Arc<dyn TraceSink>) {
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
    sink.record(TraceRecord::Prompt {
        trace_id,
        timestamp: Utc::now(),
        provider: provider.to_string(),
        model: model.to_string(),
        streaming,
        messages: messages.to_vec(),
        tools: tools.to_vec(),
    });
}

/// Write a response record to the global sink, tagged with the current
/// trace id. Same no-op semantics as [`record_prompt`].
pub fn record_response(
    provider: &str,
    model: &str,
    streaming: bool,
    message: &Message,
    usage: &Usage,
    stop_reason: &StopReason,
    duration_ms: u64,
) {
    let Some(sink) = SINK.get() else { return };
    let Some(trace_id) = current_trace_id() else {
        return;
    };
    sink.record(TraceRecord::Response {
        trace_id,
        timestamp: Utc::now(),
        provider: provider.to_string(),
        model: model.to_string(),
        streaming,
        message: message.clone(),
        prompt_tokens: usage.prompt_tokens,
        completion_tokens: usage.completion_tokens,
        cache_read_tokens: usage.cache_read_tokens,
        cache_creation_tokens: usage.cache_creation_tokens,
        stop_reason: format!("{stop_reason:?}"),
        duration_ms,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct CapturingSink {
        records: Mutex<Vec<TraceRecord>>,
    }

    impl TraceSink for CapturingSink {
        fn record(&self, record: TraceRecord) {
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
        let record = TraceRecord::Prompt {
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
        match &stored[0] {
            TraceRecord::Prompt { provider, .. } => assert_eq!(provider, "test"),
            _ => panic!("expected Prompt variant"),
        }
    }

    #[test]
    fn trace_record_serializes_with_kind_tag() {
        let prompt = TraceRecord::Prompt {
            trace_id: Uuid::nil(),
            timestamp: chrono::DateTime::from_timestamp(0, 0).unwrap(),
            provider: "p".into(),
            model: "m".into(),
            streaming: false,
            messages: Vec::new(),
            tools: Vec::new(),
        };
        let json = serde_json::to_string(&prompt).unwrap();
        assert!(json.contains("\"kind\":\"prompt\""));
    }
}
