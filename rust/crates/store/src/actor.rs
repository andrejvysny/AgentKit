use crate::{Result, StoreError, deadline::Deadlines, schema, transaction::TransactionScope};
use rusqlite::Connection;
use std::{
    path::Path,
    sync::{
        Arc,
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

/// Clones share one actor and database ownership lock.
#[derive(Clone)]
pub struct SqliteStore {
    pub(crate) worker: Arc<Worker>,
    pub(crate) gate: Arc<Mutex<()>>,
    pub(crate) scope: Option<Arc<TransactionScope>>,
    pub(crate) gate_timeout: Duration,
}
impl std::fmt::Debug for SqliteStore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SqliteStore")
            .finish_non_exhaustive()
    }
}
impl SqliteStore {
    pub fn open(path: impl AsRef<Path>, capacity: usize) -> Result<Self> {
        if capacity == 0 {
            return Err(StoreError::InvalidInput(
                "channel capacity must be positive".into(),
            ));
        }
        let (mut connection, lock) = schema::open(path.as_ref())?;
        let deadlines = Deadlines::new()?;
        let (sender, mut receiver) = mpsc::unbounded_channel::<Job>();
        let (ambient, _) = watch::channel(false);
        let thread = std::thread::Builder::new()
            .name("agentkit-sqlite".into())
            .spawn(move || {
                let _ownership = lock;
                while let Some(job) = receiver.blocking_recv() {
                    job(&mut connection);
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
        let job = Box::new(move |connection: &mut Connection| {
            crate::transaction::take_cleanup_failure();
            let mut result = operation(connection);
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
