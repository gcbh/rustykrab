//! One token estimator, shared by everything that has to guess a size.
//!
//! Five places independently spelled the same `len / 3.5` heuristic: the
//! runner's message estimator, its text estimator, the inverse inside
//! `truncate_summary_to_tokens`, the RLM context manager, and the gateway's
//! turn-to-memory conversion — the last carrying a comment claiming it used
//! "the same estimator the agent runner uses", which nothing enforced.
//!
//! # Know what this is for before you tune it
//!
//! This is a *cheap delta estimator*, not a tokenizer, and the compaction
//! path no longer treats it as ground truth. `predicted_prompt_tokens`
//! anchors on the last response's actual `prompt_tokens + completion_tokens`
//! and uses this heuristic only for the messages appended since — precisely
//! because the heuristic is unreliable in bulk: JSON-heavy history
//! (browser snapshots tokenize nearer 2.5 chars/token) undercounts by ~40%.
//!
//! So the constant is load-bearing for short deltas and deliberately not
//! trusted for whole conversations. Raising it to "be more accurate" on
//! JSON would make it over-estimate ordinary prose, which is the common
//! case; the correct fix for accuracy is a real tokenizer, not a different
//! divisor.
//!
//! # This is the caller-side estimator. Do not fold providers into it.
//!
//! `rustykrab-providers` estimates differently on purpose — Ollama uses
//! `CHARS_PER_TOKEN = 4` for its own budget accounting. The gap is not an
//! oversight. The runner compacts against a budget derived from the
//! provider's reported window, and the provider then trims history against
//! its own; collapsing the two removes headroom between those thresholds,
//! and when the trim budget falls below the compaction threshold, trimming
//! fires first and *deletes* the oldest turns where compaction would have
//! summarised them.
//!
//! `ModelProvider::context_limit_with_tools` addresses that properly by
//! letting a provider report the budget for the tool set actually loaded.
//! Until you have confirmed that path covers every provider in use, treat
//! the two estimators as separate on purpose.

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

/// Estimated tokens for a message body of `bytes`, including the
/// per-message framing overhead.
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

    /// The property the summary cap depends on: a string cut to
    /// `max_bytes_for_tokens(n)` must estimate at or below `n`. Holds for
    /// whatever the constant is, so tuning it cannot invert the relationship.
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
