//! The per-request `tool block sent` log line (see
//! `rustykrab_core::tool_block`), in one place so every provider writes the
//! same shape and one parser reads them all.

use rustykrab_core::tool_block::{display_id, hex, ToolBlockObservation};

/// Log the tool block one request declared. `tool_tokens` is the
/// provider's estimate of the block's size, when it makes one.
pub fn log_tool_block(
    provider: &str,
    seen: &ToolBlockObservation,
    num_tools: usize,
    tool_tokens: Option<u32>,
) {
    tracing::info!(
        provider,
        conversation_id = %display_id(seen.conversation_id),
        trace_id = %display_id(rustykrab_core::prompt_trace::current_trace_id()),
        tool_block = %hex(seen.fingerprint),
        num_tools,
        tool_tokens = tool_tokens.unwrap_or(0),
        changed = seen.changed(),
        "tool block sent"
    );
}
