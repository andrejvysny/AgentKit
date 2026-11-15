//! The memory context's in-memory twins must satisfy the ports exactly as
//! the SQLite adapters will: one suite per port, shared by both
//! implementations ([`agentkit_testkit::conformance`]).

use agentkit_memory::{Embedder, NoopReranker};
use agentkit_testkit::{
    HashEmbedder, InMemoryMemoryRepository, InMemoryRetrievalIndex,
    conformance::{embedder, memory_repository, reranker, retrieval_index},
};

#[tokio::test]
async fn in_memory_memory_repository_conforms() {
    memory_repository::run(|| async { InMemoryMemoryRepository::new() }).await;
}

#[tokio::test]
async fn in_memory_retrieval_index_conforms() {
    retrieval_index::run(|| async { InMemoryRetrievalIndex::new() }).await;
}

#[tokio::test]
async fn hash_embedder_conforms() {
    embedder::run(|| async { HashEmbedder::new() }).await;
}

#[tokio::test]
async fn noop_reranker_conforms() {
    reranker::run(|| async { NoopReranker }).await;
}

#[test]
fn suites_have_unique_case_names() {
    for (cases, floor) in [
        (memory_repository::CASES, 25),
        (retrieval_index::CASES, 15),
        (embedder::CASES, 4),
        (reranker::CASES, 3),
    ] {
        let mut names = cases.to_vec();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), cases.len());
        assert!(names.len() >= floor, "{} cases", names.len());
    }
}

#[tokio::test]
async fn hash_embedder_scores_shared_words_higher() {
    let embedder = HashEmbedder::new();
    let docs = vec![
        "owner writes backend services in rust".to_owned(),
        "favourite tea is jasmine".to_owned(),
    ];
    let vectors = embedder.embed_documents(&docs).await.expect("documents");
    let query = embedder.embed_query("rust backend").await.expect("query");
    let dot = |v: &[f32]| v.iter().zip(&query).map(|(a, b)| a * b).sum::<f32>();
    assert!(dot(&vectors[0]) > dot(&vectors[1]));
}
