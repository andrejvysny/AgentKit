//! Local embedder (roadmap 3.5, spec "Embeddings"): an ONNX model run in
//! process behind [`agentkit_memory::Embedder`]. The model is never bundled;
//! the owner downloads it and pins the sha-256 of model and tokenizer; the
//! bytes are read once, hashed, and the runtime loads that same buffer. Without the `onnx` cargo feature (the
//! default) [`LocalEmbedder::load`] reports a backend error, so a host that
//! cannot load a model keeps the lexical-only search path.

use std::path::{Path, PathBuf};

use agentkit_memory::{Embedder, MemoryError, MemoryResult, ModelSpec};
use async_trait::async_trait;
use sha2::{Digest, Sha256};

#[cfg(feature = "onnx")]
mod onnx;

/// What the owner supplies: the model's identity and shape, where its files
/// are, and the sha-256 (lowercase hex) of the model and tokenizer files they
/// trust.
#[derive(Debug, Clone)]
pub struct LocalModel {
    pub spec: ModelSpec,
    pub model_path: PathBuf,
    pub model_sha256: String,
    /// The tokenizer file (`tokenizer.json`) shipped with the model.
    pub tokenizer_path: PathBuf,
    pub tokenizer_sha256: String,
}

/// Read `path` once and check the bytes hash to `expected` (lowercase hex,
/// case-insensitive). The returned buffer is what was hashed, so the caller
/// loads exactly the verified bytes (no check-then-reopen race).
pub fn read_verified(path: &Path, expected: &str) -> MemoryResult<Vec<u8>> {
    let bytes = std::fs::read(path)
        .map_err(|error| MemoryError::Backend(format!("model file {}: {error}", path.display())))?;
    let actual: String = Sha256::digest(&bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    if actual.eq_ignore_ascii_case(expected.trim()) {
        Ok(bytes)
    } else {
        Err(MemoryError::Backend(format!(
            "model file {} has sha-256 {actual}, expected {expected}",
            path.display()
        )))
    }
}

/// An [`Embedder`] over a local ONNX model.
pub struct LocalEmbedder {
    spec: ModelSpec,
    #[cfg(feature = "onnx")]
    engine: std::sync::Arc<onnx::Engine>,
}

impl LocalEmbedder {
    /// Verify the pinned hashes, then load the model from the verified
    /// bytes. Fails (never panics) on a missing file, a hash mismatch, or a
    /// build without `onnx`.
    pub fn load(model: LocalModel) -> MemoryResult<Self> {
        let model_bytes = read_verified(&model.model_path, &model.model_sha256)?;
        let tokenizer_bytes = read_verified(&model.tokenizer_path, &model.tokenizer_sha256)?;
        #[cfg(feature = "onnx")]
        {
            let engine = onnx::Engine::open(&model.spec, &model_bytes, &tokenizer_bytes)?;
            Ok(Self {
                spec: model.spec,
                engine: std::sync::Arc::new(engine),
            })
        }
        #[cfg(not(feature = "onnx"))]
        {
            drop((model_bytes, tokenizer_bytes));
            Err(MemoryError::Backend(format!(
                "model {} cannot be loaded: agentkit-embed-local was built without the `onnx` feature",
                model.spec.id
            )))
        }
    }

    fn prefixed(&self, prefix: &str, text: &str) -> String {
        format!("{prefix}{text}")
    }
}

#[async_trait]
impl Embedder for LocalEmbedder {
    fn spec(&self) -> &ModelSpec {
        &self.spec
    }

    async fn embed_documents(&self, texts: &[String]) -> MemoryResult<Vec<Vec<f32>>> {
        let prefixed: Vec<String> = texts
            .iter()
            .map(|text| self.prefixed(&self.spec.passage_prefix, text))
            .collect();
        self.run(prefixed).await
    }

    async fn embed_query(&self, text: &str) -> MemoryResult<Vec<f32>> {
        let prefixed = self.prefixed(&self.spec.query_prefix, text);
        let mut vectors = self.run(vec![prefixed]).await?;
        vectors
            .pop()
            .ok_or_else(|| MemoryError::Backend("embedder returned no vector".to_owned()))
    }
}

impl LocalEmbedder {
    #[cfg(feature = "onnx")]
    async fn run(&self, texts: Vec<String>) -> MemoryResult<Vec<Vec<f32>>> {
        let engine = std::sync::Arc::clone(&self.engine);
        tokio::task::spawn_blocking(move || engine.embed(&texts))
            .await
            .map_err(|error| MemoryError::Backend(format!("embedding task: {error}")))?
    }

    #[cfg(not(feature = "onnx"))]
    async fn run(&self, _texts: Vec<String>) -> MemoryResult<Vec<Vec<f32>>> {
        // Unreachable: `load` never returns a value without the engine.
        Err(MemoryError::Backend("no onnx engine".to_owned()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentkit_memory::ModelId;
    use std::io::Write;

    const HELLO_SHA256: &str = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";

    fn model(path: &Path, sha: &str) -> LocalModel {
        LocalModel {
            spec: ModelSpec {
                id: ModelId::new("local@test"),
                dims: 4,
                max_tokens: 512,
                query_prefix: "query: ".to_owned(),
                passage_prefix: "passage: ".to_owned(),
            },
            model_path: path.to_owned(),
            model_sha256: sha.to_owned(),
            tokenizer_path: path.to_owned(),
            tokenizer_sha256: sha.to_owned(),
        }
    }

    fn file_with(bytes: &[u8]) -> (tempfile::TempDir, PathBuf) {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("model.onnx");
        std::fs::File::create(&path)
            .and_then(|mut file| file.write_all(bytes))
            .expect("write");
        (directory, path)
    }

    #[test]
    fn a_matching_hash_verifies_and_a_wrong_one_is_refused() {
        let (_directory, path) = file_with(b"hello");
        let bytes = read_verified(&path, HELLO_SHA256).expect("pinned hash");
        assert_eq!(bytes, b"hello");
        read_verified(&path, &HELLO_SHA256.to_uppercase()).expect("case-insensitive");
        let wrong = read_verified(&path, &"0".repeat(64));
        assert!(matches!(wrong, Err(MemoryError::Backend(_))), "{wrong:?}");
        let missing = read_verified(&path.with_extension("none"), HELLO_SHA256);
        assert!(
            matches!(missing, Err(MemoryError::Backend(_))),
            "{missing:?}"
        );
    }

    #[test]
    fn loading_refuses_a_tampered_file_before_any_engine_runs() {
        let (_directory, path) = file_with(b"tampered");
        let outcome = LocalEmbedder::load(model(&path, HELLO_SHA256));
        assert!(
            matches!(&outcome, Err(MemoryError::Backend(text)) if text.contains("sha-256")),
            "{:?}",
            outcome.err()
        );
    }

    #[test]
    fn loading_refuses_a_tampered_tokenizer() {
        let (_directory, path) = file_with(b"hello");
        let (_other, tokenizer) = file_with(b"tampered");
        let mut pinned = model(&path, HELLO_SHA256);
        pinned.tokenizer_path = tokenizer;
        let outcome = LocalEmbedder::load(pinned);
        assert!(
            matches!(&outcome, Err(MemoryError::Backend(text)) if text.contains("sha-256")),
            "{:?}",
            outcome.err()
        );
    }

    #[cfg(not(feature = "onnx"))]
    #[test]
    fn without_the_onnx_feature_loading_fails_cleanly() {
        let (_directory, path) = file_with(b"hello");
        let outcome = LocalEmbedder::load(model(&path, HELLO_SHA256));
        assert!(
            matches!(&outcome, Err(MemoryError::Backend(text)) if text.contains("onnx")),
            "{:?}",
            outcome.err()
        );
    }
}
