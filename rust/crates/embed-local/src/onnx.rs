//! The ONNX Runtime engine behind the `onnx` feature.
//!
//! Written against `ort` 2.0.0-rc.15 (`load-dynamic`: the host supplies
//! libonnxruntime via `ORT_DYLIB_PATH`) and `tokenizers` 0.23.2 by reading
//! their sources; NOT YET COMPILED (builds wait for the milestone gate).
//! Pipeline: tokenize (truncate to the model's
//! `max_tokens`), run the session with `input_ids` / `attention_mask`, mean
//! pool the last hidden state over the attention mask, L2-normalise.

use std::sync::{Mutex, OnceLock};

use agentkit_memory::{MemoryError, MemoryResult, ModelSpec};
use ort::environment::Environment;
use ort::session::Session;
use tokenizers::{Tokenizer, TruncationParams};

/// ort allows one environment per process; every engine shares it.
static ENVIRONMENT: OnceLock<Result<Environment, String>> = OnceLock::new();

pub(crate) struct Engine {
    // `Session::run` needs `&mut`.
    session: Mutex<Session>,
    tokenizer: Tokenizer,
    dims: usize,
}

fn backend(what: &str, error: impl std::fmt::Display) -> MemoryError {
    MemoryError::Backend(format!("{what}: {error}"))
}

impl Engine {
    /// Build the engine from bytes the caller already hash-verified.
    pub(crate) fn open(spec: &ModelSpec, model: &[u8], tokenizer: &[u8]) -> MemoryResult<Self> {
        // `build` refuses a second environment; `get_or_init` runs it once.
        let environment = ENVIRONMENT
            .get_or_init(|| ort::init().build().map_err(|error| error.to_string()))
            .as_ref()
            .map_err(|error| backend("onnx environment", error))?;
        let session = Session::builder(environment)
            .and_then(|mut builder| builder.commit_from_memory(model))
            .map_err(|error| backend("onnx session", error))?;
        let mut tokenizer =
            Tokenizer::from_bytes(tokenizer).map_err(|error| backend("tokenizer", error))?;
        tokenizer
            .with_truncation(Some(TruncationParams {
                max_length: spec.max_tokens as usize,
                ..Default::default()
            }))
            .map_err(|error| backend("tokenizer truncation", error))?;
        Ok(Self {
            session: Mutex::new(session),
            tokenizer,
            dims: spec.dims as usize,
        })
    }

    /// One L2-normalised vector of `dims` per text, in order.
    pub(crate) fn embed(&self, texts: &[String]) -> MemoryResult<Vec<Vec<f32>>> {
        let mut out = Vec::with_capacity(texts.len());
        for text in texts {
            let encoding = self
                .tokenizer
                .encode(text.as_str(), true)
                .map_err(|error| backend("tokenize", error))?;
            let ids: Vec<i64> = encoding.get_ids().iter().map(|&id| i64::from(id)).collect();
            let mask: Vec<i64> = encoding
                .get_attention_mask()
                .iter()
                .map(|&m| i64::from(m))
                .collect();
            let length = ids.len();
            let inputs = ort::inputs![
                "input_ids" => ort::value::Tensor::from_array(([1, length], ids))
                    .map_err(|error| backend("input_ids", error))?,
                "attention_mask" => ort::value::Tensor::from_array(([1, length], mask.clone()))
                    .map_err(|error| backend("attention_mask", error))?,
            ];
            let mut session = self
                .session
                .lock()
                .map_err(|_| MemoryError::Backend("onnx session lock poisoned".to_owned()))?;
            let outputs = session
                .run(inputs)
                .map_err(|error| backend("onnx run", error))?;
            let (shape, hidden) = outputs[0]
                .try_extract_tensor::<f32>()
                .map_err(|error| backend("onnx output", error))?;
            if shape.len() != 3 || shape[2] as usize != self.dims {
                return Err(MemoryError::Backend(format!(
                    "model output shape {shape:?} does not match {} dims",
                    self.dims
                )));
            }
            let mut pooled = vec![0.0_f32; self.dims];
            for (token, keep) in mask.iter().enumerate() {
                if *keep == 0 {
                    continue;
                }
                for (slot, value) in pooled
                    .iter_mut()
                    .zip(&hidden[token * self.dims..(token + 1) * self.dims])
                {
                    *slot += value;
                }
            }
            // Mean pooling then L2 normalisation equals L2 of the sum.
            let norm = pooled.iter().map(|x| x * x).sum::<f32>().sqrt();
            if norm > 0.0 {
                pooled.iter_mut().for_each(|x| *x /= norm);
            }
            out.push(pooled);
        }
        Ok(out)
    }
}
