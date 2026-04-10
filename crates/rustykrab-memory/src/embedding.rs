use async_trait::async_trait;
use rustykrab_core::Result;

/// Trait for generating text embeddings.
///
/// Implementations can wrap fastembed (ONNX), OpenAI API, or any other
/// embedding provider. The trait is async to support both local inference
/// (via `spawn_blocking`) and API-based embedding.
#[async_trait]
pub trait Embedder: Send + Sync {
    /// Embed a batch of texts, returning one vector per input.
    async fn embed(&self, texts: Vec<String>) -> Result<Vec<Vec<f32>>>;

    /// The dimensionality of produced vectors.
    fn dimensions(&self) -> usize;

    /// Model version string for provenance tracking.
    fn model_version(&self) -> &str;
}

/// Cosine similarity between two vectors. Returns 0.0 for zero-norm vectors
/// or mismatched dimensions (#144).
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() {
        tracing::warn!(
            a_len = a.len(),
            b_len = b.len(),
            "cosine_similarity: dimension mismatch, returning 0.0"
        );
        return 0.0;
    }

    let mut dot = 0.0f32;
    let mut norm_a = 0.0f32;
    let mut norm_b = 0.0f32;

    for i in 0..a.len() {
        dot += a[i] * b[i];
        norm_a += a[i] * a[i];
        norm_b += b[i] * b[i];
    }

    let denom = norm_a.sqrt() * norm_b.sqrt();
    if denom < f32::EPSILON {
        0.0
    } else {
        dot / denom
    }
}

/// Find the top-k most similar vectors to `query` from `candidates`.
/// Returns (index, similarity) pairs sorted by descending similarity.
pub fn top_k_similar(
    query: &[f32],
    candidates: &[(uuid::Uuid, Vec<f32>)],
    k: usize,
) -> Vec<(uuid::Uuid, f32)> {
    let mut scores: Vec<(uuid::Uuid, f32)> = candidates
        .iter()
        .map(|(id, vec)| (*id, cosine_similarity(query, vec)))
        .collect();

    // Sort descending by similarity.
    scores.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    scores.truncate(k);
    scores
}

/// A no-op embedder that produces zero vectors. For testing only.
pub struct ZeroEmbedder {
    dims: usize,
}

impl ZeroEmbedder {
    pub fn new(dims: usize) -> Self {
        Self { dims }
    }
}

#[async_trait]
impl Embedder for ZeroEmbedder {
    async fn embed(&self, texts: Vec<String>) -> Result<Vec<Vec<f32>>> {
        Ok(texts.iter().map(|_| vec![0.0f32; self.dims]).collect())
    }

    fn dimensions(&self) -> usize {
        self.dims
    }

    fn model_version(&self) -> &str {
        "zero-embedder-test"
    }
}

/// A deterministic embedder that hashes text content to produce
/// reproducible vectors. Useful for integration tests where you need
/// consistent but non-zero embeddings without a real model.
pub struct HashEmbedder {
    dims: usize,
}

impl HashEmbedder {
    pub fn new(dims: usize) -> Self {
        Self { dims }
    }
}

#[async_trait]
impl Embedder for HashEmbedder {
    async fn embed(&self, texts: Vec<String>) -> Result<Vec<Vec<f32>>> {
        use sha2::{Digest, Sha256};

        Ok(texts
            .iter()
            .map(|text| {
                let mut vec = Vec::with_capacity(self.dims);
                let mut hasher = Sha256::new();
                hasher.update(text.as_bytes());

                // Chain hashes to fill the full dimensionality.
                let mut hash = hasher.finalize().to_vec();
                let mut offset = 0;
                for _ in 0..self.dims {
                    if offset + 4 > hash.len() {
                        let mut h = Sha256::new();
                        h.update(&hash);
                        hash = h.finalize().to_vec();
                        offset = 0;
                    }
                    let bytes: [u8; 4] =
                        hash[offset..offset + 4].try_into().unwrap_or([0; 4]);
                    // Map hash bytes to [-1, 1] deterministically without
                    // producing NaN/Inf (#131). Use integer interpretation
                    // instead of f32::from_bits which can produce NaN/Inf.
                    let int_val = i32::from_le_bytes(bytes);
                    let val = int_val as f64 / i32::MAX as f64;
                    vec.push(val as f32);
                    offset += 4;
                }

                // L2-normalize so cosine similarity is meaningful.
                let norm: f32 = vec.iter().map(|v| v * v).sum::<f32>().sqrt();
                if norm > f32::EPSILON {
                    for v in &mut vec {
                        *v /= norm;
                    }
                }

                vec
            })
            .collect())
    }

    fn dimensions(&self) -> usize {
        self.dims
    }

    fn model_version(&self) -> &str {
        "hash-embedder-test"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cosine_identical() {
        let a = vec![1.0, 2.0, 3.0];
        let sim = cosine_similarity(&a, &a);
        assert!((sim - 1.0).abs() < 1e-6);
    }

    #[test]
    fn test_cosine_orthogonal() {
        let a = vec![1.0, 0.0];
        let b = vec![0.0, 1.0];
        let sim = cosine_similarity(&a, &b);
        assert!(sim.abs() < 1e-6);
    }

    #[test]
    fn test_cosine_opposite() {
        let a = vec![1.0, 0.0];
        let b = vec![-1.0, 0.0];
        let sim = cosine_similarity(&a, &b);
        assert!((sim + 1.0).abs() < 1e-6);
    }

    #[test]
    fn test_zero_vector() {
        let a = vec![0.0, 0.0];
        let b = vec![1.0, 2.0];
        assert_eq!(cosine_similarity(&a, &b), 0.0);
    }

    #[tokio::test]
    async fn test_hash_embedder_deterministic() {
        let embedder = HashEmbedder::new(64);
        let v1 = embedder.embed(vec!["hello".into()]).await.unwrap();
        let v2 = embedder.embed(vec!["hello".into()]).await.unwrap();
        assert_eq!(v1, v2);
    }

    #[tokio::test]
    async fn test_hash_embedder_different_texts() {
        let embedder = HashEmbedder::new(64);
        let vecs = embedder
            .embed(vec!["hello".into(), "world".into()])
            .await
            .unwrap();
        let sim = cosine_similarity(&vecs[0], &vecs[1]);
        // Different texts should produce different (non-identical) vectors.
        assert!(sim < 0.99);
    }
}
