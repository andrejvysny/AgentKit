//! A deterministic fake [`Embedder`]: each word hashes (FNV-1a) into a
//! bucket of an L2-normalised bag-of-words vector, so texts sharing words
//! get a higher cosine than unrelated ones and every run gives the same
//! bytes. No model, no network.

use agentkit_memory::{Embedder, MemoryResult, ModelId, ModelSpec};
use async_trait::async_trait;

/// Dimensions of the fake model.
pub const HASH_EMBEDDER_DIMS: u32 = 64;

pub struct HashEmbedder {
    spec: ModelSpec,
}

impl HashEmbedder {
    pub fn new() -> Self {
        Self::with_id("hash-bow@test")
    }

    pub fn with_id(id: &str) -> Self {
        Self {
            spec: ModelSpec {
                id: ModelId::new(id),
                dims: HASH_EMBEDDER_DIMS,
                max_tokens: 512,
                query_prefix: String::new(),
                passage_prefix: String::new(),
            },
        }
    }

    fn embed(&self, text: &str) -> Vec<f32> {
        let mut vector = vec![0.0_f32; self.spec.dims as usize];
        for word in text
            .to_lowercase()
            .split(|c: char| !c.is_alphanumeric())
            .filter(|word| !word.is_empty())
        {
            let bucket = fnv1a(word.as_bytes()) as usize % vector.len();
            vector[bucket] += 1.0;
        }
        let norm = vector.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm > 0.0 {
            vector.iter_mut().for_each(|x| *x /= norm);
        }
        vector
    }
}

impl Default for HashEmbedder {
    fn default() -> Self {
        Self::new()
    }
}

fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

#[async_trait]
impl Embedder for HashEmbedder {
    fn spec(&self) -> &ModelSpec {
        &self.spec
    }

    async fn embed_documents(&self, texts: &[String]) -> MemoryResult<Vec<Vec<f32>>> {
        Ok(texts.iter().map(|text| self.embed(text)).collect())
    }

    async fn embed_query(&self, text: &str) -> MemoryResult<Vec<f32>> {
        Ok(self.embed(text))
    }
}
