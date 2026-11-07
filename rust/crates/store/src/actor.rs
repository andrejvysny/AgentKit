use crate::{Result, StoreError, deadline::Deadlines, schema, transaction::TransactionScope};
use rusqlite::Connection;
use std::{
    panic::{AssertUnwindSafe, catch_unwind},
    path::Path,
    sync::{
        Arc, Weak,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::sync::{Mutex, mpsc, oneshot, watch};

pub(crate) type Job = Box<dyn FnOnce(&mut Connection) + Send>;
pub(crate) struct Worker {
    pub(crate) sender: Option<mpsc::UnboundedSender<Job>>,
    thread: Option<std::thread::JoinHandle<()>>,
    pub(crate) poisoned: AtomicBool,
    pub(crate) deadlines: Deadlines,
    pub(crate) ambient: watch::Sender<bool>,
}
impl Worker {
    pub(crate) fn send(&self, job: Job) -> Result<()> {
        if self.poisoned.load(Ordering::Acquire) {
            return Err(StoreError::RecoveryUnreadable(
                "transaction cleanup failed".into(),
            ));
        }
        self.send_cleanup(job)
    }
    pub(crate) fn send_cleanup(&self, job: Job) -> Result<()> {
        self.sender
            .as_ref()
            .ok_or(StoreError::Closed)?
            .send(job)
            .map_err(|_| StoreError::Closed)
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        self.sender.take();
        if let Some(thread) = self.thread.take()
            && thread.thread().id() != std::thread::current().id()
        {
            let _ = thread.join();
        }
    }
}

/// A panicking job can leave a transaction open; the next job must not
/// inherit it.
fn rollback_if_open(connection: &Connection) {
    if !connection.is_autocommit() {
        let _ = connection.execute_batch("ROLLBACK");
    }
}

pub(crate) fn scope_aborted() -> StoreError {
    StoreError::Internal(
        "transaction scope aborted: an operation panicked and rolled it back".into(),
    )
}

fn panic_message(panic: &(dyn std::any::Any + Send)) -> String {
    panic
        .downcast_ref::<&str>()
        .map(|message| (*message).to_owned())
        .or_else(|| panic.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "store job panicked".into())
}

/// Clones share one actor and database ownership lock.
#[derive(Clone)]
pub struct SqliteStore {
    pub(crate) worker: Arc<Worker>,
    pub(crate) gate: Arc<Mutex<()>>,
    pub(crate) scope: Option<Arc<TransactionScope>>,
    pub(crate) gate_timeout: Duration,
    pub(crate) aging: crate::TaskAging,
    /// Application-provided secret for sealing `connector_connections`
    /// credential values (AGENTKIT-72). `None` selects the store-local key.
    pub(crate) connector_credential_key: Option<Arc<Vec<u8>>>,
    /// When true, the store-local connector key is refused (see
    /// `with_connector_key_required`).
    pub(crate) connector_key_required: bool,
}

/// Opaque identity that does not keep a SQLite actor or its file lock alive.
#[derive(Clone)]
pub struct StoreActorIdentity {
    worker: Weak<Worker>,
}

impl StoreActorIdentity {
    pub fn matches(&self, store: &SqliteStore) -> bool {
        Weak::ptr_eq(&self.worker, &Arc::downgrade(&store.worker))
    }

    pub fn is_alive(&self) -> bool {
        self.worker.strong_count() != 0
    }
}
impl std::fmt::Debug for SqliteStore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SqliteStore")
            .finish_non_exhaustive()
    }
}
impl SqliteStore {
    /// True only for handles sharing the same live SQLite actor.
    pub fn same_actor(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.worker, &other.worker)
    }

    pub fn actor_identity(&self) -> StoreActorIdentity {
        StoreActorIdentity {
            worker: Arc::downgrade(&self.worker),
        }
    }

    /// `capacity` is reserved: the actor queue is unbounded today, so the
    /// value is only validated (must be positive) and kept for API stability.
    pub fn open(path: impl AsRef<Path>, capacity: usize) -> Result<Self> {
        if capacity == 0 {
            return Err(StoreError::InvalidInput(
                "channel capacity must be positive".into(),
            ));
        }
        let (mut connection, lock) = schema::open(path.as_ref())?;
        // Durability fault plan: unset environment means all-zero and this
        // thread-local bind plus the atomic counter are the whole cost.
        let faults = std::sync::Arc::new(crate::faults::StoreFaults::from_env());
        crate::faults::apply_synchronous(&connection, &faults)?;
        let deadlines = Deadlines::new()?;
        let (sender, mut receiver) = mpsc::unbounded_channel::<Job>();
        let (ambient, _) = watch::channel(false);
        let thread = std::thread::Builder::new()
            .name("agentkit-sqlite".into())
            .spawn(move || {
                crate::faults::bind(faults);
                let _ownership = lock;
                while let Some(job) = receiver.blocking_recv() {
                    // `call` jobs catch their own panics to answer the caller;
                    // this net keeps the actor alive for any other job.
                    if catch_unwind(AssertUnwindSafe(|| job(&mut connection))).is_err() {
                        rollback_if_open(&connection);
                    }
                }
            })?;
        Ok(Self {
            worker: Arc::new(Worker {
                sender: Some(sender),
                thread: Some(thread),
                poisoned: AtomicBool::new(false),
                deadlines,
                ambient,
            }),
            gate: Arc::new(Mutex::new(())),
            scope: None,
            gate_timeout: Duration::from_secs(30),
            aging: crate::TaskAging::default(),
            connector_credential_key: None,
            connector_key_required: false,
        })
    }
    pub fn memory(capacity: usize) -> Result<Self> {
        Self::open(":memory:", capacity)
    }
    /// Sets the maximum wait for a root operation blocked by an ambient transaction.
    pub fn with_transaction_gate_timeout(mut self, timeout: Duration) -> Self {
        self.gate_timeout = timeout;
        self
    }
    /// Opts `claim` into priority aging; the default store never ages.
    pub fn with_task_aging(mut self, aging: crate::TaskAging) -> Self {
        self.aging = aging;
        self
    }
    /// Supplies the application secret used to seal `connector_connections`
    /// credential values at rest (AGENTKIT-72). Production compositions keep
    /// this secret outside the database file (OS-keychain custody behind the
    /// `agentkit-connections` SecretStore port is the reference mechanism);
    /// without it the store falls back to a generated store-local key row.
    pub fn with_connector_credential_key(mut self, secret: impl Into<Vec<u8>>) -> Self {
        self.connector_credential_key = Some(Arc::new(secret.into()));
        self
    }
    /// Production posture for connector credentials: when `required` and no
    /// external key was injected, every connector credential seal/open that
    /// would need the store-local key fails with
    /// `InvalidInput("connector_credential_key_required: ...")`, and the
    /// store-local key row is never created. Default `false`.
    pub fn with_connector_key_required(mut self, required: bool) -> Self {
        self.connector_key_required = required;
        self
    }
    pub(crate) fn connector_crypto_source(&self) -> crate::connector_crypto::CryptoSource {
        crate::connector_crypto::CryptoSource {
            injected: self.connector_credential_key.clone(),
            required: self.connector_key_required,
        }
    }
    pub(crate) async fn lock_root(&self) -> Result<tokio::sync::OwnedMutexGuard<()>> {
        if let Ok(permit) = self.gate.clone().try_lock_owned() {
            return Ok(permit);
        }
        let mut ambient = self.worker.ambient.subscribe();
        let acquire = self.gate.clone().lock_owned();
        tokio::pin!(acquire);
        let mut deadline = None;
        let mut expires_at = None;
        loop {
            let active = *ambient.borrow_and_update();
            if active && deadline.is_none() {
                let expires = match expires_at {
                    Some(expires) => expires,
                    None => {
                        let expires = std::time::Instant::now()
                            .checked_add(self.gate_timeout)
                            .ok_or_else(|| {
                                StoreError::InvalidInput("transaction timeout too large".into())
                            })?;
                        expires_at = Some(expires);
                        expires
                    }
                };
                deadline = Some(
                    self.worker
                        .deadlines
                        .register(expires.saturating_duration_since(std::time::Instant::now()))?,
                );
            } else if !active {
                deadline = None;
            }
            if let Some((_, receiver)) = deadline.as_mut() {
                tokio::select! {
                    permit = &mut acquire => return Ok(permit),
                    _ = ambient.changed() => {},
                    _ = receiver => {
                        if *ambient.borrow_and_update() { return Err(StoreError::TransactionGateTimeout); }
                    },
                }
            } else {
                tokio::select! {
                    permit = &mut acquire => return Ok(permit),
                    _ = ambient.changed() => {},
                }
            }
        }
    }
    pub(crate) async fn call<T, F>(&self, operation: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> Result<T> + Send + 'static,
    {
        let permit = if let Some(scope) = &self.scope {
            scope.operation_gate.clone().lock_owned().await
        } else {
            self.lock_root().await?
        };
        let (sender, receiver) = oneshot::channel();
        let worker = self.worker.clone();
        let scope = self.scope.clone();
        let job = Box::new(move |connection: &mut Connection| {
            crate::transaction::take_cleanup_failure();
            if scope.as_ref().is_some_and(|scope| scope.is_failed()) {
                drop(permit);
                let _ = sender.send(Err(scope_aborted()));
                return;
            }
            // Read BEFORE the job runs: an ambient transaction is already
            // open here, and a bare ROLLBACK must not be mistaken for the
            // job's own cleanup.
            let was_autocommit = connection.is_autocommit();
            let mut result = match catch_unwind(AssertUnwindSafe(|| operation(connection))) {
                Ok(result) => result,
                Err(panic) => {
                    rollback_if_open(connection);
                    if !was_autocommit && let Some(scope) = &scope {
                        // The outer transaction is gone; every later job of
                        // this scope must fail rather than autocommit.
                        scope.mark_failed();
                    }
                    Err(StoreError::Internal(panic_message(panic.as_ref())))
                }
            };
            if crate::transaction::take_cleanup_failure() {
                worker.poisoned.store(true, Ordering::Release);
                if result.is_ok() {
                    result = Err(StoreError::RecoveryUnreadable(
                        "atomic operation cleanup failed".into(),
                    ));
                }
            }
            drop(permit);
            drop(worker);
            let _ = sender.send(result);
        });
        if let Some(scope) = &self.scope {
            let state = scope
                .state
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if !state.active {
                return Err(StoreError::TransactionScopeExpired);
            }
            if state.failed {
                return Err(scope_aborted());
            }
            self.worker.send(job)?;
        } else {
            self.worker.send(job)?;
        }
        receiver.await.map_err(|_| StoreError::Closed)?
    }
    pub async fn flush(&self) -> Result<()> {
        let result = self.call(|_| Ok(())).await;
        let releases = self.worker.deadlines.flush_releases().await;
        result.and(releases)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_panicking_job_rolls_back_replies_internal_and_keeps_the_actor_alive() {
        let store = SqliteStore::memory(2).unwrap();
        let error = store
            .call(|db| -> Result<()> {
                db.execute_batch("BEGIN; CREATE TABLE ghost(x);")?;
                panic!("boom");
            })
            .await
            .unwrap_err();
        assert!(
            matches!(&error, StoreError::Internal(message) if message == "boom"),
            "{error:?}"
        );
        // The open transaction was rolled back, so the table never existed,
        // and the next job runs normally.
        let exists = store
            .call(|db| {
                Ok(db.query_row(
                    "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='ghost')",
                    [],
                    |row| row.get::<_, bool>(0),
                )?)
            })
            .await
            .unwrap();
        assert!(!exists);
        store.flush().await.unwrap();
    }

    #[tokio::test]
    async fn a_panic_inside_an_ambient_transaction_aborts_the_whole_scope() {
        let directory = tempfile::tempdir().unwrap();
        let store = SqliteStore::open(directory.path().join("scope.db"), 2).unwrap();
        let transaction = store.begin_transaction().await.unwrap();
        let view = transaction.view();
        view.call(|db| {
            Ok(db.execute_batch("CREATE TABLE partial(x); INSERT INTO partial VALUES(1);")?)
        })
        .await
        .unwrap();
        let error = view
            .call(|_| -> Result<()> { panic!("mid-scope") })
            .await
            .unwrap_err();
        assert!(matches!(error, StoreError::Internal(_)), "{error:?}");
        // Later scope jobs must fail instead of autocommitting piecemeal.
        let later = view
            .call(|db| Ok(db.execute_batch("INSERT INTO partial VALUES(2);")?))
            .await;
        assert!(matches!(later, Err(StoreError::Internal(_))), "{later:?}");
        assert!(transaction.commit().await.is_err());
        // The worker is healthy and nothing from the scope persisted.
        let exists = store
            .call(|db| {
                Ok(db.query_row(
                    "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='partial')",
                    [],
                    |row| row.get::<_, bool>(0),
                )?)
            })
            .await
            .unwrap();
        assert!(!exists);
        store
            .begin_transaction()
            .await
            .unwrap()
            .rollback()
            .await
            .unwrap();
    }
}
