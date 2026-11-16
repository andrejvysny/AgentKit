//! The memory service. It holds the ports and implements supersession and
//! purge; hybrid search and fusion (`search.rs`) and background embedding
//! (`embedding.rs`) are further `impl` blocks. The write gate lands with a
//! later roadmap package. All of it lives here, never in an adapter.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError, RwLock};

use sha2::{Digest, Sha256};

use crate::error::{MemoryError, MemoryResult};
use crate::ids::{ItemId, RecordId, Timestamp};
use crate::model::Page;
use crate::model::{
    Actor, Corpus, EventKind, NewRecord, PurgeReport, Record, RecordFilter, Scope, Status,
};
use crate::ports::{CorePin, Embedder, MemoryRepository, NoopReranker, Reranker, RetrievalIndex};
use crate::retrieval::{IndexItem, REEMBED_GRACE_MS, Searchability};

/// A retracted record is purged for good this long after its retraction
/// (spec, "Purge": 30 days).
pub const RETRACTED_RETENTION_MS: i64 = 30 * 24 * 60 * 60 * 1000;

/// A rejected proposal is kept this long (to suppress repeats of it), then
/// purged by the same sweep (spec, "Purge": 90 days).
pub const REJECTED_RETENTION_MS: i64 = 90 * 24 * 60 * 60 * 1000;

/// The index entry of a record. Only active records are indexed; the
/// service is the one place that enforces it.
pub(crate) fn index_item(
    id: RecordId,
    scope: &Scope,
    text: &str,
    ts: Timestamp,
    s: Searchability,
    valid: (Timestamp, Option<Timestamp>),
) -> IndexItem {
    IndexItem {
        id: ItemId::from(id),
        corpus: Corpus::Memory,
        scope_key: scope.key(),
        ts,
        searchable: s,
        text_sha256: Sha256::digest(text.as_bytes()).into(),
        text: text.to_owned(),
        vector: None,
        valid_from: Some(valid.0),
        valid_to: valid.1,
    }
}

pub struct MemoryService {
    pub(crate) repository: Arc<dyn MemoryRepository>,
    pub(crate) index: Arc<dyn RetrievalIndex>,
    /// Swapped at the end of a re-embed, so a lock guard never crosses an
    /// await: callers clone the `Arc` out.
    pub(crate) embedder: RwLock<Option<Arc<dyn Embedder>>>,
    pub(crate) reranker: Arc<dyn Reranker>,
    pub(crate) reembed_grace_ms: i64,
    /// Set when an item may lack a vector; the background indexer takes it.
    pub(crate) index_hint: AtomicBool,
    /// Writers hold the read side across their commit; the final pass of a
    /// re-embed holds the write side, so no write slips between it and the
    /// model switch.
    pub(crate) write_gate: tokio::sync::RwLock<()>,
    /// Uses noted by search and get, written by [`Self::flush_usage`] off
    /// the request path.
    usage: Mutex<Vec<(RecordId, Timestamp)>>,
}

impl MemoryService {
    /// A service with no embedder (search runs lexical-only) and the no-op
    /// reranker.
    pub fn new(repository: Arc<dyn MemoryRepository>, index: Arc<dyn RetrievalIndex>) -> Self {
        Self {
            repository,
            index,
            embedder: RwLock::new(None),
            reranker: Arc::new(NoopReranker),
            reembed_grace_ms: REEMBED_GRACE_MS,
            index_hint: AtomicBool::new(false),
            write_gate: tokio::sync::RwLock::new(()),
            usage: Mutex::new(Vec::new()),
        }
    }

    /// Note that `ids` were returned to the model at `at`. Memory only: the
    /// request path never waits for a write.
    pub fn note_use(&self, ids: &[RecordId], at: Timestamp) {
        let mut pending = self.usage.lock().unwrap_or_else(PoisonError::into_inner);
        pending.extend(ids.iter().map(|id| (*id, at)));
    }

    /// Write the noted uses (`last_used_at`, `use_count`) in one call and
    /// return how many were written. A failed write drops them: usage is
    /// telemetry, not a record.
    pub async fn flush_usage(&self) -> MemoryResult<usize> {
        let pending =
            std::mem::take(&mut *self.usage.lock().unwrap_or_else(PoisonError::into_inner));
        let Some(at) = pending.iter().map(|(_, at)| *at).max() else {
            return Ok(0);
        };
        let ids: Vec<RecordId> = pending.iter().map(|(id, _)| *id).collect();
        self.repository.record_use(&ids, at).await?;
        Ok(ids.len())
    }

