//! Cancellation-safe scoped transactions and operation savepoints.
use crate::{Result, SqliteStore, StoreError, actor::Worker};
use rusqlite::{Connection, Savepoint, Transaction, TransactionBehavior};
use std::{
    ops::Deref,
    sync::{Arc, Mutex, atomic::Ordering},
};
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard, oneshot};

thread_local! {
    static CLEANUP_FAILED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}
pub(crate) fn take_cleanup_failure() -> bool {
    CLEANUP_FAILED.with(|flag| flag.replace(false))
}
fn note_failure(result: &rusqlite::Result<()>) {
    if result.is_err() {
        CLEANUP_FAILED.with(|flag| flag.set(true));
    }
}
enum AtomicInner<'a> {
    Transaction(Transaction<'a>),
    Savepoint(Savepoint<'a>),
}
pub(crate) struct Atomic<'a> {
    inner: Option<AtomicInner<'a>>,
}
impl Deref for Atomic<'_> {
    type Target = Connection;
    fn deref(&self) -> &Connection {
        match self.inner.as_ref().expect("active atomic operation") {
            AtomicInner::Transaction(tx) => tx,
            AtomicInner::Savepoint(tx) => tx,
        }
    }
}
impl Atomic<'_> {
    pub(crate) fn commit(mut self) -> rusqlite::Result<()> {
        let inner = self.inner.take().expect("active atomic operation");
        let nested = matches!(inner, AtomicInner::Savepoint(_));
        let result = match inner {
            AtomicInner::Transaction(tx) => tx.commit(),
            AtomicInner::Savepoint(tx) => tx.commit(),
        };
        if nested {
            note_failure(&result);
        }
        result
    }
    pub(crate) fn rollback(mut self) -> rusqlite::Result<()> {
        let inner = self.inner.take().expect("active atomic operation");
        let nested = matches!(inner, AtomicInner::Savepoint(_));
        let result = rollback(inner);
        if nested {
            note_failure(&result);
        }
        result
    }
}
fn rollback(inner: AtomicInner<'_>) -> rusqlite::Result<()> {
    match inner {
        AtomicInner::Transaction(tx) => tx.rollback(),
        AtomicInner::Savepoint(mut tx) => {
            tx.rollback()?;
            tx.commit()
        }
    }
}
impl Drop for Atomic<'_> {
    fn drop(&mut self) {
        if let Some(inner) = self.inner.take() {
            let nested = matches!(inner, AtomicInner::Savepoint(_));
            let result = rollback(inner);
            if nested {
                note_failure(&result);
            }
        }
    }
}
pub(crate) fn atomic(db: &mut Connection) -> rusqlite::Result<Atomic<'_>> {
    atomic_with_behavior(db, TransactionBehavior::Deferred)
}
pub(crate) fn atomic_with_behavior(
    db: &mut Connection,
    behavior: TransactionBehavior,
) -> rusqlite::Result<Atomic<'_>> {
    if db.is_autocommit() {
        Ok(Atomic {
            inner: Some(AtomicInner::Transaction(
                db.transaction_with_behavior(behavior)?,
            )),
        })
    } else {
        Ok(Atomic {
            inner: Some(AtomicInner::Savepoint(db.savepoint()?)),
        })
    }
}

pub(crate) struct ScopeState {
    pub(crate) active: bool,
    root_guard: Option<OwnedMutexGuard<()>>,
}
pub(crate) struct TransactionScope {
    worker: Option<Arc<Worker>>,
    pub(crate) state: Mutex<ScopeState>,
    pub(crate) operation_gate: Arc<AsyncMutex<()>>,
}
impl Drop for TransactionScope {
    fn drop(&mut self) {
        if let Some(worker) = self.worker.take() {
            let queue = worker.deadlines.release_queue();
            queue.release(worker);
        }
    }
}

