//! The native store handle.
//!
//! One handle owns one SQLite actor. Ownership is the honest part: the
//! store enforces single ownership of the file — a second open on the same
//! path (in this or another process) fails with `already_owned` rather than
//! double-driving the database. `close()` flushes durably and releases the
//! last clone so the actor thread exits; closing twice is a no-op.

use std::path::Path;
use std::sync::Mutex;

use agentkit_store::SqliteStore;
use napi::bindgen_prelude::*;
use napi::threadsafe_function::ThreadsafeFunction;

use crate::errors;

#[napi(js_name = "NativeStore")]
pub struct NativeStore {
    inner: Mutex<Option<SqliteStore>>,
}

#[napi]
impl NativeStore {
    /// Open (creating if necessary) a durable store file. A second open of
    /// the same path while it is owned is rejected with `already_owned`.
    #[napi(factory)]
    pub fn open(path: String) -> Result<Self> {
        let capacity = default_capacity()?;
        let store = SqliteStore::open(Path::new(&path), capacity).map_err(errors::store)?;
        Ok(Self {
            inner: Mutex::new(Some(store)),
        })
    }

    /// Open an in-memory store (tests, throwaway sessions).
    #[napi(factory)]
    pub fn memory() -> Result<Self> {
        let capacity = default_capacity()?;
        let store = SqliteStore::memory(capacity).map_err(errors::store)?;
        Ok(Self {
            inner: Mutex::new(Some(store)),
        })
    }

    /// Flush all pending writes durably and release ownership. Idempotent.
    #[napi]
    pub async fn close(&self) -> Result<()> {
        let store = self.take()?;
        store.flush().await.map_err(errors::store)?;
        drop(store);
        Ok(())
    }

    /// Create the session service over this store: host + chat executor +
    /// durable session service + background worker, with crash recovery run
    /// before the worker claims any work. `onEvent` receives every
    /// provider/tool callback as a JSON envelope.
    #[napi]
    pub async fn create_service(
        &self,
        on_event: ThreadsafeFunction<String>,
    ) -> Result<crate::service::SessionService> {
        crate::service::SessionService::build(self.handle()?, on_event).await
    }
}

impl NativeStore {
    /// A fresh clone for a service handle. Errors once `close()` released it.
    pub(crate) fn handle(&self) -> Result<SqliteStore> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
            .ok_or_else(|| errors::closed("the native store is closed"))
    }

    fn take(&self) -> Result<SqliteStore> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
            .ok_or_else(|| errors::closed("the native store is closed"))
    }
}

fn default_capacity() -> Result<usize> {
    // The store actor rejects a zero queue; 64 is the capacity the host
    // tests use and comfortably exceeds what the binding itself enqueues.
    const QUEUE_CAPACITY: usize = 64;
    Ok(QUEUE_CAPACITY)
}
