use std::collections::HashMap;
use uuid::Uuid;

/// Stopwords list for tokenization (shared with keyword extraction).
const STOPWORDS: &[&str] = &[
    "the", "a", "an", "is", "are", "was", "were", "be", "been", "being", "have",
    "has", "had", "do", "does", "did", "will", "would", "could", "should", "may",
    "might", "shall", "can", "need", "dare", "ought", "used", "to", "of", "in",
    "for", "on", "with", "at", "by", "from", "as", "into", "through", "during",
    "before", "after", "above", "below", "between", "out", "off", "over", "under",
    "again", "further", "then", "once", "here", "there", "when", "where", "why",
    "how", "all", "both", "each", "few", "more", "most", "other", "some", "such",
    "no", "nor", "not", "only", "own", "same", "so", "than", "too", "very", "just",
    "don", "now", "and", "but", "or", "because", "if", "while", "that", "this",
    "these", "those", "what", "which", "who", "whom", "its", "it", "he", "she",
    "they", "them", "his", "her", "their", "our", "your", "my", "me", "we", "you",
    "about", "also", "get", "got", "like", "make", "made", "want", "let", "know",
    "think", "see", "come", "look", "use", "find", "give", "tell", "say", "said",
    "try", "ask", "work", "seem", "feel", "leave", "call", "keep", "put", "show",
    "take",
];

/// Tokenize text into lowercase terms, removing stopwords and short tokens.
pub fn tokenize(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric() && c != '_' && c != '-')
        .map(|w| w.to_lowercase())
        .filter(|w| w.len() >= 2 && !STOPWORDS.contains(&w.as_str()))
        .collect()
}

/// In-process BM25 index for keyword-based retrieval.
///
/// Maintains an inverted index mapping terms to (doc_id, term_frequency)
/// pairs, plus document-level statistics for BM25 scoring.
/// Partitioned by agent_id to prevent cross-agent data leaks.
pub struct Bm25Index {
    /// term → [(doc_id, term_freq)]
    inverted: HashMap<String, Vec<(Uuid, u32)>>,
    /// doc_id → total token count
    doc_lengths: HashMap<Uuid, u32>,
    /// doc_id → agent_id (for agent-scoped search)
    doc_agents: HashMap<Uuid, Uuid>,
    /// Total documents indexed.
    doc_count: u32,
    /// Sum of all doc lengths (for average doc length).
    total_length: u64,
    /// BM25 parameter: term frequency saturation. Typical: 1.2–2.0.
    k1: f64,
    /// BM25 parameter: document length normalization. Typical: 0.75.
    b: f64,
}

impl Bm25Index {
    /// Create a new empty BM25 index with default parameters.
    pub fn new() -> Self {
        Self {
            inverted: HashMap::new(),
            doc_lengths: HashMap::new(),
            doc_agents: HashMap::new(),
            doc_count: 0,
            total_length: 0,
            k1: 1.2,
            b: 0.75,
        }
    }

    /// Index a document with its owning agent. If the document was
    /// previously indexed, it is replaced.
    pub fn index_document(&mut self, doc_id: Uuid, agent_id: Uuid, content: &str) {
        // Remove old entry if re-indexing.
        self.remove_document(doc_id);

        let tokens = tokenize(content);
        let doc_len = tokens.len() as u32;

        // Count term frequencies.
        let mut tf: HashMap<String, u32> = HashMap::new();
        for token in &tokens {
            *tf.entry(token.clone()).or_default() += 1;
        }

        // Insert into inverted index.
        for (term, freq) in tf {
            self.inverted
                .entry(term)
                .or_default()
                .push((doc_id, freq));
        }

        self.doc_lengths.insert(doc_id, doc_len);
        self.doc_agents.insert(doc_id, agent_id);
        self.doc_count += 1;
        self.total_length += doc_len as u64;
    }

    /// Remove a document from the index.
    pub fn remove_document(&mut self, doc_id: Uuid) {
        if let Some(old_len) = self.doc_lengths.remove(&doc_id) {
            self.doc_count = self.doc_count.saturating_sub(1);
            self.total_length = self.total_length.saturating_sub(old_len as u64);
            self.doc_agents.remove(&doc_id);

            // Remove from inverted index entries.
            for postings in self.inverted.values_mut() {
                postings.retain(|(id, _)| *id != doc_id);
            }
            // Clean up empty posting lists.
            self.inverted.retain(|_, v| !v.is_empty());
        }
    }

