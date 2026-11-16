//! Numerical parity of the ONNX adapter against sentence-transformers
//! (BAAI/bge-m3, CLS pooling, normalised). Gated: it needs the model, so it
//! only runs when `AGENTKIT_EMBED_MODEL_DIR` names a directory holding
//! `tokenizer.json` and the ONNX file (`AGENTKIT_EMBED_MODEL_FILE`, default
//! `model.onnx`), and `ORT_DYLIB_PATH` points at libonnxruntime. Without the
//! variable the test prints a notice and passes.
//!
//! Regenerate the fixture with `fixtures/gen_bge_m3_reference.py`.
#![cfg(feature = "onnx")]

use std::path::PathBuf;

use agentkit_embed_local::{LocalEmbedder, LocalModel, PoolingConfig, bge_m3_spec};
use agentkit_memory::Embedder;
use sha2::{Digest, Sha256};

const MIN_COSINE: f32 = 0.999;

fn sha256_of(path: &PathBuf) -> String {
    let bytes = std::fs::read(path).expect("read model file");
    Sha256::digest(&bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let norm = |v: &[f32]| v.iter().map(|x| x * x).sum::<f32>().sqrt();
    dot / (norm(a) * norm(b))
}

#[tokio::test]
async fn onnx_adapter_matches_the_sentence_transformers_reference() {
    let Ok(dir) = std::env::var("AGENTKIT_EMBED_MODEL_DIR") else {
        eprintln!("AGENTKIT_EMBED_MODEL_DIR not set: skipping BGE-M3 parity test");
        return;
    };
    let dir = PathBuf::from(dir);
    let file = std::env::var("AGENTKIT_EMBED_MODEL_FILE").unwrap_or_else(|_| "model.onnx".into());
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/bge_m3_reference.json")).expect("fixture json");
    let cases = fixture["cases"].as_array().expect("cases");

    let mut spec = bge_m3_spec();
    // The reference was generated with max_seq_length = 256 so the truncation
    // cases stay cheap; the production spec keeps 8192.
    spec.max_tokens = fixture["max_tokens"].as_u64().expect("max_tokens") as u32;
    let model_path = dir.join(file);
    let tokenizer_path = dir.join("tokenizer.json");
    let embedder = LocalEmbedder::load(LocalModel {
        spec,
        pooling: PoolingConfig::bge_m3(),
        model_sha256: sha256_of(&model_path),
        model_path,
        tokenizer_sha256: sha256_of(&tokenizer_path),
        tokenizer_path,
    })
    .expect("load model");

    let texts: Vec<String> = cases
        .iter()
        .map(|case| case["text"].as_str().expect("text").to_owned())
        .collect();
    // One batch of mixed lengths: exercises padding.
    let batch = embedder.embed_documents(&texts).await.expect("batch");
    assert_eq!(batch.len(), cases.len());

    let mut min_reference = f32::MAX;
    let mut min_batch_vs_single = f32::MAX;
    for ((case, text), vector) in cases.iter().zip(&texts).zip(&batch) {
        let name = case["name"].as_str().expect("name");
        let reference: Vec<f32> = case["vector"]
            .as_array()
            .expect("vector")
            .iter()
            .map(|x| x.as_f64().expect("f64") as f32)
            .collect();
        assert_eq!(vector.len(), reference.len(), "{name}: dims");
        let norm = vector.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-3, "{name}: norm {norm}");

        let to_reference = cosine(vector, &reference);
        min_reference = min_reference.min(to_reference);
        assert!(
            to_reference >= MIN_COSINE,
            "{name}: cosine to reference {to_reference}"
        );

        let single = embedder
            .embed_documents(std::slice::from_ref(text))
            .await
            .expect("single")
            .remove(0);
        let to_single = cosine(vector, &single);
        min_batch_vs_single = min_batch_vs_single.min(to_single);
        assert!(
            to_single >= 0.9999,
            "{name}: batch vs single cosine {to_single}"
        );
    }
    eprintln!("min cosine to reference: {min_reference}");
    eprintln!("min cosine batch vs single: {min_batch_vs_single}");
}
