//! Admission control for the memory write path.
//!
//! Decides *whether* content deserves a memory record at all, before
//! importance scoring decides how it ranks. Without this gate, raw tool
//! output and agent-loop control messages accumulate in the store and —
//! because the importance heuristic rewards length — end up outranking
//! genuine facts (observed live: 91.7% of a 26k-row corpus was
//! `tool_call:`/`tool_result:` dumps before the 2026-08 purge).
//!
//! The gate is deliberately conservative: it rejects only content that is
//! structurally machine output or known agent-loop chatter. Anything that
//! reads like prose is admitted and left to the scorer.

/// Why a piece of content was refused admission.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rejection {
    /// Empty or whitespace-only.
    Empty,
    /// Too short to be a retrievable fact.
    TooShort,
    /// Raw tool output, JSON, or other machine-generated structure.
    MachineOutput,
    /// Agent-loop control chatter (iteration warnings, retry nags, todo dumps).
    LoopControl,
}

impl Rejection {
    /// Human-readable reason, suitable for tool feedback to the model.
    pub fn reason(&self) -> &'static str {
        match self {
            Rejection::Empty => "content is empty",
            Rejection::TooShort => "content is too short to be a useful memory",
            Rejection::MachineOutput => {
                "content looks like raw tool output or serialized data; \
                 save a concise factual statement instead"
            }
            Rejection::LoopControl => {
                "content is agent-loop control chatter, not a fact worth remembering"
            }
        }
    }
}

/// Minimum content length in characters for ASCII-only content. Shorter
/// strings cannot carry a retrievable fact ("ok", "done", "yes").
const MIN_ASCII_CHARS: usize = 8;

/// Minimum content length for content containing non-ASCII characters.
/// Information-dense scripts (CJK) pack a full fact into a few characters,
/// so the ASCII floor would wrongly reject them.
const MIN_ANY_CHARS: usize = 4;

/// Fraction of non-whitespace characters that may be structural symbols
/// (braces, brackets, quotes, colons) before content is treated as
/// serialized data rather than prose.
const MAX_STRUCTURAL_RATIO: f64 = 0.30;

/// Tighter structural bound for content that opens with `{`. Prose almost
/// never starts with a brace, so any real structure past this point marks a
/// truncated/unparseable JSON dump.
const BRACE_LED_STRUCTURAL_RATIO: f64 = 0.15;

/// Prefixes that identify machine-generated content. Matched after trimming
/// leading whitespace.
const MACHINE_PREFIXES: &[&str] = &["tool_call:", "tool_result:"];

/// Prefixes of agent-loop control messages injected by the harness or the
/// agent loop itself. These describe the machinery, not the conversation.
/// Each entry is anchored to the harness's exact wording — a legitimate
/// reply that merely opens with similar words ("Current task list: 1. Book
/// flights…") must not be silently dropped from memory.
const LOOP_CONTROL_PREFIXES: &[&str] = &[
    "Multiple consecutive tool calls have failed",
    "Your response was empty",
    "[Harness observation]",
    "[Earlier messages in this conversation were omitted",
    "[CONVERSATION SUMMARY",
    "Current task list (maintained via todo_write",
];

