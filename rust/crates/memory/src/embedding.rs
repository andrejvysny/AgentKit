//! Background indexing and the re-embed flow (spec, "Embeddings"). Records
//! are written and found lexically at once; vectors are filled afterwards,
//! in batches, by [`MemoryService::index_pending`] (the runtime drives it
//! from a background task). A new model is built beside the active one and
//! switched in by [`MemoryService::commit_reembed`]; the retired model is
//! dropped by [`MemoryService::drop_retired`] after the grace period.

use std::sync::atomic::Ordering;
use std::sync::{Arc, PoisonError};

use sha2::{Digest, Sha256};

use crate::error::{MemoryError, MemoryResult};
use crate::ids::{ItemId, ModelId, RecordId, Timestamp};
use crate::model::Status;
use crate::ports::Embedder;
use crate::retrieval::{ItemVector, ModelState};
use crate::service::MemoryService;

/// How the configured embedder relates to the index (see
/// [`MemoryService::embedding_status`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EmbeddingStatus {
    /// No embedder configured: search is lexical-only.
    Disabled,
    /// The configured model is the active one.
    Ready,
    /// No model is active yet; the next `index_pending` activates it.
    Uninitialized,
    /// The configured model is `building`, an interrupted re-embed that
    /// `index_pending` resumes (or `abort_reembed` drops).
    Resuming,
    /// The index serves another model and the configured one was never
    /// registered: search is lexical-only until a `reembed`.
    Mismatch {
        configured: ModelId,
        active: ModelId,
    },
}

impl MemoryService {
    /// Replace the embedder. Search keeps using only an embedder whose
    /// model is the index's active one, so a mismatch degrades to lexical.
    pub fn set_embedder(&self, embedder: Arc<dyn Embedder>) {
        *self
            .embedder
            .write()
            .unwrap_or_else(PoisonError::into_inner) = Some(embedder);
    }

    /// Whether a write since the last call may have left items without a
    /// vector. The background indexer polls it to stay idle otherwise.
    pub fn take_index_hint(&self) -> bool {
        self.index_hint.swap(false, Ordering::AcqRel)
    }

    /// Flag that items may lack a vector, so the background indexer runs at
    /// its next tick (it also retries a failed batch this way).
    pub fn request_index(&self) {
        self.index_hint.store(true, Ordering::Release);
    }

    /// Embed every item that lacks a vector under the embedder's model, in
    /// batches of `batch`; returns how many were embedded. The first model
    /// an index ever sees becomes active here; a configured model left
    /// `building` by an interrupted re-embed is resumed to completion
    /// (see [`Self::abort_reembed`] for the alternative). With another
    /// model already active and the configured one never registered nothing
    /// happens: an explicit [`Self::reembed`] is the way, and
    /// [`Self::embedding_status`] reports the mismatch. `now` stamps a model
    /// retired by the switch.
    pub async fn index_pending(&self, batch: u32, now: Timestamp) -> MemoryResult<usize> {
        let Some(embedder) = self.embedder() else {
            return Ok(0);
        };
        let spec = embedder.spec().clone();
        let stats = self.index.stats().await?;
        let known = stats
            .models
            .iter()
            .find(|model| model.id == spec.id)
            .map(|model| model.state);
        match (known, stats.active_model().is_some()) {
            (Some(ModelState::Active), _) => {}
            (None, false) => {
                self.index.begin_reembed(&spec).await?;
                self.index.commit_reembed(&spec.id, now).await?;
            }
            (Some(ModelState::Building), false) => {
                self.index.commit_reembed(&spec.id, now).await?;
            }
            (Some(ModelState::Building), true) => {
                return self.reembed(embedder, batch, now).await;
            }
            _ => return Ok(0),
        }
        self.fill_model(&*embedder, batch).await
    }

    /// How the configured embedder relates to the index, for a host to
    /// surface (a mismatch otherwise degrades search to lexical-only
    /// without a word).
    pub async fn embedding_status(&self) -> MemoryResult<EmbeddingStatus> {
        let Some(embedder) = self.embedder() else {
            return Ok(EmbeddingStatus::Disabled);
        };
        let configured = embedder.spec().id.clone();
        let stats = self.index.stats().await?;
        let state = stats
            .models
            .iter()
            .find(|model| model.id == configured)
            .map(|model| model.state);
        Ok(match (state, stats.active_model()) {
            (Some(ModelState::Active), _) => EmbeddingStatus::Ready,
            (Some(ModelState::Building), _) => EmbeddingStatus::Resuming,
            (_, None) => EmbeddingStatus::Uninitialized,
            (_, Some(active)) => EmbeddingStatus::Mismatch {
                configured,
                active: active.clone(),
            },
        })
    }

