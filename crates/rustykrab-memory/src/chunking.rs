/// Estimate token count for a text string (~3.5 chars per token).
/// Uses char count instead of byte length for correct CJK handling (#122).
pub fn estimate_tokens(text: &str) -> usize {
    (text.chars().count() as f64 / 3.5).ceil() as usize
}

/// Estimate character count for a given token budget.
fn tokens_to_chars(tokens: usize) -> usize {
    (tokens as f64 * 3.5) as usize
}

/// Snap a byte offset forward to the nearest UTF-8 character boundary.
fn snap_to_char_boundary(s: &str, offset: usize) -> usize {
    let mut pos = offset.min(s.len());
    while pos < s.len() && !s.is_char_boundary(pos) {
        pos += 1;
    }
    pos
}

/// Split text into overlapping chunks of approximately `max_tokens` tokens
/// with `overlap_ratio` fraction of overlap between consecutive chunks.
///
/// Splits prefer natural boundaries (sentence endings, newlines) over
/// hard character cuts. For conversation memory, call `chunk_by_turns`
/// first, then subdivide long turns with this function.
pub fn chunk_text(content: &str, max_tokens: usize, overlap_ratio: f64) -> Vec<String> {
    if content.is_empty() {
        return Vec::new();
    }

    let max_chars = tokens_to_chars(max_tokens);
    let overlap_chars = (max_chars as f64 * overlap_ratio.clamp(0.0, 0.5)) as usize;

    if content.len() <= max_chars {
        return vec![content.to_string()];
    }

    let mut chunks = Vec::new();
    let mut start = 0;

    while start < content.len() {
        let end = snap_to_char_boundary(content, (start + max_chars).min(content.len()));

        // Try to find a natural break point near the end boundary.
        let chunk_end = if end < content.len() {
            find_break_point(content, start, end)
        } else {
            end
        };

        chunks.push(content[start..chunk_end].to_string());

        if chunk_end >= content.len() {
            break;
        }

        // Advance by (chunk_size - overlap), but at least 1 char to avoid infinite loop.
        let advance = (chunk_end - start).saturating_sub(overlap_chars).max(1);
        start = snap_to_char_boundary(content, start + advance);
    }

    chunks
}

/// Find the best break point near `target_end` by looking for sentence
/// boundaries, then newlines, then word boundaries.
fn find_break_point(content: &str, start: usize, target_end: usize) -> usize {
    let search_start = snap_to_char_boundary(content, start + (target_end - start) * 3 / 4);
    let target_end = snap_to_char_boundary(content, target_end);
    if search_start >= target_end {
        return target_end;
    }
    let region = &content[search_start..target_end];

    // Prefer sentence boundaries (. ! ? followed by whitespace).
    if let Some(pos) = region.rfind(". ") {
        return search_start + pos + 2;
    }
    if let Some(pos) = region.rfind("! ") {
        return search_start + pos + 2;
    }
    if let Some(pos) = region.rfind("? ") {
        return search_start + pos + 2;
    }

    // Then newlines.
    if let Some(pos) = region.rfind('\n') {
        return search_start + pos + 1;
    }

    // Then word boundaries.
    if let Some(pos) = region.rfind(' ') {
        return search_start + pos + 1;
    }

    // Fall back to hard cut.
    target_end
}

/// Split conversation text by speaker turn boundaries first, then
/// subdivide long turns. Input format: lines starting with "Speaker: ".
pub fn chunk_by_turns(content: &str, max_tokens: usize, overlap_ratio: f64) -> Vec<String> {
    let mut turns = Vec::new();
    let mut current_turn = String::new();

    for line in content.lines() {
        // Detect speaker change: line starts with a word followed by ": "
        let is_new_turn = line
            .find(": ")
            .map(|pos| {
                let prefix = &line[..pos];
                !prefix.is_empty()
                    && prefix
                        .chars()
                        .all(|c| c.is_alphanumeric() || c == '_' || c == '-')
            })
            .unwrap_or(false);

        if is_new_turn && !current_turn.is_empty() {
            turns.push(std::mem::take(&mut current_turn));
        }
        if !current_turn.is_empty() {
            current_turn.push('\n');
        }
        current_turn.push_str(line);
    }
    if !current_turn.is_empty() {
        turns.push(current_turn);
    }

    // Subdivide any turn that exceeds max_tokens.
    let mut chunks = Vec::new();
    for turn in turns {
        if estimate_tokens(&turn) <= max_tokens {
            chunks.push(turn);
        } else {
            chunks.extend(chunk_text(&turn, max_tokens, overlap_ratio));
        }
    }

    chunks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_estimate_tokens() {
        assert_eq!(estimate_tokens(""), 0);
        assert_eq!(estimate_tokens("hello"), 2); // 5 / 3.5 = 1.43 → ceil = 2
    }

    #[test]
    fn test_short_text_single_chunk() {
        let text = "This is a short text.";
        let chunks = chunk_text(text, 512, 0.15);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0], text);
    }

    #[test]
    fn test_long_text_multiple_chunks() {
        let text = "Hello world. ".repeat(500); // ~6500 chars
        let chunks = chunk_text(&text, 100, 0.15); // ~350 chars per chunk
        assert!(chunks.len() > 1);
        // All content should be covered
        for chunk in &chunks {
            assert!(!chunk.is_empty());
        }
    }

    #[test]
    fn test_chunk_by_turns() {
        let conv = "Alice: Hello, how are you?\nBob: I'm doing well.\nAlice: Great!";
        let chunks = chunk_by_turns(conv, 512, 0.15);
        assert_eq!(chunks.len(), 3);
        assert!(chunks[0].starts_with("Alice: Hello"));
        assert!(chunks[1].starts_with("Bob:"));
        assert!(chunks[2].starts_with("Alice: Great"));
    }
}
