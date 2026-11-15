//! Conformance suite for `agentkit_memory::Reranker`: an implementation may
//! reorder and rescore, but never drops or invents a candidate.

use std::sync::Arc;

use agentkit_memory::{Candidate, Corpus, ItemId, Reranker};

use super::case_support::port;

fn candidate(id: &str, score: f32) -> Candidate {
    Candidate {
        id: ItemId::new(id),
        corpus: Corpus::Memory,
        text: format!("text of {id}"),
        score,
    }
}

fn sorted_ids(candidates: &[Candidate]) -> Vec<String> {
    let mut ids: Vec<String> = candidates.iter().map(|c| c.id.0.clone()).collect();
    ids.sort();
    ids
}

async fn keeps_every_candidate<T: Reranker>(reranker: Arc<T>) {
    let input = vec![
        candidate("a", 0.9),
        candidate("b", 0.5),
        candidate("c", 0.1),
    ];
    let output = port(reranker.rerank("query", input.clone()).await);
    assert_eq!(sorted_ids(&output), sorted_ids(&input));
}

async fn empty_input_yields_empty<T: Reranker>(reranker: Arc<T>) {
    assert!(port(reranker.rerank("query", Vec::new()).await).is_empty());
}

async fn is_deterministic<T: Reranker>(reranker: Arc<T>) {
    let input = vec![candidate("a", 0.9), candidate("b", 0.5)];
    let first = port(reranker.rerank("query", input.clone()).await);
    let second = port(reranker.rerank("query", input).await);
    assert_eq!(first, second);
}

suite!(
    "reranker",
    Reranker,
    keeps_every_candidate,
    empty_input_yields_empty,
    is_deterministic
);