    /// Register `target`'s model as `building`; the active model keeps
    /// serving until [`Self::commit_reembed`].
    pub async fn begin_reembed(&self, target: &dyn Embedder) -> MemoryResult<()> {
        self.index.begin_reembed(target.spec()).await
    }

    /// Give up an interrupted re-embed: drop the `building` model and its
    /// vectors. Any other state is refused.
    pub async fn abort_reembed(&self, model: &ModelId) -> MemoryResult<()> {
        let stats = self.index.stats().await?;
        match stats.models.iter().find(|m| &m.id == model) {
            Some(found) if found.state == ModelState::Building => {
                self.index.drop_model(model).await
            }
            Some(_) => Err(MemoryError::InvalidInput(format!(
                "model {model} is not building"
            ))),
            None => Err(MemoryError::UnknownModel(model.clone())),
        }
    }

    /// One pass over the index: embed items without a vector under
    /// `embedder`'s model. Items that are not records, or whose record is
    /// no longer active, are skipped.
    pub async fn fill_model(&self, embedder: &dyn Embedder, batch: u32) -> MemoryResult<usize> {
        let model = embedder.spec().id.clone();
        let batch = batch.max(1);
        let mut after: Option<ItemId> = None;
        let mut done = 0;
        loop {
            let ids = self.index.unembedded(&model, after.as_ref(), batch).await?;
            let Some(last) = ids.last().cloned() else {
                return Ok(done);
            };
            after = Some(last);
            done += self.embed_items(embedder, &model, &ids).await?;
        }
    }

    /// Vector rows only: the index item and its `searchable` state are never
    /// rewritten here, so a record purged, retracted or superseded while
    /// its text was being embedded is not brought back or re-marked (the
    /// index skips vectors for items that are gone, changed, or inactive).
    async fn embed_items(
        &self,
        embedder: &dyn Embedder,
        model: &ModelId,
        ids: &[ItemId],
    ) -> MemoryResult<usize> {
        let mut records = Vec::new();
        for id in ids {
            let Ok(record_id) = id.as_str().parse::<RecordId>() else {
                continue;
            };
            if let Some(record) = self.repository.get(record_id).await?
                && record.status == Status::Active
            {
                records.push(record);
            }
        }
        if records.is_empty() {
            return Ok(0);
        }
        let texts: Vec<String> = records.iter().map(|record| record.text.clone()).collect();
        let vectors = embedder.embed_documents(&texts).await?;
        if vectors.len() != records.len() {
            return Err(MemoryError::Backend(format!(
                "embedder returned {} vectors for {} texts",
                vectors.len(),
                records.len()
            )));
        }
        let items: Vec<ItemVector> = records
            .iter()
            .zip(vectors)
            .map(|(record, vector)| ItemVector {
                id: ItemId::from(record.id),
                text_sha256: Sha256::digest(record.text.as_bytes()).into(),
                vector,
            })
            .collect();
        self.index.put_vectors(model, &items).await
    }

    /// Make `target`'s (built) model the active one and start using it for
    /// queries; the previous model becomes retired at `at`.
    pub async fn commit_reembed(
        &self,
        target: Arc<dyn Embedder>,
        at: Timestamp,
    ) -> MemoryResult<()> {
        self.index.commit_reembed(&target.spec().id, at).await?;
        self.set_embedder(target);
        self.index_hint.store(true, Ordering::Release);
        Ok(())
    }

    /// Re-embed everything with `target`: register it (resuming a
    /// `building` registration), fill until no item lacks a vector, then
    /// run one more pass holding the write gate (no write can land between
    /// it and the switch) and switch. Search works throughout on the old
    /// model.
    pub async fn reembed(
        &self,
        target: Arc<dyn Embedder>,
        batch: u32,
        at: Timestamp,
    ) -> MemoryResult<usize> {
        self.begin_reembed(&*target).await?;
        let mut total = 0;
        loop {
            let done = self.fill_model(&*target, batch).await?;
            if done == 0 {
                break;
            }
            total += done;
        }
        let _gate = self.write_gate.write().await;
        total += self.fill_model(&*target, batch).await?;
        self.commit_reembed(target, at).await?;
        Ok(total)
    }

    /// Drop models retired at least the grace period before `now`; returns
    /// the ids dropped.
    pub async fn drop_retired(&self, now: Timestamp) -> MemoryResult<Vec<ModelId>> {
        let stats = self.index.stats().await?;
        let mut dropped = Vec::new();
        for model in stats.models {
            let due = model.state == ModelState::Retired
                && model
                    .retired_at
                    .is_some_and(|at| at.0.saturating_add(self.reembed_grace_ms) <= now.0);
            if due {
                self.index.drop_model(&model.id).await?;
                dropped.push(model.id);
            }
        }
        Ok(dropped)
    }
}
