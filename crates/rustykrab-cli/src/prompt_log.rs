//! File-backed [`PromptSink`] for the CLI.
//!
//! Writes one JSON object per line (JSONL) into a daily-rotated file under
//! the data directory's `logs/` folder. Off by default; enable with
//! `RUSTYKRAB_PROMPT_LOG=1`. Prompt content can include user secrets, so
//! the operator opts in deliberately.

use std::io::Write;
use std::path::Path;
use std::sync::{Arc, Mutex};

use rustykrab_core::prompt_trace::{set_sink, PromptRecord, PromptSink};
use tracing_appender::non_blocking::WorkerGuard;

/// Sink that serializes each [`PromptRecord`] as a single JSON line.
///
/// Wraps a [`tracing_appender::non_blocking::NonBlocking`] writer behind
/// a mutex — `NonBlocking` is already MPSC under the hood, but its
/// `Write` impl takes `&mut self`, so we serialize callers here.
struct FilePromptSink {
    writer: Mutex<tracing_appender::non_blocking::NonBlocking>,
}

impl PromptSink for FilePromptSink {
    fn record(&self, record: PromptRecord) {
        let line = match serde_json::to_string(&record) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("failed to serialize prompt record: {e}");
                return;
            }
        };
        let mut writer = match self.writer.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        if let Err(e) = writeln!(*writer, "{line}") {
            // Don't bring down the agent loop on a logging failure.
            tracing::warn!("failed to write prompt record: {e}");
        }
    }
}

/// Init the prompt log when `RUSTYKRAB_PROMPT_LOG=1` is set.
///
/// Returns a [`WorkerGuard`] that must be kept alive for the lifetime of
/// the process — dropping it flushes pending records and shuts the worker
/// down. Returns `None` when the env var is unset, which is the default.
pub fn init(log_dir: &Path) -> Option<WorkerGuard> {
    let enabled = std::env::var("RUSTYKRAB_PROMPT_LOG")
        .map(|v| matches!(v.as_str(), "1" | "true" | "TRUE" | "yes"))
        .unwrap_or(false);
    if !enabled {
        return None;
    }

    let appender = tracing_appender::rolling::daily(log_dir, "prompts.log");
    let (writer, guard) = tracing_appender::non_blocking(appender);
    let sink: Arc<dyn PromptSink> = Arc::new(FilePromptSink {
        writer: Mutex::new(writer),
    });
    set_sink(sink);
    tracing::info!(
        log_dir = %log_dir.display(),
        "prompt log enabled (RUSTYKRAB_PROMPT_LOG=1) — prompts will be written to prompts.log"
    );
    Some(guard)
}
