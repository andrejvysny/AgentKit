//! In-memory `MemoryRepository`. A transaction works on a private copy of
//! the committed state and replaces it at commit if no other commit moved
//! the base meanwhile (optimistic), so a dropped transaction leaves no
//! trace and `supersede` is all-or-nothing.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use agentkit_memory::{
    Actor, CorePin, EventKind, MemoryError, MemoryEvent, MemoryRepository, MemoryResult, MemoryTx,
    NewRecord, Page, PurgeReport, Record, RecordFilter, RecordId, Scope, Status, Timestamp,
    core_tokens,
};
use async_trait::async_trait;
use serde_json::json;

#[derive(Clone, Default)]
struct State {
    version: u64,
    records: BTreeMap<RecordId, Record>,
    events: Vec<MemoryEvent>,
    next_seq: u64,
}

impl State {
    fn push_event(
        &mut self,
        record_id: RecordId,
        kind: EventKind,
        actor: Actor,
        detail: Option<serde_json::Value>,
        at: Timestamp,
    ) {
        self.next_seq += 1;
        self.events.push(MemoryEvent {
            seq: self.next_seq,
            record_id,
            kind,
            actor,
            detail,
            at,
        });
    }

    fn insert(&mut self, rec: NewRecord, supersedes: Option<RecordId>) -> MemoryResult<RecordId> {
        rec.validate()?;
        let id = RecordId::new();
        let kind = if rec.status == Status::Active {
            EventKind::Captured
        } else {
            EventKind::Proposed
        };
        let (actor, at) = (rec.origin.actor(), rec.created_at);
        let mut detail = serde_json::Map::new();
        if let Some(old) = supersedes {
            detail.insert("supersedes".to_owned(), json!(old.to_string()));
        }
        if let Some(why) = &rec.rationale {
            detail.insert("rationale".to_owned(), json!(why));
        }
        let detail = (!detail.is_empty()).then(|| serde_json::Value::Object(detail));
        let mut record = rec.into_record(id);
        record.supersedes = supersedes;
        self.records.insert(id, record);
        self.push_event(id, kind, actor, detail, at);
        Ok(id)
    }
}

struct Staged {
    base: u64,
    state: State,
}

/// Clones share one committed state.
#[derive(Clone, Default)]
pub struct InMemoryMemoryRepository {
    state: Arc<Mutex<State>>,
}

impl InMemoryMemoryRepository {
    pub fn new() -> Self {
        Self::default()
    }

    fn committed(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().expect("memory repository state")
    }
}

fn staged(tx: &mut MemoryTx) -> MemoryResult<&mut Staged> {
    tx.state_mut::<Staged>()
}

#[async_trait]
impl MemoryRepository for InMemoryMemoryRepository {
    async fn begin(&self) -> MemoryResult<MemoryTx> {
        let state = self.committed().clone();
        Ok(MemoryTx::new(Staged {
            base: state.version,
            state,
        }))
    }

    async fn commit(&self, tx: MemoryTx) -> MemoryResult<()> {
        let staged = tx.into_state::<Staged>()?;
        let mut committed = self.committed();
        if committed.version != staged.base {
            return Err(MemoryError::Conflict(
                "another commit moved the repository".to_owned(),
            ));
        }
        *committed = staged.state;
        committed.version += 1;
        Ok(())
    }

    async fn insert(&self, tx: &mut MemoryTx, rec: NewRecord) -> MemoryResult<RecordId> {
        staged(tx)?.state.insert(rec, None)
    }

    async fn supersede(
        &self,
        tx: &mut MemoryTx,
        old: RecordId,
        new: NewRecord,
    ) -> MemoryResult<RecordId> {
        let state = &mut staged(tx)?.state;
        if new.status != Status::Active {
            return Err(MemoryError::InvalidInput(
                "a replacement record must be active".to_owned(),
            ));
        }
        let current = state.records.get(&old).ok_or(MemoryError::NotFound(old))?;
        if current.status != Status::Active || current.valid_to.is_some() {
            return Err(MemoryError::InvalidTransition {
                from: current.status,
                to: Status::Active,
            });
        }
        if current.scope != new.scope {
            return Err(MemoryError::InvalidInput(
                "a record supersedes only within its scope".to_owned(),
            ));
        }
        if new.valid_from < current.valid_from {
            return Err(MemoryError::InvalidInput(
                "a replacement cannot start before the record it replaces".to_owned(),
            ));
        }
        let (valid_from, actor) = (new.valid_from, new.origin.actor());
        let new_id = state.insert(new, Some(old))?;
        if let Some(record) = state.records.get_mut(&old) {
            record.valid_to = Some(valid_from);
            record.superseded_by = Some(new_id);
        }
        let detail = json!({ "by": new_id.to_string() });
        state.push_event(old, EventKind::Superseded, actor, Some(detail), valid_from);
        Ok(new_id)
    }

    async fn set_status(
        &self,
        tx: &mut MemoryTx,
        id: RecordId,
        to: Status,
        actor: Actor,
        at: Timestamp,
    ) -> MemoryResult<()> {
        let state = &mut staged(tx)?.state;
        let record = state
            .records
            .get_mut(&id)
            .ok_or(MemoryError::NotFound(id))?;
        if !record.status.can_transition_to(to) {
            return Err(MemoryError::InvalidTransition {
                from: record.status,
                to,
            });
        }
        record.status = to;
        match to {
            Status::Active => record.approved_at = Some(at),
            Status::Retracted if record.valid_to.is_none() => record.valid_to = Some(at),
            _ => {}
        }
        state.push_event(id, Status::event_into(to), actor, None, at);
        Ok(())
    }

