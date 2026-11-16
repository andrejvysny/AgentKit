//! How token states become one vector. Pooling is model-specific: reading a
//! CLS-trained model with mean pooling (or the reverse) gives vectors that are
//! close to, but not, the reference embeddings.

use agentkit_memory::{ModelId, ModelSpec};

/// How token states are reduced to one vector.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pooling {
    /// The first token's state (`[CLS]` / `<s>`), whatever the mask says.
    Cls,
    /// The mean of the states of unmasked tokens.
    Mean,
}

/// Model-specific pooling settings, supplied next to the [`ModelSpec`].
///
/// `ModelSpec` (identity, dims, max tokens, prefixes) lives in the memory
/// crate and is stored with the vectors; these settings only affect how this
/// adapter reads the ONNX outputs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolingConfig {
    pub pooling: Pooling,
    /// L2-normalise the pooled vector.
    pub normalize: bool,
    /// Name of the ONNX output to read. `None` picks the first present of
    /// `last_hidden_state`, `token_embeddings`, `sentence_embedding`,
    /// `dense_vecs`, else the first output. A rank-3 output `[batch, tokens,
    /// dims]` is pooled with `pooling`; a rank-2 output `[batch, dims]` is
    /// taken as already pooled (only `normalize` applies).
    pub output: Option<String>,
}

impl PoolingConfig {
    /// BGE-M3 dense embedding: CLS pooling, L2-normalised.
    #[must_use]
    pub fn bge_m3() -> Self {
        Self {
            pooling: Pooling::Cls,
            normalize: true,
            output: None,
        }
    }

    /// Masked mean pooling, L2-normalised (E5/MiniLM style models).
    #[must_use]
    pub fn mean_normalized() -> Self {
        Self {
            pooling: Pooling::Mean,
            normalize: true,
            output: None,
        }
    }
}

/// The BGE-M3 spec: 1024 dims, 8192 tokens, no query/passage prefixes.
/// Callers may lower `max_tokens`; that is part of the stored model identity
/// only through `id`, so use a distinct id if you do.
#[must_use]
pub fn bge_m3_spec() -> ModelSpec {
    ModelSpec {
        id: ModelId::new("bge-m3"),
        dims: 1024,
        max_tokens: 8192,
        query_prefix: String::new(),
        passage_prefix: String::new(),
    }
}

/// Pool one sequence. `hidden` is `[tokens, dims]` row-major and `mask` has
/// one entry per token. Returns `dims` values. An all-masked mean yields zeros.
pub(crate) fn pool_tokens(
    hidden: &[f32],
    mask: &[i64],
    dims: usize,
    config: &PoolingConfig,
) -> Vec<f32> {
    let mut pooled = vec![0.0_f32; dims];
    match config.pooling {
        Pooling::Cls => {
            if let Some(first) = hidden.get(..dims) {
                pooled.copy_from_slice(first);
            }
        }
        Pooling::Mean => {
            let mut kept = 0_u32;
            for (token, keep) in mask.iter().enumerate() {
                if *keep == 0 {
                    continue;
                }
                let Some(row) = hidden.get(token * dims..(token + 1) * dims) else {
                    continue;
                };
                kept += 1;
                pooled.iter_mut().zip(row).for_each(|(slot, v)| *slot += v);
            }
            if kept > 0 {
                pooled.iter_mut().for_each(|x| *x /= kept as f32);
            }
        }
    }
    finish(&mut pooled, config.normalize);
    pooled
}

/// Apply the optional L2 normalisation to an already pooled vector.
pub(crate) fn finish(vector: &mut [f32], normalize: bool) {
    if !normalize {
        return;
    }
    let norm = vector.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        vector.iter_mut().for_each(|x| *x /= norm);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // 3 tokens x 2 dims.
    const HIDDEN: [f32; 6] = [3.0, 4.0, 10.0, 0.0, 100.0, 100.0];

    fn config(pooling: Pooling, normalize: bool) -> PoolingConfig {
        PoolingConfig {
            pooling,
            normalize,
            output: None,
        }
    }

    #[test]
    fn cls_takes_token_zero_regardless_of_mask() {
        for mask in [[1, 1, 1], [1, 1, 0], [0, 0, 0]] {
            let out = pool_tokens(&HIDDEN, &mask, 2, &config(Pooling::Cls, false));
            assert_eq!(out, vec![3.0, 4.0], "mask {mask:?}");
        }
    }

    #[test]
    fn mean_respects_the_mask_and_divides_by_kept_tokens() {
        let out = pool_tokens(&HIDDEN, &[1, 1, 0], 2, &config(Pooling::Mean, false));
        assert_eq!(out, vec![6.5, 2.0]);
        let all = pool_tokens(&HIDDEN, &[1, 1, 1], 2, &config(Pooling::Mean, false));
        assert!((all[0] - 113.0 / 3.0).abs() < 1e-5, "{all:?}");
        let none = pool_tokens(&HIDDEN, &[0, 0, 0], 2, &config(Pooling::Mean, true));
        assert_eq!(none, vec![0.0, 0.0]);
    }

    #[test]
    fn normalisation_gives_unit_length_and_is_optional() {
        let out = pool_tokens(&HIDDEN, &[1, 1, 1], 2, &config(Pooling::Cls, true));
        assert!(
            (out[0] - 0.6).abs() < 1e-6 && (out[1] - 0.8).abs() < 1e-6,
            "{out:?}"
        );
        let norm = out.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-6);
        let raw = pool_tokens(&HIDDEN, &[1, 1, 1], 2, &config(Pooling::Cls, false));
        assert_eq!(raw, vec![3.0, 4.0]);
    }

    #[test]
    fn bge_m3_is_cls_normalised_without_prefixes() {
        let config = PoolingConfig::bge_m3();
        assert_eq!((config.pooling, config.normalize), (Pooling::Cls, true));
        let spec = bge_m3_spec();
        assert_eq!((spec.dims, spec.max_tokens), (1024, 8192));
        assert!(spec.query_prefix.is_empty() && spec.passage_prefix.is_empty());
    }
}
