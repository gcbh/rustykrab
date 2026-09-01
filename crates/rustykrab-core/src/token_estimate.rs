//! One token estimator, shared by everything that has to guess a size.
//!
//! Five places independently spelled the same `len / 3.5` heuristic: the
//! runner's message estimator, its text estimator, the inverse inside
//! `truncate_summary_to_tokens`, the RLM context manager, and the gateway's
//! turn-to-memory conversion — the last carrying a comment claiming it used
//! "the same estimator the agent runner uses", which nothing enforced.
//!
//! They have to agree. Compaction fires when the conversation crosses a
//! fraction of the context limit, and the summary it produces is capped by
//! the same estimate; if the two drift, compaction either fires too late to
//! prevent an overflow or truncates a summary that did not need it. The
//! gateway's copy sizes the token count stored with every memory, which
//! downstream budgeting reads back as though it were comparable.
//!
//! The constant is deliberately conservative — real tokenizers average
//! closer to 4 bytes per token on English prose, and the gap is the safety
//! margin. Over-estimating costs a slightly early compaction;
//! under-estimating costs a request the provider rejects.
//!
//! # This is the *caller-side* estimator. Do not fold providers into it.
//!
//! `rustykrab-providers` deliberately estimates differently — Ollama uses
//! `CHARS_PER_TOKEN = 4` for its own budget accounting. The ~14% gap is not
//! an oversight, and collapsing it would remove headroom that the compaction
//! path currently depends on.
//!
//! The reason: the runner compacts at a fraction of the provider's reported
//! budget, and the provider then trims history against its own budget. If a
//! large loaded tool set pushes the provider's trim budget below the runner's
//! compaction threshold, trimming fires first and *deletes* the oldest turns
//! outright, where compaction would have summarised them. The estimator gap
//! pushes that crossover several thousand tool-tokens further out.
//!
//! The root cause is addressed properly by `ModelProvider::context_limit_with_tools`,
//! which lets a provider report the budget for the tool set actually loaded
//! rather than an assumed one. Until you have confirmed that path covers
//! every provider in use, treat the two estimators as separate on purpose.

/// Bytes per token.
pub const BYTES_PER_TOKEN: f64 = 3.5;

/// Framing overhead charged per message, for its role and delimiters.
pub const MESSAGE_OVERHEAD_TOKENS: usize = 4;

/// Estimated tokens in a plain string.
pub fn estimate_text_tokens(text: &str) -> usize {
    estimate_bytes(text.len())
}

/// Estimated tokens for a payload of `bytes`.
///
/// For callers that know a serialized size without holding the string — the
/// runner sizes JSON tool arguments with a counting writer rather than
/// materializing them.
pub fn estimate_bytes(bytes: usize) -> usize {
    (bytes as f64 / BYTES_PER_TOKEN).ceil() as usize
}

/// Estimated tokens for a message body of `bytes`, including the per-message
/// framing overhead.
pub fn estimate_message_bytes(bytes: usize) -> usize {
    estimate_bytes(bytes) + MESSAGE_OVERHEAD_TOKENS
}

/// The largest byte length whose estimate stays within `max_tokens`.
///
/// The inverse of [`estimate_bytes`], so a caller truncating to fit a budget
/// cannot drift from the estimator that set the budget.
pub fn max_bytes_for_tokens(max_tokens: usize) -> usize {
    (max_tokens as f64 * BYTES_PER_TOKEN) as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_partial_token_is_still_charged() {
        assert_eq!(estimate_text_tokens(""), 0);
        assert_eq!(estimate_text_tokens("a"), 1);
        assert_eq!(estimate_text_tokens(&"a".repeat(35)), 10);
        assert_eq!(estimate_text_tokens(&"a".repeat(36)), 11);
    }

    #[test]
    fn message_estimate_charges_the_framing_overhead_once() {
        assert_eq!(
            estimate_message_bytes(35),
            estimate_bytes(35) + MESSAGE_OVERHEAD_TOKENS
        );
        assert_eq!(estimate_message_bytes(0), MESSAGE_OVERHEAD_TOKENS);
    }

    /// The property compaction depends on: a string cut to
    /// `max_bytes_for_tokens(n)` must estimate at or below `n`. Holds for
    /// whatever the constant is, so tuning it cannot break the relationship.
    #[test]
    fn truncation_bound_is_the_inverse_of_the_estimate() {
        for tokens in [0usize, 1, 7, 100, 4096, 1_000_000] {
            let bytes = max_bytes_for_tokens(tokens);
            assert!(
                estimate_bytes(bytes) <= tokens,
                "{bytes} bytes estimated above {tokens} tokens"
            );
        }
    }
}
