//! Token estimation utility for recursive calls.

/// Estimate token count for a string (conservative: ~3.5 chars/token).
pub fn estimate_tokens(text: &str) -> usize {
    (text.len() as f64 / 3.5).ceil() as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_estimate_tokens() {
        // 35 bytes / 3.5 = 10 tokens
        assert_eq!(estimate_tokens(&"a".repeat(35)), 10);
    }

    #[test]
    fn test_estimate_tokens_empty() {
        assert_eq!(estimate_tokens(""), 0);
    }
}
