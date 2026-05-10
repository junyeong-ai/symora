//! Local embeddings backend (opt-in via the `embeddings` feature).
//!
//! Symora keeps BM25/FTS5 as the default content-search path because it
//! ships with no extra dependencies and works everywhere. Builds with
//! `--features embeddings` get a local ONNX backend (fastembed-rs) and
//! gain `symora search semantic`, which lets agents ask natural-language
//! questions like "where is the retry logic" instead of having to guess
//! exact identifiers.
//!
//! The trait surface is feature-agnostic so the rest of the crate can
//! call into it without `#[cfg(feature = "embeddings")]` everywhere.
//! When the feature is off, [`default_provider`] returns a sentinel
//! that fails fast with a clear, actionable message.

use anyhow::Result;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum EmbeddingError {
    #[error(
        "Semantic search requires the 'embeddings' feature. \
         Reinstall with: cargo install symora --features embeddings"
    )]
    FeatureDisabled,
    #[error("Embedding model failed to load: {0}")]
    ModelLoad(String),
    #[error("Embedding inference failed: {0}")]
    Inference(String),
}

/// Pluggable embedding backend. Implementations stay sync because the
/// only one we ship is ONNX-CPU; async would buy nothing and complicate
/// the call sites in the search command.
pub trait EmbeddingProvider: Send + Sync {
    /// Stable identifier for the model in use, used to invalidate caches
    /// when the user swaps models out.
    fn model_id(&self) -> &str;

    /// Vector dimensionality of the embeddings this provider returns.
    fn dimension(&self) -> usize;

    /// Embed `texts` in one batch. Implementations should handle batch
    /// sizing internally — callers pass everything they have.
    fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbeddingError>;

    fn embed_query(&self, query: &str) -> Result<Vec<f32>, EmbeddingError> {
        let mut v = self.embed_batch(&[query.to_string()])?;
        v.pop()
            .ok_or_else(|| EmbeddingError::Inference("empty result".into()))
    }
}

/// Construct the default provider for the current build.
///
/// - With `--features embeddings`: a fastembed-rs backed `BgeBaseEnV1`
///   provider, which strikes a good balance of accuracy + size for code.
/// - Without the feature: a sentinel that fails on first use.
pub fn default_provider() -> Result<Box<dyn EmbeddingProvider>, EmbeddingError> {
    #[cfg(feature = "embeddings")]
    {
        Ok(Box::new(fastembed_backend::Fastembed::default_model()?))
    }
    #[cfg(not(feature = "embeddings"))]
    {
        Err(EmbeddingError::FeatureDisabled)
    }
}

/// Cosine similarity between two equally-sized vectors.
/// Returns 0.0 when either vector is zero — callers don't have to
/// special-case the warm-up state.
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() {
        return 0.0;
    }
    let mut dot = 0.0_f32;
    let mut na = 0.0_f32;
    let mut nb = 0.0_f32;
    for i in 0..a.len() {
        dot += a[i] * b[i];
        na += a[i] * a[i];
        nb += b[i] * b[i];
    }
    let denom = (na.sqrt() * nb.sqrt()).max(f32::EPSILON);
    dot / denom
}

#[cfg(feature = "embeddings")]
mod fastembed_backend {
    use super::{EmbeddingError, EmbeddingProvider};
    use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};
    use std::sync::Mutex;

    pub struct Fastembed {
        model: Mutex<TextEmbedding>,
        model_id: String,
        dimension: usize,
    }

    impl Fastembed {
        pub fn default_model() -> Result<Self, EmbeddingError> {
            let model_kind = EmbeddingModel::BGEBaseENV15;
            let model = TextEmbedding::try_new(InitOptions::new(model_kind.clone()))
                .map_err(|e| EmbeddingError::ModelLoad(e.to_string()))?;
            Ok(Self {
                model: Mutex::new(model),
                model_id: format!("{model_kind:?}"),
                dimension: 768, // BGE-base-en-v1.5 hidden size
            })
        }
    }

    impl EmbeddingProvider for Fastembed {
        fn model_id(&self) -> &str {
            &self.model_id
        }
        fn dimension(&self) -> usize {
            self.dimension
        }
        fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbeddingError> {
            if texts.is_empty() {
                return Ok(Vec::new());
            }
            let mut model = self
                .model
                .lock()
                .map_err(|_| EmbeddingError::Inference("model mutex poisoned".into()))?;
            let owned: Vec<String> = texts.to_vec();
            model
                .embed(owned, None)
                .map_err(|e| EmbeddingError::Inference(e.to_string()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cosine_identity_is_one() {
        let v = vec![1.0, 2.0, 3.0];
        let s = cosine(&v, &v);
        assert!((s - 1.0).abs() < 1e-6);
    }

    #[test]
    fn cosine_orthogonal_is_zero() {
        assert!(cosine(&[1.0, 0.0], &[0.0, 1.0]).abs() < 1e-6);
    }

    #[test]
    fn cosine_handles_length_mismatch() {
        assert_eq!(cosine(&[1.0, 0.0, 0.0], &[1.0, 0.0]), 0.0);
    }

    #[test]
    fn cosine_safe_on_zero_vectors() {
        let s = cosine(&[0.0, 0.0], &[1.0, 1.0]);
        assert!(s.abs() < 1.0);
    }

    #[cfg(not(feature = "embeddings"))]
    #[test]
    fn default_provider_without_feature_returns_disabled() {
        match default_provider() {
            Err(EmbeddingError::FeatureDisabled) => {}
            other => panic!("expected FeatureDisabled, got {:?}", other.is_ok()),
        }
    }
}
