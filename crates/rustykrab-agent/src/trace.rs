use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Outcome of a single tool execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolTrace {
    pub tool_name: String,
    pub success: bool,
    pub duration: Duration,
    /// Short error message if the call failed.
    pub error: Option<String>,
}

/// Aggregated stats for a single tool across the session.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ToolStats {
    pub calls: u32,
    pub successes: u32,
    pub failures: u32,
    pub total_duration: Duration,
}

impl ToolStats {
    pub fn success_rate(&self) -> f64 {
        if self.calls == 0 {
            return 1.0;
        }
        self.successes as f64 / self.calls as f64
    }

    pub fn avg_duration(&self) -> Duration {
        if self.calls == 0 {
            return Duration::ZERO;
        }
        self.total_duration / self.calls
    }
}

/// Accumulates execution traces for the current agent session.
///
/// This is the core data structure behind Meta-Harness-style optimization:
/// by recording what worked and what didn't, the agent can adapt its
/// strategy mid-conversation (trace-informed tool guidance) and the
/// harness can be tuned offline using historical trace data.
///
/// Security: Each session should get its own ExecutionTracer instance
/// to prevent cross-session information leakage (H8).
#[derive(Debug, Clone)]
pub struct ExecutionTracer {
    inner: Arc<Mutex<TracerInner>>,
}

#[derive(Debug, Default)]
struct TracerInner {
    traces: Vec<ToolTrace>,
    stats: HashMap<String, ToolStats>,
    /// Total iterations the agent has completed.
    iterations: u32,
    /// How many times compression was triggered.
    compressions: u32,
}

/// Sanitize a tool name to prevent prompt injection via trace summaries.
/// Only allows alphanumeric characters, underscores, and hyphens,
/// and limits length to 64 characters.
fn sanitize_tool_name(name: &str) -> String {
    name.chars()
        .filter(|c| c.is_alphanumeric() || *c == '_' || *c == '-')
        .take(64)
        .collect()
}

impl ExecutionTracer {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(TracerInner::default())),
        }
    }

    /// Acquire the inner lock, recovering from poison if needed.
    /// This prevents cascade panics when a thread panics while holding the lock (H9).
    fn lock_inner(&self) -> std::sync::MutexGuard<'_, TracerInner> {
        self.inner.lock().unwrap_or_else(|poisoned| {
            tracing::warn!("tracer mutex was poisoned, recovering");
            poisoned.into_inner()
        })
    }

    /// Record a tool execution outcome.
    ///
    /// Tool names are sanitized before recording to prevent prompt injection
    /// when trace summaries are injected into system prompts.
    pub fn record(&self, trace: ToolTrace) {
        let mut inner = self.lock_inner();
        let sanitized_name = sanitize_tool_name(&trace.tool_name);
        let stats = inner.stats.entry(sanitized_name.clone()).or_default();
        stats.calls += 1;
        stats.total_duration += trace.duration;
        if trace.success {
            stats.successes += 1;
        } else {
            stats.failures += 1;
        }
        let sanitized_trace = ToolTrace {
            tool_name: sanitized_name,
            ..trace
        };
        inner.traces.push(sanitized_trace);
    }

    /// Increment the iteration counter.
    pub fn record_iteration(&self) {
        self.lock_inner().iterations += 1;
    }

    /// Increment the compression counter.
    pub fn record_compression(&self) {
        self.lock_inner().compressions += 1;
    }

    /// Get aggregated stats for all tools.
    pub fn tool_stats(&self) -> HashMap<String, ToolStats> {
        self.lock_inner().stats.clone()
    }

    /// Get the full trace log.
    pub fn traces(&self) -> Vec<ToolTrace> {
        self.lock_inner().traces.clone()
    }

    /// Get tools with a failure rate above the given threshold (0.0–1.0).
    pub fn unreliable_tools(&self, failure_threshold: f64) -> Vec<(String, ToolStats)> {
        self.lock_inner()
            .stats
            .iter()
            .filter(|(_, s)| s.calls >= 2 && s.success_rate() < (1.0 - failure_threshold))
            .map(|(name, stats)| (name.clone(), stats.clone()))
            .collect()
    }

    /// Get the most-used tools, ordered by call count descending.
    pub fn most_used(&self, limit: usize) -> Vec<(String, ToolStats)> {
        let inner = self.lock_inner();
        let mut sorted: Vec<_> = inner
            .stats
            .iter()
            .map(|(n, s)| (n.clone(), s.clone()))
            .collect();
        sorted.sort_by(|a, b| b.1.calls.cmp(&a.1.calls));
        sorted.truncate(limit);
        sorted
    }

    /// Generate a compact text summary of the session's trace data,
    /// suitable for injection into the system prompt.
    pub fn summary_for_prompt(&self) -> Option<String> {
        let inner = self.lock_inner();
        if inner.traces.is_empty() {
            return None;
        }

        let mut lines = Vec::new();
        lines.push("TOOL EXECUTION HISTORY (this session):".to_string());

        // Sort by call count descending.
        let mut sorted: Vec<_> = inner.stats.iter().collect();
        sorted.sort_by(|a, b| b.1.calls.cmp(&a.1.calls));

        for (name, stats) in &sorted {
            let rate = (stats.success_rate() * 100.0) as u32;
            let avg_ms = stats.avg_duration().as_millis();
            let mut line = format!(
                "- {name}: {}/{} succeeded ({rate}%), avg {avg_ms}ms",
                stats.successes, stats.calls,
            );
            if stats.failures > 0 {
                line.push_str(" — has failures");
            }
            lines.push(line);
        }

        // Add specific warnings for unreliable tools.
        let unreliable: Vec<_> = sorted
            .iter()
            .filter(|(_, s)| s.calls >= 2 && s.success_rate() < 0.5)
            .collect();

        if !unreliable.is_empty() {
            lines.push(String::new());
            lines.push("UNRELIABLE TOOLS — consider alternative approaches:".to_string());
            for (name, stats) in unreliable {
                lines.push(format!(
                    "  - {name}: only {:.0}% success rate over {} calls",
                    stats.success_rate() * 100.0,
                    stats.calls,
                ));
            }
        }

        Some(lines.join("\n"))
    }
}

impl Default for ExecutionTracer {
    fn default() -> Self {
        Self::new()
    }
}
