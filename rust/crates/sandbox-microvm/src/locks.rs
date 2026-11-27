//! One lock per workspace id, so `ensure`, `stop`, `reset_home` and
//! `destroy` of one workspace never interleave while other workspaces
//! proceed. An entry lives only while someone holds or waits for it.

use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex, PoisonError};

use tokio::sync::{Mutex, OwnedMutexGuard};

use crate::naming::WorkspaceId;

/// The per-workspace locks.
#[derive(Debug, Default)]
pub(crate) struct WorkspaceLocks {
    entries: StdMutex<HashMap<String, Arc<Mutex<()>>>>,
}

/// Held while one workspace's request runs.
#[derive(Debug)]
pub(crate) struct WorkspaceGuard<'a> {
    locks: &'a WorkspaceLocks,
    key: String,
    guard: Option<OwnedMutexGuard<()>>,
}

impl WorkspaceLocks {
    /// Wait for and take the lock of `ws`.
    pub(crate) async fn lock(&self, ws: &WorkspaceId) -> WorkspaceGuard<'_> {
        let entry = {
            let mut entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
            Arc::clone(entries.entry(ws.as_str().to_string()).or_default())
        };
        let guard = entry.lock_owned().await;
        WorkspaceGuard {
            locks: self,
            key: ws.as_str().to_string(),
            guard: Some(guard),
        }
    }
}

impl Drop for WorkspaceGuard<'_> {
    fn drop(&mut self) {
        // Release first, then drop the entry if nobody else holds a clone
        // (a waiter holds one from before it waits).
        drop(self.guard.take());
        let mut entries = self
            .locks
            .entries
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if entries
            .get(&self.key)
            .is_some_and(|entry| Arc::strong_count(entry) == 1)
        {
            entries.remove(&self.key);
        }
    }
}
