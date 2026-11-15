//! Conformance suite for `agentkit_memory::Embedder`: shape and determinism
//! guarantees the retrieval pipeline relies on. A real model adapter and the
//! deterministic fake run the same cases.

use std::sync::Arc;

use agentkit_memory::Embedder;

use super::case_support::port;

async fn spec_is_usable<T: Embedder>(embedder: Arc<T>) {
    let spec = embedder.spec();
    assert!(spec.dims > 0 && spec.max_tokens > 0);
    assert!(!spec.id.as_str().is_empty());
}

async fn documents_yield_one_vector_per_text_in_order<T: Embedder>(embedder: Arc<T>) {
    let texts = vec![
        "alpha beta".to_owned(),
        "gamma".to_owned(),
        "alpha beta".to_owned(),
    ];
    let vectors = port(embedder.embed_documents(&texts).await);
    assert_eq!(vectors.len(), 3);
    for vector in &vectors {
        assert_eq!(vector.len(), embedder.spec().dims as usize);
        assert!(vector.iter().all(|x| x.is_finite()));
    }
    assert_eq!(vectors[0], vectors[2], "equal texts embed equally");
    assert_ne!(vectors[0], vectors[1]);
}

async fn empty_batch_yields_empty<T: Embedder>(embedder: Arc<T>) {
    assert!(port(embedder.embed_documents(&[]).await).is_empty());
}

async fn query_has_model_dims_and_is_deterministic<T: Embedder>(embedder: Arc<T>) {
    let first = port(
        embedder
            .embed_query("what language does the owner use")
            .await,
    );
    let second = port(
        embedder
            .embed_query("what language does the owner use")
            .await,
    );
    assert_eq!(first.len(), embedder.spec().dims as usize);
    assert_eq!(first, second);
}

suite!(
    "embedder",
    Embedder,
    spec_is_usable,
    documents_yield_one_vector_per_text_in_order,
    empty_batch_yields_empty,
    query_has_model_dims_and_is_deterministic,
);
