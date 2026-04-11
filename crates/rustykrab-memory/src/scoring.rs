use std::collections::HashMap;
use uuid::Uuid;

use crate::types::TurnMetadata;

/// A ranked list with associated weight and retrieval source for RRF fusion.
type RankedSourceList = (Vec<(Uuid, usize)>, f64, crate::types::RetrievalSource);

/// Compute heuristic importance score for a piece of content.
///
/// Returns a value in [0.0, 1.0] based on content features:
/// - Named entities (+0.05 each, max +0.25)
/// - Temporal markers (+0.05)
/// - Tool usage (+0.15)
/// - User flagged (+0.30)
///
/// This runs synchronously at write time with zero LLM calls.
pub fn compute_importance(content: &str, metadata: &TurnMetadata) -> f64 {
    let mut score = 0.3; // baseline

    // Named entities: capitalized words not at sentence start.
    let entity_count = count_named_entities(content);
    score += (entity_count as f64).min(5.0) * 0.05;

    // Sentiment intensity as a proxy for emotional significance.
    score += sentiment_intensity(content) * 0.15;

    // Tool usage indicates actionable/significant interaction.
    if metadata.involves_tool_use {
        score += 0.15;
    }

    // Explicit user flagging is the strongest signal.
    if metadata.user_flagged {
        score += 0.30;
    }

    // Temporal markers suggest time-bound information worth tracking.
    if has_temporal_markers(content) {
        score += 0.05;
    }

    score.clamp(0.0, 1.0)
}

/// Count words that look like named entities (capitalized, not at sentence
/// start, not common words).
fn count_named_entities(content: &str) -> usize {
    let mut count = 0;
    for sentence in content.split(['.', '!', '?', '\n']) {
        let words: Vec<&str> = sentence.split_whitespace().collect();
        for (i, word) in words.iter().enumerate() {
            if i == 0 {
                continue; // Skip sentence-initial capitalization.
            }
            let trimmed = word.trim_matches(|c: char| !c.is_alphanumeric());
            if trimmed.len() >= 2 {
                if let Some(first) = trimmed.chars().next() {
                    if first.is_uppercase() {
                        count += 1;
                    }
                }
            }
        }
    }
    count
}

/// Simple sentiment intensity based on the presence of strong words.
/// Returns a value in [0.0, 1.0].
/// Uses word-boundary matching to avoid false positives (#136).
fn sentiment_intensity(content: &str) -> f64 {
    const STRONG_WORDS: &[&str] = &[
        "love",
        "hate",
        "amazing",
        "terrible",
        "critical",
        "urgent",
        "important",
        "essential",
        "never",
        "always",
        "must",
        "definitely",
        "absolutely",
        "crucial",
        "disaster",
        "excellent",
        "perfect",
        "worst",
        "best",
        "emergency",
        "breakthrough",
        "deadline",
        "required",
    ];

    let lower = content.to_lowercase();
    let words: Vec<&str> = lower
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect();
    let matches = STRONG_WORDS.iter().filter(|w| words.contains(w)).count();

    (matches as f64 / 3.0).min(1.0) // 3+ strong words → max intensity
}

/// Check for temporal markers (dates, days, relative time references).
/// Multi-word markers use substring matching; single-word markers use
/// word-boundary matching to avoid false positives like "may" in "maybe" (#138).
fn has_temporal_markers(content: &str) -> bool {
    let lower = content.to_lowercase();

    // Multi-word markers can safely use substring matching.
    const MULTI_WORD_MARKERS: &[&str] = &[
        "next week",
        "last week",
        "next month",
        "this morning",
        "by end of",
        "due date",
    ];
    if MULTI_WORD_MARKERS.iter().any(|m| lower.contains(m)) {
        return true;
    }

    // Single-word markers use word-boundary matching.
    const SINGLE_WORD_MARKERS: &[&str] = &[
        "today",
        "tomorrow",
        "yesterday",
        "monday",
        "tuesday",
        "wednesday",
        "thursday",
        "friday",
        "saturday",
        "sunday",
        "tonight",
        "deadline",
        "scheduled",
        "january",
        "february",
        "march",
        "april",
        "may",
        "june",
        "july",
        "august",
        "september",
        "october",
        "november",
        "december",
    ];

    let words: Vec<&str> = lower
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect();
    SINGLE_WORD_MARKERS.iter().any(|m| words.contains(m))
}

