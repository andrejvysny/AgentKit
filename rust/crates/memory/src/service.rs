//! The memory service. This skeleton holds the ports and implements
//! supersession; the write gate, hybrid search and fusion land with later
//! roadmap packages and live here, never in an adapter.

use std::sync::Arc;

use sha2::{Digest, Sha256};

use crate::error::{MemoryError, MemoryResult};
use crate::ids::{ItemId, RecordId, Timestamp};
use crate::model::Page;
use crate::model::{
    Actor, Corpus, EventKind, NewRecord, PurgeReport, Record, RecordFilter, Scope, Status,
};
use crate::ports::{Embedder, MemoryRepository, NoopReranker, Reranker, RetrievalIndex};
use crate::retrieval::{IndexItem, Searchability};

/// A retracted record is purged for good this long after its retraction
/// (spec, "Purge": 30 days).
pub const RETRACTED_RETENTION_MS: i64 = 30 * 24 * 60 * 60 * 1000;

/// A rejected proposal is kept this long (to suppress repeats of it), then
/// purged by the same sweep (spec, "Purge": 90 days).
pub const REJECTED_RETENTION_MS: i64 = 90 * 24 * 60 * 60 * 1000;

/// The index entry of a record. Only active records are indexed; the
/// service is the one place that enforces it.
fn index_item(
    id: RecordId,
    scope: &Scope,
    text: &str,
    ts: Timestamp,
    s: Searchability,
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
    }
}

pub struct MemoryService {
    repository: Arc<dyn MemoryRepository>,
    index: Arc<dyn RetrievalIndex>,
    embedder: Option<Arc<dyn Embedder>>,
    reranker: Arc<dyn Reranker>,
}

impl MemoryService {
    /// A service with no embedder (search runs lexical-only) and the no-op
    /// reranker.
    pub fn new(repository: Arc<dyn MemoryRepository>, index: Arc<dyn RetrievalIndex>) -> Self {
        Self {
            repository,
            index,
            embedder: None,
            reranker: Arc::new(NoopReranker),
        }
    }

    pub fn with_embedder(mut self, embedder: Arc<dyn Embedder>) -> Self {
        self.embedder = Some(embedder);
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

    pub fn embedder(&self) -> Option<&Arc<dyn Embedder>> {
        self.embedder.as_ref()
    }

    pub fn reranker(&self) -> &Arc<dyn Reranker> {
        &self.reranker
    }

    /// Insert one record in its own transaction. An active record is
    /// indexed in the same write path; a proposal is not.
    pub async fn insert(&self, rec: NewRecord) -> MemoryResult<RecordId> {
        rec.validate()?;
        let item = (rec.status == Status::Active)
            .then(|| (rec.scope.clone(), rec.text.clone(), rec.created_at));
        let mut tx = self.repository.begin().await?;
        let id = self.repository.insert(&mut tx, rec).await?;
        let mut index_tx = self.index.begin_index().await?;
        if let Some((scope, text, ts)) = item {
            let item = index_item(id, &scope, &text, ts, Searchability::Current);
            self.index.upsert(&mut index_tx, &[item]).await?;
        }
        self.repository
            .commit_with_index(tx, &*self.index, index_tx)
            .await?;
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
        let record = self
            .repository
            .get(id)
            .await?
            .ok_or(MemoryError::NotFound(id))?;
        let mut tx = self.repository.begin().await?;
        self.repository
            .set_status(&mut tx, id, to, actor, at)
            .await?;
        let mut index_tx = self.index.begin_index().await?;
        if to == Status::Active {
            let item = index_item(
                id,
                &record.scope,
                &record.text,
                record.created_at,
                Searchability::Current,
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
        Ok(())
    }

    /// Replace the current record `old` with `new`: one transaction sets
    /// `valid_to` and `superseded_by` on the old record and `supersedes`
    /// on the new one. The old item stays in the index as superseded (hidden
    /// from default search) and the new one is indexed. A replacement that
    /// is not active, or is otherwise invalid, leaves everything untouched.
    pub async fn supersede(&self, old: RecordId, new: NewRecord) -> MemoryResult<RecordId> {
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
        let fresh = (new.scope.clone(), new.text.clone(), new.created_at);
        let mut tx = self.repository.begin().await?;
        let id = self.repository.supersede(&mut tx, old, new).await?;
        let mut index_tx = self.index.begin_index().await?;
        let closed = index_item(
            old,
            &previous.scope,
            &previous.text,
            previous.created_at,
            Searchability::Superseded,
        );
        let (scope, text, ts) = fresh;
        let opened = index_item(id, &scope, &text, ts, Searchability::Current);
        self.index.upsert(&mut index_tx, &[closed, opened]).await?;
        self.repository
            .commit_with_index(tx, &*self.index, index_tx)
            .await?;
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
