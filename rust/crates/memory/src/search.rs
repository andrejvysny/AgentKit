//! Hybrid search (spec, "Hybrid search"): a lexical and a vector candidate
//! list, fused by reciprocal rank (k = 60, equal weights), the top 30 sent
//! through the reranker port, then cut to `limit`. Fusion lives here, never
//! in an adapter, so a store swap cannot change ranking.

use std::collections::HashMap;

use crate::error::{MemoryError, MemoryResult};
use crate::ids::{ItemId, RecordId};
use crate::model::{Corpus, Status};
use crate::retrieval::{
    Candidate, DEFAULT_SEARCH_LIMIT, Hit, IndexFilter, LEXICAL_TOP_K, LexicalQuery,
    MAX_SEARCH_LIMIT, MatchKind, RERANK_TOP_K, RRF_K, SearchHit, SearchMode, SearchResults,
    VECTOR_TOP_K,
};
use crate::service::MemoryService;

struct Fused {
    corpus: Corpus,
    score: f32,
    lexical: bool,
    vector: bool,
}

/// Sum `1 / (RRF_K + rank)` over the lists an item appears in (rank from 1).
fn fuse(lexical: &[Hit], vector: &[Hit]) -> Vec<(ItemId, Fused)> {
    let mut table: HashMap<ItemId, Fused> = HashMap::new();
    for (list, is_vector) in [(lexical, false), (vector, true)] {
        for (rank, hit) in list.iter().enumerate() {
            let entry = table.entry(hit.id.clone()).or_insert(Fused {
                corpus: hit.corpus,
                score: 0.0,
                lexical: false,
                vector: false,
            });
            entry.score += 1.0 / (RRF_K + rank as f32 + 1.0);
            if is_vector {
                entry.vector = true;
            } else {
                entry.lexical = true;
            }
        }
    }
    let mut fused: Vec<_> = table.into_iter().collect();
    fused.sort_by(|(a_id, a), (b_id, b)| b.score.total_cmp(&a.score).then_with(|| a_id.cmp(b_id)));
    fused
}

/// The record-store check every hit passes: active, not superseded unless
/// asked, and valid at `as_of`.
fn visible(record: &crate::model::Record, filter: &IndexFilter) -> bool {
    record.status == Status::Active
        && (filter.include_superseded || record.superseded_by.is_none())
        && filter
            .as_of
            .is_none_or(|at| record.valid_from <= at && record.valid_to.is_none_or(|to| to > at))
}

impl MemoryService {
    /// Search `filter`ed items for `query` and return at most `limit` hits
    /// (`0` means [`DEFAULT_SEARCH_LIMIT`]; more than [`MAX_SEARCH_LIMIT`]
    /// is cut to it). The filter must name a scope
    /// ([`MemoryError::EmptyScope`] otherwise). Without an embedder (or when
    /// its model is not the index's active one, or the vector side fails)
    /// the result is lexical-only; the lexical side is always available.
    /// Every hit is re-read from the record store: only active records
    /// come back (superseded ones only with `include_superseded`), valid at
    /// `as_of` when set.
    pub async fn search(
        &self,
        query: &str,
        filter: &IndexFilter,
        limit: u32,
    ) -> MemoryResult<SearchResults> {
        if filter.scope_keys.is_empty() {
            return Err(MemoryError::EmptyScope);
        }
        let limit = if limit == 0 {
            DEFAULT_SEARCH_LIMIT
        } else {
            limit.min(MAX_SEARCH_LIMIT)
        };
        let lexical = self
            .index
            .lexical(&LexicalQuery::new(query), filter, LEXICAL_TOP_K)
            .await?;
        let vector = self.vector_hits(query, filter).await;
        let mode = if vector.is_some() {
            SearchMode::Hybrid
        } else {
            SearchMode::LexicalOnly
        };
        let fused = fuse(&lexical, vector.as_deref().unwrap_or_default());
        let mut candidates = Vec::new();
        let mut found: HashMap<ItemId, (bool, bool)> = HashMap::new();
        for (id, entry) in fused {
            if candidates.len() >= RERANK_TOP_K {
                break;
            }
            let text = match id.as_str().parse::<RecordId>() {
                Ok(record_id) => match self.repository.get(record_id).await? {
                    Some(record) if visible(&record, filter) => record.text,
                    // Purged or changed between the index read and now.
                    _ => continue,
                },
                Err(_) => String::new(),
            };
            found.insert(id.clone(), (entry.lexical, entry.vector));
            candidates.push(Candidate {
                id,
                corpus: entry.corpus,
                text,
                score: entry.score,
            });
        }
        let ranked = self.reranker.rerank(query, candidates).await?;
        let hits = ranked
            .into_iter()
            .filter_map(|candidate| {
                let (lexical, vector) = found.get(&candidate.id).copied()?;
                let matched = match (lexical, vector) {
                    (true, true) => MatchKind::Both,
                    (false, true) => MatchKind::Vector,
                    _ => MatchKind::Lexical,
                };
                Some(SearchHit {
                    id: candidate.id,
                    corpus: candidate.corpus,
                    text: candidate.text,
                    score: candidate.score,
                    matched,
                })
            })
            .take(limit as usize)
            .collect();
        Ok(SearchResults { mode, hits })
    }

    /// Nearest neighbours under the active model, embedding the query with
    /// that same model. `None` means "no vector side" (see [`Self::search`]).
    async fn vector_hits(&self, query: &str, filter: &IndexFilter) -> Option<Vec<Hit>> {
        let embedder = self.embedder()?;
        let active = self.index.active_model().await.ok()??;
        if active != embedder.spec().id {
            return None;
        }
        let vector = embedder.embed_query(query).await.ok()?;
        self.index
            .knn(&active, &vector, filter, VECTOR_TOP_K)
            .await
            .ok()
    }
}