/// Reciprocal Rank Fusion: merge multiple ranked lists into a single
/// scored list.
///
/// `ranked_lists` is a slice of (ranked_doc_ids_with_ranks, weight) tuples.
/// Each inner vec contains (doc_id, rank) where rank is 0-based.
/// `k` is the RRF constant (default 60).
///
/// Formula: `RRF_score(d) = Σ w_r / (k + rank_r(d))`
pub fn rrf_fuse(ranked_lists: &[(Vec<(Uuid, usize)>, f64)], k: f64) -> Vec<(Uuid, f64)> {
    let mut scores: HashMap<Uuid, f64> = HashMap::new();

    for (list, weight) in ranked_lists {
        for &(doc_id, rank) in list {
            *scores.entry(doc_id).or_default() += weight / (k + rank as f64);
        }
    }

    let mut results: Vec<(Uuid, f64)> = scores.into_iter().collect();
    results.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    results
}

/// Track which retrieval sources contributed to each fused result.
pub fn rrf_fuse_with_sources(
    ranked_lists: &[RankedSourceList],
    k: f64,
) -> Vec<(Uuid, f64, Vec<crate::types::RetrievalSource>)> {
    let mut scores: HashMap<Uuid, f64> = HashMap::new();
    let mut sources: HashMap<Uuid, Vec<crate::types::RetrievalSource>> = HashMap::new();

    for (list, weight, source) in ranked_lists {
        for &(doc_id, rank) in list {
            *scores.entry(doc_id).or_default() += weight / (k + rank as f64);
            sources.entry(doc_id).or_default().push(*source);
        }
    }

    let mut results: Vec<_> = scores
        .into_iter()
        .map(|(id, score)| {
            let srcs = sources.remove(&id).unwrap_or_default();
            (id, score, srcs)
        })
        .collect();
    results.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    results
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::TurnMetadata;

    #[test]
    fn test_importance_baseline() {
        let meta = TurnMetadata::default();
        let score = compute_importance("hello world", &meta);
        assert!((score - 0.3).abs() < 0.01);
    }

    #[test]
    fn test_importance_tool_use() {
        let meta = TurnMetadata {
            involves_tool_use: true,
            ..Default::default()
        };
        let score = compute_importance("hello", &meta);
        assert!(score > 0.4);
    }

    #[test]
    fn test_importance_user_flagged() {
        let meta = TurnMetadata {
            user_flagged: true,
            ..Default::default()
        };
        let score = compute_importance("hello", &meta);
        assert!(score >= 0.6);
    }

    #[test]
    fn test_importance_clamp() {
        let meta = TurnMetadata {
            involves_tool_use: true,
            user_flagged: true,
            ..Default::default()
        };
        let content = "This is absolutely critical! John from Microsoft said the deadline for the Project Alpha emergency is tomorrow.";
        let score = compute_importance(content, &meta);
        assert!(score <= 1.0);
        assert!(score > 0.8);
    }

    #[test]
    fn test_rrf_fusion() {
        let id_a = Uuid::new_v4();
        let id_b = Uuid::new_v4();
        let id_c = Uuid::new_v4();

        let lists = vec![
            (vec![(id_a, 0), (id_b, 1), (id_c, 2)], 1.0), // semantic
            (vec![(id_b, 0), (id_c, 1), (id_a, 2)], 1.0), // keyword
        ];

        let results = rrf_fuse(&lists, 60.0);
        assert_eq!(results.len(), 3);
        // id_b should rank highest: rank 1 in semantic + rank 0 in keyword
        // id_b = 1/(60+1) + 1/(60+0) = 0.01639 + 0.01667 = 0.03306
        // id_a = 1/(60+0) + 1/(60+2) = 0.01667 + 0.01613 = 0.03280
        // id_b > id_a
        assert_eq!(results[0].0, id_b);
    }

    #[test]
    fn test_rrf_weighted() {
        let id_a = Uuid::new_v4();
        let id_b = Uuid::new_v4();

        let lists = vec![
            (vec![(id_a, 0)], 1.0),
            (vec![(id_b, 0)], 0.5), // half weight
        ];

        let results = rrf_fuse(&lists, 60.0);
        assert_eq!(results.len(), 2);
        // id_a: 1.0/60 > id_b: 0.5/60
        assert_eq!(results[0].0, id_a);
        assert!(results[0].1 > results[1].1);
    }
}
