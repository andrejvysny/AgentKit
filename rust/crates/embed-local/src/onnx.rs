//! The ONNX Runtime engine behind the `onnx` feature.
//!
//! Written against `ort` 2.0.0-rc.15 (`load-dynamic`: the host supplies
//! libonnxruntime via `ORT_DYLIB_PATH`) and `tokenizers` 0.23.2.
//! Pipeline: tokenize (truncate to the model's `max_tokens`), pad the batch to
//! its longest sequence, run the session with `input_ids` / `attention_mask`
//! (and zero `token_type_ids` when the model declares them), then pool the
//! selected output as the model's [`PoolingConfig`] says (CLS for BGE-M3).

use std::sync::{Mutex, OnceLock};

use agentkit_memory::{MemoryError, MemoryResult, ModelSpec};
use ort::environment::Environment;
use ort::session::Session;
use tokenizers::{Tokenizer, TruncationParams};

use crate::pooling::{PoolingConfig, finish, pool_tokens};

/// Texts per session run: bounds the padded tensor size.
const BATCH: usize = 16;

/// Output names tried, in order, when the config does not name one.
const OUTPUT_CANDIDATES: [&str; 4] = [
    "last_hidden_state",
    "token_embeddings",
    "sentence_embedding",
    "dense_vecs",
];

/// ort allows one environment per process; every engine shares it.
static ENVIRONMENT: OnceLock<Result<Environment, String>> = OnceLock::new();

pub(crate) struct Engine {
    // `Session::run` needs `&mut`.
    session: Mutex<Session>,
    tokenizer: Tokenizer,
    dims: usize,
    pooling: PoolingConfig,
    pad_id: i64,
    wants_token_types: bool,
    output_name: String,
}

fn backend(what: &str, error: impl std::fmt::Display) -> MemoryError {
    MemoryError::Backend(format!("{what}: {error}"))
}

impl Engine {
    /// Build the engine from bytes the caller already hash-verified.
    pub(crate) fn open(
        spec: &ModelSpec,
        pooling: &PoolingConfig,
        model: &[u8],
        tokenizer: &[u8],
    ) -> MemoryResult<Self> {
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
        let pad_id = tokenizer
            .get_padding()
            .map(|padding| i64::from(padding.pad_id))
            .or_else(|| tokenizer.token_to_id("<pad>").map(i64::from))
            .unwrap_or(0);
        let wants_token_types = session
            .inputs()
            .iter()
            .any(|input| input.name() == "token_type_ids");
        let output_name = select_output(
            session.outputs().iter().map(|output| output.name()),
            pooling.output.as_deref(),
        )?;
        Ok(Self {
            session: Mutex::new(session),
            tokenizer,
            dims: spec.dims as usize,
            pooling: pooling.clone(),
            pad_id,
            wants_token_types,
            output_name,
        })
    }

    /// One vector of `dims` per text, in order (L2-normalised when the
    /// pooling config says so).
    pub(crate) fn embed(&self, texts: &[String]) -> MemoryResult<Vec<Vec<f32>>> {
        let mut out = Vec::with_capacity(texts.len());
        for chunk in texts.chunks(BATCH) {
            out.extend(self.embed_batch(chunk)?);
        }
        Ok(out)
    }

    fn embed_batch(&self, texts: &[String]) -> MemoryResult<Vec<Vec<f32>>> {
        let mut encodings = Vec::with_capacity(texts.len());
        for text in texts {
            encodings.push(
                self.tokenizer
                    .encode(text.as_str(), true)
                    .map_err(|error| backend("tokenize", error))?,
            );
        }
        let width = encodings
            .iter()
            .map(|e| e.get_ids().len())
            .max()
            .unwrap_or(0);
        let rows = encodings.len();
        let mut ids = vec![self.pad_id; rows * width];
        let mut mask = vec![0_i64; rows * width];
        for (row, encoding) in encodings.iter().enumerate() {
            for (column, (&id, &keep)) in encoding
                .get_ids()
                .iter()
                .zip(encoding.get_attention_mask())
                .enumerate()
            {
                ids[row * width + column] = i64::from(id);
                mask[row * width + column] = i64::from(keep);
            }
        }
        let tensor = |name: &str, data: Vec<i64>| {
            ort::value::Tensor::from_array(([rows, width], data))
                .map_err(|error| backend(name, error))
        };
        let mut inputs = ort::inputs![
            "input_ids" => tensor("input_ids", ids)?,
            "attention_mask" => tensor("attention_mask", mask.clone())?,
        ];
        if self.wants_token_types {
            inputs.push((
                "token_type_ids".into(),
                tensor("token_type_ids", vec![0; rows * width])?.into(),
            ));
        }
        let mut session = self
            .session
            .lock()
            .map_err(|_| MemoryError::Backend("onnx session lock poisoned".to_owned()))?;
        let outputs = session
            .run(inputs)
            .map_err(|error| backend("onnx run", error))?;
        let (shape, data) = outputs[self.output_name.as_str()]
            .try_extract_tensor::<f32>()
            .map_err(|error| backend("onnx output", error))?;
        let mismatch = || {
            MemoryError::Backend(format!(
                "model output {} shape {shape:?} does not match {rows}x{width}x{} dims",
                self.output_name, self.dims
            ))
        };
        let dims = self.dims;
        let mut out = Vec::with_capacity(rows);
        match shape.len() {
            3 if shape[0] as usize == rows
                && shape[1] as usize == width
                && shape[2] as usize == dims =>
            {
                for row in 0..rows {
                    let states = &data[row * width * dims..(row + 1) * width * dims];
                    let row_mask = &mask[row * width..(row + 1) * width];
                    out.push(pool_tokens(states, row_mask, dims, &self.pooling));
                }
            }
            // Already pooled by the export (`sentence_embedding`, `dense_vecs`).
            2 if shape[0] as usize == rows && shape[1] as usize == dims => {
                for row in 0..rows {
                    let mut vector = data[row * dims..(row + 1) * dims].to_vec();
                    finish(&mut vector, self.pooling.normalize);
                    out.push(vector);
                }
            }
            _ => return Err(mismatch()),
        }
        Ok(out)
    }
}

/// The output to read: `wanted` if named, else the first candidate present,
/// else the first output.
fn select_output<'a>(
    names: impl Iterator<Item = &'a str>,
    wanted: Option<&str>,
) -> MemoryResult<String> {
    let names: Vec<&str> = names.collect();
    let found = match wanted {
        Some(name) => names.iter().find(|n| **n == name).copied().ok_or_else(|| {
            MemoryError::Backend(format!("model has no output {name:?}; outputs: {names:?}"))
        })?,
        None => OUTPUT_CANDIDATES
            .iter()
            .find_map(|candidate| names.iter().find(|n| *n == candidate).copied())
            .or_else(|| names.first().copied())
            .ok_or_else(|| MemoryError::Backend("model has no outputs".to_owned()))?,
    };
    Ok(found.to_owned())
}

#[cfg(test)]
mod tests {
    use super::select_output;

    #[test]
    fn output_selection_prefers_token_states_and_honours_a_named_output() {
        let names = ["sentence_embedding", "last_hidden_state"];
        assert_eq!(
            select_output(names.into_iter(), None).unwrap(),
            "last_hidden_state"
        );
        assert_eq!(
            select_output(names.into_iter(), Some("sentence_embedding")).unwrap(),
            "sentence_embedding"
        );
        assert!(select_output(names.into_iter(), Some("nope")).is_err());
        assert_eq!(
            select_output(["logits"].into_iter(), None).unwrap(),
            "logits"
        );
        assert!(select_output([].into_iter(), None).is_err());
    }
}