    /// Search for documents matching the query, scoped to a single agent.
    /// Returns (doc_id, bm25_score) sorted by descending score, at most `limit` results.
    pub fn search(&self, query: &str, agent_id: Uuid, limit: usize) -> Vec<(Uuid, f64)> {
        if self.doc_count == 0 {
            return Vec::new();
        }

        let query_tokens = tokenize(query);
        let avgdl = self.total_length as f64 / self.doc_count.max(1) as f64;

        let mut scores: HashMap<Uuid, f64> = HashMap::new();

        for token in &query_tokens {
            if let Some(postings) = self.inverted.get(token) {
                // IDF: log((N - df + 0.5) / (df + 0.5) + 1)
                let df = postings.len() as f64;
                let n = self.doc_count as f64;
                let idf = ((n - df + 0.5) / (df + 0.5) + 1.0).ln();

                for &(doc_id, tf) in postings {
                    // Filter by agent to prevent cross-agent data leaks.
                    if self.doc_agents.get(&doc_id) != Some(&agent_id) {
                        continue;
                    }
                    let dl = *self.doc_lengths.get(&doc_id).unwrap_or(&0) as f64;
                    let tf_f = tf as f64;

                    // BM25: IDF * (tf * (k1 + 1)) / (tf + k1 * (1 - b + b * dl/avgdl))
                    let numerator = tf_f * (self.k1 + 1.0);
                    let denominator =
                        tf_f + self.k1 * (1.0 - self.b + self.b * dl / avgdl);
                    let bm25 = idf * numerator / denominator;

                    *scores.entry(doc_id).or_default() += bm25;
                }
            }
        }

        let mut results: Vec<(Uuid, f64)> = scores.into_iter().collect();
        results.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        results.truncate(limit);
        results
    }

    /// Remove all documents from the index.
    pub fn clear(&mut self) {
        self.inverted.clear();
        self.doc_lengths.clear();
        self.doc_agents.clear();
        self.doc_count = 0;
        self.total_length = 0;
    }

    /// Number of documents in the index.
    pub fn len(&self) -> usize {
        self.doc_count as usize
    }

    /// Whether the index is empty.
    pub fn is_empty(&self) -> bool {
        self.doc_count == 0
    }
}

impl Default for Bm25Index {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tokenize() {
        let tokens = tokenize("Hello World! This is a test.");
        assert!(tokens.contains(&"hello".to_string()));
        assert!(tokens.contains(&"world".to_string()));
        assert!(tokens.contains(&"test".to_string()));
        // Stopwords removed.
        assert!(!tokens.contains(&"this".to_string()));
        assert!(!tokens.contains(&"is".to_string()));
    }

    #[test]
    fn test_bm25_basic_search() {
        let mut index = Bm25Index::new();
        let agent = Uuid::new_v4();
        let id1 = Uuid::new_v4();
        let id2 = Uuid::new_v4();
        let id3 = Uuid::new_v4();

        index.index_document(id1, agent, "Rust programming language systems");
        index.index_document(id2, agent, "Python programming language scripting");
        index.index_document(id3, agent, "Rust systems programming memory safety");

        let results = index.search("Rust memory", agent, 10);
        assert!(!results.is_empty());
        // id3 mentions both "rust" and "memory" — should rank highest.
        assert_eq!(results[0].0, id3);
    }

    #[test]
    fn test_bm25_agent_isolation() {
        let mut index = Bm25Index::new();
        let agent_a = Uuid::new_v4();
        let agent_b = Uuid::new_v4();
        let id1 = Uuid::new_v4();
        let id2 = Uuid::new_v4();

        index.index_document(id1, agent_a, "Rust programming language");
        index.index_document(id2, agent_b, "Rust programming language");

        // Agent A should only see its own document.
        let results = index.search("Rust", agent_a, 10);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, id1);

        // Agent B should only see its own document.
        let results = index.search("Rust", agent_b, 10);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, id2);
    }

    #[test]
    fn test_bm25_no_results() {
        let mut index = Bm25Index::new();
        let agent = Uuid::new_v4();
        let id1 = Uuid::new_v4();
        index.index_document(id1, agent, "completely unrelated topic");
        let results = index.search("quantum physics", agent, 10);
        assert!(results.is_empty());
    }

    #[test]
    fn test_bm25_remove_document() {
        let mut index = Bm25Index::new();
        let agent = Uuid::new_v4();
        let id1 = Uuid::new_v4();
        index.index_document(id1, agent, "Rust programming");
        assert_eq!(index.len(), 1);

        index.remove_document(id1);
        assert_eq!(index.len(), 0);

        let results = index.search("Rust", agent, 10);
        assert!(results.is_empty());
    }

    #[test]
    fn test_bm25_reindex() {
        let mut index = Bm25Index::new();
        let agent = Uuid::new_v4();
        let id1 = Uuid::new_v4();
        index.index_document(id1, agent, "Rust programming");
        index.index_document(id1, agent, "Python scripting");
        assert_eq!(index.len(), 1);

        let results = index.search("Python", agent, 10);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, id1);

        // Old content should not match.
        let results = index.search("Rust", agent, 10);
        assert!(results.is_empty());
    }
}