    pub fn with_embedder(self, embedder: Arc<dyn Embedder>) -> Self {
        self.set_embedder(embedder);
        self.index_hint.store(true, Ordering::Release);
        self
    }

    /// How long a retired embedding model is kept before [`Self::drop_retired`]
    /// removes it (default [`REEMBED_GRACE_MS`]).
    pub fn with_reembed_grace(mut self, grace_ms: i64) -> Self {
        self.reembed_grace_ms = grace_ms;
        self
    }

    pub fn with_reranker(mut self, reranker: Arc<dyn Reranker>) -> Self {
        self.reranker = reranker;
        self
    }

    pub fn repository(&self) -> &Arc<dyn MemoryRepository> {
        &self.repository
    }

    pub fn index(&self) -> &Arc<dyn RetrievalIndex> {
        &self.index
    }

    pub fn embedder(&self) -> Option<Arc<dyn Embedder>> {
        self.embedder
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub fn reranker(&self) -> &Arc<dyn Reranker> {
        &self.reranker
    }

    /// Insert one record in its own transaction. An active record is
    /// indexed in the same write path; a proposal is not.
    pub async fn insert(&self, rec: NewRecord) -> MemoryResult<RecordId> {
        let _gate = self.write_gate.read().await;
        self.insert_held(rec).await
    }

    /// [`Self::insert`] for a caller that already holds the write gate (the
    /// gate is not reentrant).
    pub(crate) async fn insert_held(&self, rec: NewRecord) -> MemoryResult<RecordId> {
        rec.validate()?;
        let item = (rec.status == Status::Active).then(|| {
            (
                rec.scope.clone(),
                rec.text.clone(),
                rec.created_at,
                (rec.valid_from, None),
            )
        });
        let mut tx = self.repository.begin().await?;
        let id = self.repository.insert(&mut tx, rec).await?;
        let mut index_tx = self.index.begin_index().await?;
        if let Some((scope, text, ts, valid)) = item {
            let item = index_item(id, &scope, &text, ts, Searchability::Current, valid);
            self.index.upsert(&mut index_tx, &[item]).await?;
        }
        self.repository
            .commit_with_index(tx, &*self.index, index_tx)
            .await?;
        self.index_hint.store(true, Ordering::Release);
        Ok(id)
    }

    /// Move a record along the status machine and keep the index in step:
    /// approval indexes it, rejection or retraction removes it.
    pub async fn set_status(
        &self,
        id: RecordId,
        to: Status,
        actor: Actor,
        at: Timestamp,
    ) -> MemoryResult<()> {
        let _gate = self.write_gate.read().await;
        self.set_status_held(id, to, actor, at, None).await
    }

    /// [`Self::set_status`] for a caller that already holds the write gate
    /// (the gate is not reentrant).
    pub(crate) async fn set_status_held(
        &self,
        id: RecordId,
        to: Status,
        actor: Actor,
        at: Timestamp,
        detail: Option<serde_json::Value>,
    ) -> MemoryResult<()> {
        let record = self
            .repository
            .get(id)
            .await?
            .ok_or(MemoryError::NotFound(id))?;
        if record.status == Status::Proposed && to == Status::Active && record.supersedes.is_some()
        {
            return Err(MemoryError::InvalidInput(
                "a proposal with a replacement target activates only through approve".to_owned(),
            ));
        }
        let mut tx = self.repository.begin().await?;
        self.repository
            .set_status(&mut tx, id, to, actor, at, detail)
            .await?;
        let mut index_tx = self.index.begin_index().await?;
        if to == Status::Active {
            let item = index_item(
                id,
                &record.scope,
                &record.text,
                record.created_at,
                Searchability::Current,
                (record.valid_from, record.valid_to),
            );
            self.index.upsert(&mut index_tx, &[item]).await?;
        } else {
            self.index
                .delete(&mut index_tx, &[ItemId::from(id)])
                .await?;
        }
        self.repository
            .commit_with_index(tx, &*self.index, index_tx)
            .await?;
        self.index_hint.store(true, Ordering::Release);
        Ok(())
    }

    /// Replace the current record `old` with `new`: one transaction sets
    /// `valid_to` and `superseded_by` on the old record and `supersedes`
    /// on the new one. The old item stays in the index as superseded (hidden
    /// from default search) and the new one is indexed. A replacement that
    /// is not active, or is otherwise invalid, leaves everything untouched.
    pub async fn supersede(&self, old: RecordId, new: NewRecord) -> MemoryResult<RecordId> {
        self.supersede_pinned(old, new, None).await
    }

    /// [`Self::supersede`], pinning the replacement in the same transaction
    /// when `pin` is set, so the repository re-checks the core budget against
    /// the block as the commit sees it (the old record already closed).
    pub(crate) async fn supersede_pinned(
        &self,
        old: RecordId,
        new: NewRecord,
        pin: Option<CorePin>,
    ) -> MemoryResult<RecordId> {
        let _gate = self.write_gate.read().await;
        self.supersede_held(old, new, pin).await
    }

    /// [`Self::supersede_pinned`] for a caller that already holds the write
    /// gate (the gate is not reentrant).
    pub(crate) async fn supersede_held(
        &self,
        old: RecordId,
        new: NewRecord,
        pin: Option<CorePin>,
    ) -> MemoryResult<RecordId> {
        new.validate()?;
        if new.status != Status::Active {
            return Err(MemoryError::InvalidInput(
                "a replacement record must be active".to_owned(),
            ));
        }
        let previous = self
            .repository
            .get(old)
            .await?
            .ok_or(MemoryError::NotFound(old))?;
        let fresh = (
            new.scope.clone(),
            new.text.clone(),
            new.created_at,
            new.valid_from,
        );
        let mut tx = self.repository.begin().await?;
        let id = self.repository.supersede(&mut tx, old, new).await?;
        if pin.is_some() {
            self.repository
                .set_core(&mut tx, id, pin, Actor::Owner, fresh.3)
                .await?;
        }
        let mut index_tx = self.index.begin_index().await?;
        let closed = index_item(
            old,
            &previous.scope,
            &previous.text,
            previous.created_at,
            Searchability::Superseded,
            (previous.valid_from, Some(fresh.3)),
        );
        let (scope, text, ts, valid_from) = fresh;
        let opened = index_item(
            id,
            &scope,
            &text,
            ts,
            Searchability::Current,
            (valid_from, None),
        );
        self.index.upsert(&mut index_tx, &[closed, opened]).await?;
        self.repository
            .commit_with_index(tx, &*self.index, index_tx)
            .await?;
        self.index_hint.store(true, Ordering::Release);
        Ok(id)
    }

    pub async fn get(&self, id: RecordId) -> MemoryResult<Option<Record>> {
        self.repository.get(id).await
    }

    /// Remove records for good: the record rows and audit details first,
    /// then whatever the index still holds for the ids (a repository and
    /// index that share a database purge both in one transaction, so the
    /// second step then finds nothing). The merged report carries the rows
    /// removed per table and the residue check of both stores.
    pub async fn purge(
        &self,
        ids: &[RecordId],
        actor: Actor,
        at: Timestamp,
    ) -> MemoryResult<PurgeReport> {
        let mut report = self.repository.purge(ids, actor, at).await?;
        let items: Vec<ItemId> = ids.iter().copied().map(ItemId::from).collect();
        report.merge(self.index.purge(&items).await?);
        Ok(report)
    }

    /// Purge every record retracted at least [`RETRACTED_RETENTION_MS`]
    /// before `now` and every rejected proposal rejected at least
    /// [`REJECTED_RETENTION_MS`] before `now`; the time is the `retracted` /
    /// `rejected` audit event. Idempotent: a second call finds nothing.
    pub async fn sweep_retracted(&self, now: Timestamp) -> MemoryResult<PurgeReport> {
        let mut due = Vec::new();
        for (status, event, retention) in [
            (
                Status::Retracted,
                EventKind::Retracted,
                RETRACTED_RETENTION_MS,
            ),
            (Status::Rejected, EventKind::Rejected, REJECTED_RETENTION_MS),
        ] {
            let filter = RecordFilter {
                statuses: vec![status],
                include_superseded: true,
                ..Default::default()
            };
            let cutoff = now.0.saturating_sub(retention);
            for record in self.repository.list(filter, Page::all()).await? {
                let events = self.repository.events(record.id).await?;
                let since = events
                    .iter()
                    .rev()
                    .find(|e| e.kind == event)
                    .map_or(record.valid_to.unwrap_or(record.created_at), |e| e.at);
                if since.0 <= cutoff {
                    due.push(record.id);
                }
            }
        }
        if due.is_empty() {
            return Ok(PurgeReport::default());
        }
        self.purge(&due, Actor::System, now).await
    }
}
