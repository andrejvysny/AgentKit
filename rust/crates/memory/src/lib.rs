//! Memory context: long-term memory as a table of dated, attributed
//! statements (spec MEM-*, ADR 0035). The crate owns the domain types, the
//! four ports it drives (`MemoryRepository`, `RetrievalIndex`, `Embedder`,
//! `Reranker`) and `MemoryService`; it contains no SQL and names no concrete
//! store. Adapters live in layer 4, twins and conformance suites in
//! `agentkit-testkit`.

mod embedding;
mod error;
mod ids;
mod model;
mod ports;
mod retrieval;
mod search;
mod service;

pub use embedding::EmbeddingStatus;
pub use error::{MemoryError, MemoryResult};
pub use ids::{ItemId, ModelId, RecordId, Timestamp};
pub use model::{
    Actor, Corpus, EventKind, Kind, MAX_TEXT_CHARS, MemoryEvent, NewRecord, Origin, Page,
    PurgeReport, Record, RecordFilter, Scope, ScopeKind, Status,
};
pub use ports::{
    Embedder, IndexTx, MemoryRepository, MemoryTx, NoopReranker, Reranker, RetrievalIndex,
};
pub use retrieval::{
    Candidate, DEFAULT_SEARCH_LIMIT, Hit, IndexFilter, IndexItem, IndexStats, ItemVector,
    LEXICAL_TOP_K, LexicalQuery, MAX_SEARCH_LIMIT, MatchKind, ModelSpec, ModelState, ModelStats,
    REEMBED_GRACE_MS, RERANK_TOP_K, RRF_K, SearchHit, SearchMode, SearchResults, Searchability,
    VECTOR_TOP_K,
};
pub use service::{MemoryService, REJECTED_RETENTION_MS, RETRACTED_RETENTION_MS};
