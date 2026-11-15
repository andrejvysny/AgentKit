//! The four ports the memory context drives. Each has an in-memory twin and
//! one conformance suite in `agentkit-testkit`.
//!
//! A transaction is begun on the port, handed to the write methods and
//! committed on the same port; dropping it is a rollback. The two handle
//! types are distinct on purpose so a split store can later implement an
//! outbox between records and index.

use std::any::Any;

use async_trait::async_trait;

use crate::error::{MemoryError, MemoryResult};
use crate::ids::{ItemId, ModelId, RecordId, Timestamp};
use crate::model::{
    Actor, MemoryEvent, NewRecord, Page, PurgeReport, Record, RecordFilter, Scope, Status,
};
use crate::retrieval::{
    Candidate, Hit, IndexFilter, IndexItem, IndexStats, ItemVector, LexicalQuery, ModelSpec,
};

macro_rules! opaque_tx {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        pub struct $name {
            inner: Box<dyn Any + Send>,
        }

        impl $name {
            /// For implementations: wrap the adapter's own transaction state.
            pub fn new<T: Any + Send>(state: T) -> Self {
                Self { inner: Box::new(state) }
            }

            /// For implementations: the adapter's state, or
            /// [`MemoryError::ForeignTransaction`] if the handle came from
            /// another implementation.
            pub fn state_mut<T: Any + Send>(&mut self) -> MemoryResult<&mut T> {
                self.inner.downcast_mut::<T>().ok_or(MemoryError::ForeignTransaction)
            }

            /// For implementations: take the adapter's state back to commit it.
            pub fn into_state<T: Any + Send>(self) -> MemoryResult<T> {
                self.inner
                    .downcast::<T>()
                    .map(|state| *state)
                    .map_err(|_| MemoryError::ForeignTransaction)
            }
        }

        impl std::fmt::Debug for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(stringify!($name))
            }
        }
    };
}

opaque_tx!(
    /// An open record-store transaction.
    MemoryTx
);
opaque_tx!(
    /// An open retrieval-index transaction.
    IndexTx
);

#[async_trait]
pub trait MemoryRepository: Send + Sync {
    async fn begin(&self) -> MemoryResult<MemoryTx>;

    /// Apply the transaction atomically. [`MemoryError::Conflict`] means
    /// another commit moved the base and nothing was applied.
    async fn commit(&self, tx: MemoryTx) -> MemoryResult<()>;

    /// Insert a record and append its `captured` (active) or `proposed`
    /// event, credited to `rec.origin.actor()` at `rec.created_at`.
    async fn insert(&self, tx: &mut MemoryTx, rec: NewRecord) -> MemoryResult<RecordId>;

    /// Insert `new` and replace `old` with it in one step: `new` must be
    /// active (a proposal never supersedes) and `old` a current active
    /// record in the same scope; it gets `valid_to =
    /// new.valid_from` and `superseded_by`, the new record gets `supersedes`.
    /// Both records change in the same transaction or neither does.
    async fn supersede(
        &self,
        tx: &mut MemoryTx,
        old: RecordId,
        new: NewRecord,
    ) -> MemoryResult<RecordId>;

    /// Move a record along [`Status::can_transition_to`]. Approval stamps
    /// `approved_at`; retraction closes validity at `at` when still open.
    async fn set_status(
        &self,
        tx: &mut MemoryTx,
        id: RecordId,
        to: Status,
        actor: Actor,
        at: Timestamp,
    ) -> MemoryResult<()>;

    /// Commit `tx` together with the index transaction `index_tx` of
    /// `index`. The default commits the records first and the index second,
    /// two separate commits. An adapter whose repository and index share
    /// one database overrides it to apply both in a single transaction
    /// (recognising its own index by the handle's state) so a record and
    /// its index entry never part ways.
    async fn commit_with_index(
        &self,
        tx: MemoryTx,
        index: &dyn RetrievalIndex,
        index_tx: IndexTx,
    ) -> MemoryResult<()> {
        self.commit(tx).await?;
        index.commit_index(index_tx).await
    }

    /// Committed state only.
    async fn get(&self, id: RecordId) -> MemoryResult<Option<Record>>;

    /// Ordered by `created_at`, then id.
    async fn list(&self, filter: RecordFilter, page: Page) -> MemoryResult<Vec<Record>>;

    /// Active, current records with `is_core` for the given scopes, ordered
    /// by scope kind (user, project, agent), `core_order` (unset last), id.
    async fn core_block(&self, scopes: &[Scope]) -> MemoryResult<Vec<Record>>;

