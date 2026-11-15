//! Memory context: long-term memory as a table of dated, attributed
//! statements (spec MEM-*, ADR 0035). The crate owns the domain types, the
//! four ports it drives (`MemoryRepository`, `RetrievalIndex`, `Embedder`,
//! `Reranker`) and `MemoryService`; it contains no SQL and names no concrete
//! store. Adapters live in layer 4, twins and conformance suites in
//! `agentkit-testkit`.

mod error;
mod ids;
mod model;
mod ports;
mod retrieval;
mod service;

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
    Candidate, Hit, IndexFilter, IndexItem, IndexStats, LexicalQuery, ModelSpec, ModelState,
    ModelStats, Searchability,
};
pub use service::MemoryService;
