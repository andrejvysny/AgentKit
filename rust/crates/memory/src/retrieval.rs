//! Types the retrieval ports speak in. The index sees ids, filter fields,
//! lexical text and optional vectors; the record store stays the source of
//! truth for everything else.

use serde::{Deserialize, Serialize};

use crate::ids::{ItemId, ModelId, Timestamp};
use crate::model::Corpus;

/// Whether an item answers a default search. A superseded item stays in the
/// index (history search) but is hidden unless the filter asks for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Searchability {
    #[default]
    Current,
    Superseded,
}

/// One item to index. Only active records are ever indexed.
#[derive(Debug, Clone, PartialEq)]
pub struct IndexItem {
    pub id: ItemId,
    pub corpus: Corpus,
    /// `scope_kind:scope_id`, see [`crate::Scope::key`].
    pub scope_key: String,
    pub ts: Timestamp,
    pub searchable: Searchability,
    /// Hash of `text`; lets an adapter skip an unchanged re-upsert.
    pub text_sha256: [u8; 32],
    /// Lexical payload. An adapter with an external-content text index
    /// (SQLite FTS5 over the source table) may ignore it; an adapter with
    /// no source table (the in-memory twin) must keep it.
    pub text: String,
    pub vector: Option<(ModelId, Vec<f32>)>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    pub id: ItemId,
    pub corpus: Corpus,
    /// Higher is better; only comparable within one result list.
    pub score: f32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LexicalQuery {
    pub text: String,
    /// Query terms of at least this many characters are also tried as
    /// prefixes (`term*`), which softens Slovak inflection. `0` disables.
    pub prefix_min_chars: u32,
}

impl LexicalQuery {
    /// Prefix matching from four characters, as the spec says.
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            prefix_min_chars: 4,
        }
    }
}

/// Narrowing fields of a search. Empty vectors and `None` mean "any".
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IndexFilter {
    pub corpora: Vec<Corpus>,
    pub scope_keys: Vec<String>,
    /// Inclusive bounds on `IndexItem::ts`.
    pub since: Option<Timestamp>,
    pub until: Option<Timestamp>,
    /// Also return items marked [`Searchability::Superseded`].
    pub include_superseded: bool,
}

/// A fused search result handed to the reranker.
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    pub id: ItemId,
    pub corpus: Corpus,
    pub text: String,
    pub score: f32,
}

/// What an embedding model is: identity, shape and the prefixes it expects.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelSpec {
    pub id: ModelId,
    pub dims: u32,
    pub max_tokens: u32,
    pub query_prefix: String,
    pub passage_prefix: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelState {
    Building,
    Active,
    Retired,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelStats {
    pub id: ModelId,
    pub dims: u32,
    pub state: ModelState,
    pub vectors: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IndexStats {
    pub items: u64,
    pub models: Vec<ModelStats>,
}

impl IndexStats {
    pub fn active_model(&self) -> Option<&ModelId> {
        self.models
            .iter()
            .find(|model| model.state == ModelState::Active)
            .map(|model| &model.id)
    }
}