    /// Delete the records and scrub their audit rows (the `purged` event
    /// keeps only id, time and actor), plus the `detail` of other records'
    /// events that name a purged id, in one transaction. Neighbour links
    /// are cleared; a predecessor stays closed (`valid_to` kept).
    async fn purge(
        &self,
        ids: &[RecordId],
        actor: Actor,
        at: Timestamp,
    ) -> MemoryResult<PurgeReport>;

    /// The record's audit trail in `seq` order.
    async fn events(&self, id: RecordId) -> MemoryResult<Vec<MemoryEvent>>;
}

/// Storage-agnostic search over ids and filter fields. Fusion lives in the
/// service, not here, so a different store can replace an adapter without
/// changing ranking.
#[async_trait]
pub trait RetrievalIndex: Send + Sync {
    async fn begin_index(&self) -> MemoryResult<IndexTx>;

    async fn commit_index(&self, tx: IndexTx) -> MemoryResult<()>;

    /// Insert or replace by item id. A vector for a model the index does
    /// not know is [`MemoryError::UnknownModel`].
    async fn upsert(&self, tx: &mut IndexTx, items: &[IndexItem]) -> MemoryResult<()>;

    async fn delete(&self, tx: &mut IndexTx, ids: &[ItemId]) -> MemoryResult<()>;

    /// Best first; case and diacritics are ignored.
    async fn lexical(&self, q: &LexicalQuery, f: &IndexFilter, k: u32) -> MemoryResult<Vec<Hit>>;

    /// Nearest neighbours by cosine similarity, best first.
    async fn knn(
        &self,
        model: &ModelId,
        v: &[f32],
        f: &IndexFilter,
        k: u32,
    ) -> MemoryResult<Vec<Hit>>;

    /// Register `model` as `building`; the active model keeps serving.
    async fn begin_reembed(&self, model: &ModelSpec) -> MemoryResult<()>;

    /// Make a building model the active one; the previous active model
    /// becomes retired, its `retired_at` stamped with the caller's `at`.
    async fn commit_reembed(&self, model: &ModelId, at: Timestamp) -> MemoryResult<()>;

    /// Remove a model and its vectors. The active model cannot be dropped.
    async fn drop_model(&self, model: &ModelId) -> MemoryResult<()>;

    /// Up to `k` ids, ascending and strictly after `after`, of items that
    /// have no vector under `model` (any state). Drives background indexing
    /// and re-embedding; the cursor lets a caller walk items it cannot
    /// embed without looping. An unknown model is
    /// [`MemoryError::UnknownModel`].
    async fn unembedded(
        &self,
        model: &ModelId,
        after: Option<&ItemId>,
        k: u32,
    ) -> MemoryResult<Vec<ItemId>>;

    /// Store vectors for items that are ALREADY indexed, in one atomic
    /// write, and nothing else: it never creates or changes an item or its
    /// `searchable` state. An entry is skipped (not an error) when its item
    /// is absent (deleted or purged meanwhile), its current text hash
    /// differs from `text_sha256`, or the adapter can see the item's record
    /// is not active. Returns how many vectors were stored. An unknown
    /// model is [`MemoryError::UnknownModel`], a wrong dimension
    /// [`MemoryError::InvalidInput`].
    async fn put_vectors(&self, model: &ModelId, vectors: &[ItemVector]) -> MemoryResult<usize>;

    /// The active model, if any: a point lookup for the search hot path.
    async fn active_model(&self) -> MemoryResult<Option<ModelId>>;

    /// Remove items and their vectors for good.
    async fn purge(&self, ids: &[ItemId]) -> MemoryResult<PurgeReport>;

    async fn stats(&self) -> MemoryResult<IndexStats>;
}

#[async_trait]
pub trait Embedder: Send + Sync {
    /// Model identity, dimensions, token limit and prefixes.
    fn spec(&self) -> &ModelSpec;

    /// One vector of `spec().dims` per text, in input order.
    async fn embed_documents(&self, texts: &[String]) -> MemoryResult<Vec<Vec<f32>>>;

    async fn embed_query(&self, text: &str) -> MemoryResult<Vec<f32>>;
}

#[async_trait]
pub trait Reranker: Send + Sync {
    /// Reorder (and possibly rescore) the candidates; implementations must
    /// neither drop nor invent candidates.
    async fn rerank(&self, query: &str, candidates: Vec<Candidate>)
    -> MemoryResult<Vec<Candidate>>;
}

/// The default reranker: leaves the order as fusion produced it.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopReranker;

#[async_trait]
impl Reranker for NoopReranker {
    async fn rerank(
        &self,
        _query: &str,
        candidates: Vec<Candidate>,
    ) -> MemoryResult<Vec<Candidate>> {
        Ok(candidates)
    }
}
