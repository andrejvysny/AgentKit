//! Memory context: long-term memory as a table of dated, attributed
//! statements (spec MEM-*, ADR 0035). The crate owns the domain types, the
//! four ports it drives (`MemoryRepository`, `RetrievalIndex`, `Embedder`,
//! `Reranker`) and `MemoryService`; it contains no SQL and names no concrete
//! store. Adapters live in layer 4, twins and conformance suites in
//! `agentkit-testkit`.

mod capture;
mod consent;
mod core_block;
mod embedding;
mod error;
mod gate;
mod ids;
mod model;
mod ports;
mod retrieval;
mod search;
mod service;
mod tools;
mod write;

pub use capture::{
    CAPTURES_PER_DAY, CAPTURES_PER_TURN, CaptureConfig, CaptureOutcome, CaptureTurn,
    EXTRACTION_PROMPT, ExtractedCandidate, KnownRecord, MAX_CANDIDATES, PROPOSALS_PER_DAY,
    ProposalReason, Rejection, UPDATE_CONTEXT_RECORDS, extraction_prompt, parse_candidates,
    restrict_updates,
};
pub use core_block::{
    CORE_BUDGET_TOKENS, CoreBlock, CoreEntry, civil_date, core_tokens, entry_tokens,
    estimate_tokens, parse_date, render_core_block, scope_label, short_id, single_line,
};
pub use embedding::EmbeddingStatus;
pub use error::{MemoryError, MemoryResult};
pub use gate::{
    Consent, GateRefusal, MAX_QUOTE_CHARS, MIN_QUOTE_CHARS, QuoteCheck, REMEMBER_PHRASES,
    SensitiveCategory, consent_for, contains_secret, default_sensitive_categories,
    has_account_number, has_remember_request, health_category, is_full_statement, normalize,
    normalized_key, owner_span, screen_text, sensitive_category, strip_invisible, text_covered,
};
pub use ids::{ItemId, ModelId, RecordId, Timestamp};
pub use model::{
    Actor, Corpus, EventKind, Kind, MAX_TEXT_CHARS, MemoryEvent, NewRecord, Origin, Page,
    PurgeReport, Record, RecordFilter, Scope, ScopeKind, Status,
};
pub use ports::{
    CorePin, Embedder, IndexTx, MemoryRepository, MemoryTx, NoopReranker, Reranker, RetrievalIndex,
};
pub use retrieval::{
    Candidate, DEFAULT_SEARCH_LIMIT, Hit, IndexFilter, IndexItem, IndexStats, ItemVector,
    KIND_FILTER_TOP_K, LEXICAL_TOP_K, LexicalQuery, MAX_SEARCH_LIMIT, MatchKind, ModelSpec,
    ModelState, ModelStats, REEMBED_GRACE_MS, RERANK_TOP_K, RRF_K, SearchHit, SearchMode,
    SearchResults, Searchability, VECTOR_TOP_K,
};
pub use service::{MemoryService, REJECTED_RETENTION_MS, RETRACTED_RETENTION_MS};
pub use tools::{
    MEMORY_FORGET, MEMORY_GET, MEMORY_GUIDANCE, MEMORY_NOTICE, MEMORY_PROPOSE, MEMORY_SEARCH,
    MEMORY_TOOL_NAMES, MEMORY_WRITE_CAPABILITY, MEMORY_WRITE_TOOLS, MemoryToolEnv, OwnerTurnSource,
    SessionScopes, is_memory_write_tool, memory_tool_definitions, memory_tools,
};
pub use write::{Downgrade, MAX_RATIONALE_CHARS, OwnerMessage, Proposal, ProposeOutcome};
