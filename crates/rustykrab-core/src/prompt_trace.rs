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

    use crate::types::{MessageContent, Role};

    #[derive(Default)]
    struct CapturingSink {
        records: Mutex<Vec<TraceRecord>>,
    }

    impl TraceSink for CapturingSink {
        fn record(&self, record: TraceRecord) {
            self.records.lock().unwrap().push(record);
        }
    }

    /// `SINK` is a process-wide `OnceLock`, so the tests below share one
    /// capturing sink for the whole test binary. Each test scopes its own
    /// trace id and reads back only its own rows, which keeps them
    /// independent despite the shared install.
    static CAPTURED: OnceLock<Arc<CapturingSink>> = OnceLock::new();

    fn install_sink() -> Arc<CapturingSink> {
        let sink = CAPTURED
            .get_or_init(|| Arc::new(CapturingSink::default()))
            .clone();
        set_sink(sink.clone() as Arc<dyn TraceSink>);
        sink
    }

    fn rows_for(trace_id: Uuid) -> Vec<TraceRecord> {
        install_sink()
            .records
            .lock()
            .unwrap()
            .iter()
            .filter(|r| match r {
                TraceRecord::Prompt { trace_id: t, .. }
                | TraceRecord::Response { trace_id: t, .. } => *t == trace_id,
            })
            .cloned()
            .collect()
    }

    fn msg(text: &str) -> Message {
        Message {
            id: Uuid::new_v4(),
            role: Role::User,
            content: MessageContent::Text(text.to_string()),
            created_at: Utc::now(),
            agent_version: None,
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
    async fn record_prompt_reaches_the_sink_tagged_with_the_active_trace_id() {
        install_sink();
        let id = Uuid::new_v4();

        with_trace_id(id, async {
            record_prompt("ollama", "gemma4:26b", true, &[msg("hello")], &[]);
        })
        .await;

        let rows = rows_for(id);
        assert_eq!(rows.len(), 1, "expected exactly one row for this trace");
        match &rows[0] {
            TraceRecord::Prompt {
                provider,
                model,
                streaming,
                messages,
                ..
            } => {
                assert_eq!(provider, "ollama");
                assert_eq!(model, "gemma4:26b");
                assert!(streaming);
                // The prompt itself must reach the sink — a row that records
                // only metadata would make the trace log useless for its one
                // job, which is correlating a log line with the prompt.
                assert_eq!(messages.len(), 1);
                assert_eq!(messages[0].content.as_text(), Some("hello"));
            }
            other => panic!("expected a Prompt row, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn record_prompt_outside_a_trace_scope_writes_nothing() {
        // Untagged rows cannot be correlated with anything, so they are
        // dropped rather than written with a placeholder id.
        //
        // There is no trace id to filter on here, so the probe carries a
        // unique provider name instead — the sink is shared with the tests
        // running alongside this one, and a bare row count would race them.
        let sink = install_sink();
        let probe = format!("orphan-probe-{}", Uuid::new_v4());

        record_prompt(&probe, "test-model", false, &[msg("orphan")], &[]);

        let wrote_anything = sink.records.lock().unwrap().iter().any(|r| match r {
            TraceRecord::Prompt { provider, .. } | TraceRecord::Response { provider, .. } => {
                provider == &probe
            }
        });
        assert!(!wrote_anything, "an untagged prompt must not be recorded");
    }

    #[tokio::test]
    async fn record_response_carries_usage_and_stop_reason() {
        install_sink();
        let id = Uuid::new_v4();
        let usage = Usage {
            prompt_tokens: 120,
            completion_tokens: 8,
            cache_read_tokens: 100,
            cache_creation_tokens: 4,
        };

        with_trace_id(id, async {
            record_response(
                "anthropic",
                "claude",
                false,
                &msg("the answer"),
                &usage,
                &StopReason::EndTurn,
                1_234,
            );
        })
        .await;

        let rows = rows_for(id);
        assert_eq!(rows.len(), 1);
        match &rows[0] {
            TraceRecord::Response {
                prompt_tokens,
                completion_tokens,
                cache_read_tokens,
                cache_creation_tokens,
                stop_reason,
                duration_ms,
                ..
            } => {
                assert_eq!(*prompt_tokens, 120);
                assert_eq!(*completion_tokens, 8);
                assert_eq!(*cache_read_tokens, 100);
                assert_eq!(*cache_creation_tokens, 4);
                assert_eq!(stop_reason, "EndTurn");
                assert_eq!(*duration_ms, 1_234);
            }
            other => panic!("expected a Response row, got {other:?}"),
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