/// Exclusive ambient transaction. Dropping its owner queues rollback.
pub struct StoreTransaction {
    view: SqliteStore,
    owner: bool,
}
impl StoreTransaction {
    pub fn view(&self) -> SqliteStore {
        self.view.clone()
    }
    pub async fn commit(self) -> Result<()> {
        self.finish(true).await
    }
    /// Roll back the outer transaction; a nested transaction is flattened.
    pub async fn rollback(self) -> Result<()> {
        self.finish(false).await
    }
    async fn finish(mut self, commit: bool) -> Result<()> {
        if !self.owner {
            let scope = self.view.scope.as_ref().expect("transaction scope");
            return if scope
                .state
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .active
            {
                Ok(())
            } else {
                Err(StoreError::TransactionScopeExpired)
            };
        }
        let scope = self.view.scope.as_ref().expect("transaction scope");
        let permit = scope.operation_gate.clone().lock_owned().await;
        let (sender, receiver) = oneshot::channel();
        {
            let mut state = scope
                .state
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if !state.active {
                return Err(StoreError::TransactionScopeExpired);
            }
            state.active = false;
            let guard = state.root_guard.take().expect("active root guard");
            let worker = self.view.worker.clone();
            self.view.worker.send_cleanup(Box::new(move |db| {
                let _guard = guard;
                let _permit = permit;
                let result = finish_sql(db, &worker, commit);
                worker.ambient.send_replace(false);
                let _ = sender.send(result);
            }))?;
            self.owner = false;
        }
        receiver.await.map_err(|_| StoreError::Closed)?
    }
}
fn finish_sql(db: &Connection, worker: &Worker, commit: bool) -> Result<()> {
    if !commit {
        let result = db.execute_batch("ROLLBACK").map_err(StoreError::from);
        if result.is_err() {
            worker.poisoned.store(true, Ordering::Release);
        }
        return result;
    }
    let result = if worker.poisoned.load(Ordering::Acquire) {
        Err(StoreError::RecoveryUnreadable(
            "atomic operation cleanup failed".into(),
        ))
    } else {
        db.execute_batch("COMMIT").map_err(StoreError::from)
    };
    if result.is_err() && db.execute_batch("ROLLBACK").is_err() {
        worker.poisoned.store(true, Ordering::Release);
    }
    result
}
impl Drop for StoreTransaction {
    fn drop(&mut self) {
        if !self.owner {
            return;
        }
        let Some(scope) = &self.view.scope else {
            return;
        };
        let mut state = scope
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if !state.active {
            return;
        }
        state.active = false;
        let Some(guard) = state.root_guard.take() else {
            return;
        };
        let worker = self.view.worker.clone();
        let job = Box::new(move |db: &mut Connection| {
            let _guard = guard;
            if db.execute_batch("ROLLBACK").is_err() {
                worker.poisoned.store(true, Ordering::Release);
            }
            worker.ambient.send_replace(false);
        });
        let _ = self.view.worker.send_cleanup(job);
    }
}
impl SqliteStore {
    pub async fn begin_transaction(&self) -> Result<StoreTransaction> {
        if let Some(scope) = &self.scope {
            let state = scope
                .state
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if !state.active {
                return Err(StoreError::TransactionScopeExpired);
            }
            return Ok(StoreTransaction {
                view: self.clone(),
                owner: false,
            });
        }
        let guard = self.lock_root().await?;
        self.worker.ambient.send_replace(true);
        let (sender, receiver) = oneshot::channel();
        let root = self.clone();
        let enqueued = self.worker.send(Box::new(move |db| {
            if let Err(error) = db.execute_batch("BEGIN IMMEDIATE") {
                root.worker.ambient.send_replace(false);
                let _ = sender.send(Err(StoreError::from(error)));
                return;
            }
            let scope = Arc::new(TransactionScope {
                worker: Some(root.worker.clone()),
                state: Mutex::new(ScopeState {
                    active: true,
                    root_guard: Some(guard),
                }),
                operation_gate: Arc::new(AsyncMutex::new(())),
            });
            let view = SqliteStore {
                scope: Some(scope),
                ..root
            };
            let _ = sender.send(Ok(StoreTransaction { view, owner: true }));
        }));
        if let Err(error) = enqueued {
            self.worker.ambient.send_replace(false);
            return Err(error);
        }
        receiver.await.map_err(|_| StoreError::Closed)?
    }
}

#[cfg(test)]
#[path = "transaction_tests.rs"]
mod tests;