/// Decide whether `content` may enter the memory store.
pub fn admit(content: &str) -> Result<(), Rejection> {
    let trimmed = content.trim();

    if trimmed.is_empty() {
        return Err(Rejection::Empty);
    }
    let char_count = trimmed.chars().count();
    let min_chars = if trimmed.is_ascii() {
        MIN_ASCII_CHARS
    } else {
        MIN_ANY_CHARS
    };
    if char_count < min_chars {
        return Err(Rejection::TooShort);
    }

    for prefix in MACHINE_PREFIXES {
        if trimmed.starts_with(prefix) {
            return Err(Rejection::MachineOutput);
        }
    }
    for prefix in LOOP_CONTROL_PREFIXES {
        if trimmed.starts_with(prefix) {
            return Err(Rejection::LoopControl);
        }
    }
    // The harness's iteration warning ("[Warning: 150/200 iterations
    // used.]") — matched on both halves so a genuine reply that merely
    // opens with "[Warning:" (a surf advisory, say) passes.
    if trimmed.starts_with("[Warning:") && trimmed.contains("iterations used") {
        return Err(Rejection::LoopControl);
    }
    // The bare continuation nudge is exactly "Continue." — prefix-matching
    // it would reject any reply that happens to start with that word.
    if trimmed == "Continue." {
        return Err(Rejection::LoopControl);
    }

    let structural_ratio = {
        let non_ws: Vec<char> = trimmed.chars().filter(|c| !c.is_whitespace()).collect();
        if non_ws.is_empty() {
            0.0
        } else {
            let structural = non_ws
                .iter()
                .filter(|c| matches!(c, '{' | '}' | '[' | ']' | '"' | ':' | ',' | '\\'))
                .count();
            structural as f64 / non_ws.len() as f64
        }
    };

    // Whole-content JSON: a bare object or array is serialized data, not a
    // fact. (Prose that merely *contains* JSON still passes — only content
    // that parses in its entirety is rejected.)
    if trimmed.starts_with('{') || trimmed.starts_with('[') {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(trimmed) {
            if v.is_object() || v.is_array() {
                return Err(Rejection::MachineOutput);
            }
        }
        // Prose essentially never opens with a brace; content that does and
        // still shows real structure (truncated dumps that fail to parse) is
        // serialized data. `[`-led content is exempted from the tighter bound
        // because prose legitimately starts with citations and markdown links.
        if trimmed.starts_with('{') && structural_ratio > BRACE_LED_STRUCTURAL_RATIO {
            return Err(Rejection::MachineOutput);
        }
    }

    // Structure density: serialized data that doesn't parse as JSON
    // (truncated dumps, log fragments) is still dominated by structural
    // symbols in a way prose never is.
    if structural_ratio > MAX_STRUCTURAL_RATIO {
        return Err(Rejection::MachineOutput);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_ordinary_prose() {
        assert!(admit("The Maui trip is May 24-31, two travelers, mid-range budget.").is_ok());
        assert!(admit("User prefers briefings at 7am via Telegram.").is_ok());
    }

    #[test]
    fn accepts_short_but_real_facts() {
        assert!(admit("Budget: $3k").is_ok());
    }

    #[test]
    fn rejects_empty_and_tiny() {
        assert_eq!(admit(""), Err(Rejection::Empty));
        assert_eq!(admit("   \n "), Err(Rejection::Empty));
        assert_eq!(admit("ok"), Err(Rejection::TooShort));
        assert_eq!(admit("done"), Err(Rejection::TooShort));
    }

    #[test]
    fn rejects_tool_dumps() {
        assert_eq!(
            admit("tool_call:gmail({\"action\":\"read\",\"uid\":101827})"),
            Err(Rejection::MachineOutput)
        );
        assert_eq!(
            admit("tool_result:{\"ok\":true,\"summary\":\"...\"}"),
            Err(Rejection::MachineOutput)
        );
    }

    #[test]
    fn rejects_bare_json() {
        assert_eq!(
            admit("{\"count\":0,\"query\":\"weather\",\"results\":[]}"),
            Err(Rejection::MachineOutput)
        );
        assert_eq!(
            admit("[1, 2, 3, 4, 5, 6, 7]"),
            Err(Rejection::MachineOutput)
        );
    }

    #[test]
    fn accepts_prose_containing_json() {
        assert!(admit("The API returned {\"ok\": true} which confirms the fix worked.").is_ok());
    }

    #[test]
    fn rejects_truncated_json_by_density() {
        assert_eq!(
            admit("{\"empty\":false,\"matches\":[{\"byte_offset\":12074,\"line_number\":21,"),
            Err(Rejection::MachineOutput)
        );
    }

    #[test]
    fn rejects_loop_control() {
        assert_eq!(
            admit("Multiple consecutive tool calls have failed.\nRecent errors: ..."),
            Err(Rejection::LoopControl)
        );
        assert_eq!(
            admit("Your response was empty. Please provide a substantive answer."),
            Err(Rejection::LoopControl)
        );
        assert_eq!(
            admit("[Warning: 150/200 iterations used.]"),
            Err(Rejection::LoopControl)
        );
        assert_eq!(
            admit("Current task list (maintained via todo_write):\n[~] Email Review"),
            Err(Rejection::LoopControl)
        );
    }

    #[test]
    fn harness_lookalikes_from_real_replies_are_admitted() {
        // A genuine answer that merely opens with harness-like words must
        // not be silently dropped from memory.
        assert!(admit("Current task list: 1. Book Maui flights 2. Reserve the condo").is_ok());
        assert!(admit("[Warning: high surf advisory for the north shore] Waves at 20ft.").is_ok());
        assert!(admit("Continue. The next step is to book the flights.").is_ok());
        // The harness's exact forms are still rejected.
        assert_eq!(admit("Continue."), Err(Rejection::LoopControl));
        assert_eq!(
            admit("[Warning: 150/200 iterations used.]"),
            Err(Rejection::LoopControl)
        );
    }

    #[test]
    fn short_non_ascii_facts_are_admitted() {
        // Information-dense scripts pack a fact into a few characters.
        assert!(admit("予算は3千ドル").is_ok());
        assert_eq!(admit("ok"), Err(Rejection::TooShort));
        assert_eq!(admit("done"), Err(Rejection::TooShort));
    }
}
