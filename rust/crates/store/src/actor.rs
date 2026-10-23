use crate::{Result, StoreError, schema};
use rusqlite::Connection;
use std::{path::Path, sync::Arc};
use tokio::sync::{mpsc, oneshot};

type Job = Box<dyn FnOnce(&mut Connection) + Send>;

struct Worker {
    sender: Option<mpsc::Sender<Job>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.sender.take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Clones share one actor and one database ownership lock.
#[derive(Clone)]
pub struct SqliteStore {
    worker: Arc<Worker>,
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
        let (sender, mut receiver) = mpsc::channel::<Job>(capacity);
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
            }),
        })
    }

    pub fn memory(capacity: usize) -> Result<Self> {
        Self::open(":memory:", capacity)
    }

    pub(crate) async fn call<T, F>(&self, operation: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> Result<T> + Send + 'static,
    {
        let (sender, receiver) = oneshot::channel();
        self.worker
            .sender
            .as_ref()
            .ok_or(StoreError::Closed)?
            .send(Box::new(move |connection| {
                let _ = sender.send(operation(connection));
            }))
            .await
            .map_err(|_| StoreError::Closed)?;
        receiver.await.map_err(|_| StoreError::Closed)?
    }

    /// Wait for all previously queued work; does not close other clones.
    pub async fn flush(&self) -> Result<()> {
        self.call(|_| Ok(())).await
    }
}