    async fn set_core(
        &self,
        tx: &mut MemoryTx,
        id: RecordId,
        pin: Option<CorePin>,
        actor: Actor,
        at: Timestamp,
    ) -> MemoryResult<()> {
        let core = pin.is_some();
        let state = &mut staged(tx)?.state;
        let record = state
            .records
            .get_mut(&id)
            .ok_or(MemoryError::NotFound(id))?;
        if record.status != Status::Active || record.valid_to.is_some() {
            return Err(MemoryError::InvalidInput(
                "only a current active record can be pinned or unpinned".to_owned(),
            ));
        }
        record.is_core = core;
        record.core_order = pin.and_then(|pin| pin.order);
        if let Some(pin) = pin {
            // The staged state is a copy of the base the commit compares, so
            // this is the check a concurrent pin loses with a conflict.
            let scope = state.records[&id].scope.clone();
            let pinned: Vec<Record> = state
                .records
                .values()
                .filter(|r| {
                    r.is_core
                        && r.status == Status::Active
                        && r.valid_to.is_none()
                        && (r.scope == Scope::user() || r.scope == scope)
                })
                .cloned()
                .collect();
            let used = core_tokens(&pinned);
            if used > pin.max_tokens {
                return Err(MemoryError::InvalidInput(format!(
                    "pinning would use {used} of the {} core-block tokens; unpin or shorten an entry first",
                    pin.max_tokens
                )));
            }
        }
        let kind = if core {
            EventKind::Pinned
        } else {
            EventKind::Unpinned
        };
        state.push_event(id, kind, actor, None, at);
        Ok(())
    }

    async fn record_use(&self, ids: &[RecordId], at: Timestamp) -> MemoryResult<()> {
        let mut committed = self.committed();
        for id in ids {
            if let Some(record) = committed.records.get_mut(id) {
                record.last_used_at = Some(at);
                record.use_count += 1;
            }
        }
        Ok(())
    }

    async fn get(&self, id: RecordId) -> MemoryResult<Option<Record>> {
        Ok(self.committed().records.get(&id).cloned())
    }

    async fn list(&self, filter: RecordFilter, page: Page) -> MemoryResult<Vec<Record>> {
        let committed = self.committed();
        let mut rows: Vec<&Record> = committed
            .records
            .values()
            .filter(|record| filter.matches(record))
            .collect();
        rows.sort_by_key(|record| (record.created_at, record.id));
        Ok(rows
            .into_iter()
            .skip(page.offset as usize)
            .take(page.limit as usize)
            .cloned()
            .collect())
    }

    async fn core_block(&self, scopes: &[Scope]) -> MemoryResult<Vec<Record>> {
        let committed = self.committed();
        let mut rows: Vec<&Record> = committed
            .records
            .values()
            .filter(|record| {
                record.is_core
                    && record.status == Status::Active
                    && record.valid_to.is_none()
                    && scopes.contains(&record.scope)
            })
            .collect();
        rows.sort_by_key(|record| {
            (
                record.scope.kind,
                record.core_order.is_none(),
                record.core_order,
                record.id,
            )
        });
        Ok(rows.into_iter().cloned().collect())
    }

    async fn purge(
        &self,
        ids: &[RecordId],
        actor: Actor,
        at: Timestamp,
    ) -> MemoryResult<PurgeReport> {
        let mut committed = self.committed();
        let mut report = PurgeReport::default();
        for id in ids {
            if committed.records.remove(id).is_none() {
                continue;
            }
            *report
                .rows_removed
                .entry("memory_records".to_owned())
                .or_default() += 1;
            for event in committed.events.iter_mut().filter(|e| e.record_id == *id) {
                if event.detail.take().is_some() {
                    *report
                        .rows_removed
                        .entry("memory_events_scrubbed".to_owned())
                        .or_default() += 1;
                }
            }
            let needle = id.to_string();
            for event in committed.events.iter_mut().filter(|e| e.record_id != *id) {
                let names_id = event
                    .detail
                    .as_ref()
                    .is_some_and(|detail| detail.to_string().contains(&needle));
                if names_id {
                    event.detail = None;
                    report.neighbour_events_scrubbed += 1;
                }
            }
            for record in committed.records.values_mut() {
                if record.supersedes == Some(*id) {
                    record.supersedes = None;
                }
                if record.superseded_by == Some(*id) {
                    record.superseded_by = None;
                }
            }
            committed.push_event(*id, EventKind::Purged, actor, None, at);
        }
        report.residue = ids
            .iter()
            .filter(|id| committed.records.contains_key(*id))
            .count() as u64;
        committed.version += 1;
        Ok(report)
    }

    async fn events(&self, id: RecordId) -> MemoryResult<Vec<MemoryEvent>> {
        Ok(self
            .committed()
            .events
            .iter()
            .filter(|event| event.record_id == id)
            .cloned()
            .collect())
    }
}
