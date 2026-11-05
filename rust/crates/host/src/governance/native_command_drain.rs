use agentkit_core::CoreError;
use agentkit_store::{SqliteStore, StoreActorIdentity};
use std::{
    collections::HashMap,
    sync::{LazyLock, Mutex},
    time::Duration,
};
use tokio::sync::Notify;

struct Completion {
    actor: StoreActorIdentity,
    error: Option<String>,
}

static PENDING: LazyLock<Mutex<HashMap<String, Completion>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static CHANGED: Notify = Notify::const_new();

pub(super) struct CompletionGuard {
    id: String,
    finished: bool,
}

impl CompletionGuard {
    pub(super) fn register(store: &SqliteStore) -> Self {
        let id = uuid::Uuid::new_v4().to_string();
        let mut pending = PENDING.lock().unwrap_or_else(|error| error.into_inner());
        pending.retain(|_, completion| completion.actor.is_alive());
        pending.insert(
            id.clone(),
            Completion {
                actor: store.actor_identity(),
                error: None,
            },
        );
        Self {
            id,
            finished: false,
        }
    }

    pub(super) fn finish(mut self, error: Option<String>) {
        self.record(error);
        self.finished = true;
    }

    fn record(&self, error: Option<String>) {
        let mut pending = PENDING.lock().unwrap_or_else(|error| error.into_inner());
        match error {
            Some(error) => {
                if let Some(completion) = pending.get_mut(&self.id) {
                    completion.error = Some(error);
                }
            }
            None => {
                pending.remove(&self.id);
            }
        }
        drop(pending);
        CHANGED.notify_waiters();
    }
}

impl Drop for CompletionGuard {
    fn drop(&mut self) {
        if !self.finished {
            self.record(Some(
                "native command completion task dropped before durable result".into(),
            ));
        }
    }
}

/// Wait for this actor's host core producers and native command receipts.
/// The caller must stop admission and close its packs before draining.
/// A timeout or completion failure reports uncertainty instead of settlement.
pub async fn drain_native_command_completions(
    store: &SqliteStore,
    timeout: Duration,
) -> Result<(), CoreError> {
    let drain = async {
        loop {
            let changed = CHANGED.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if snapshot(store)? {
                return Ok(());
            }
            changed.await;
        }
    };
    tokio::time::timeout(timeout, drain).await.map_err(|_| {
        CoreError::new(
            "command_drain_timeout",
            "Native command receipt completion did not drain; effects remain uncertain",
        )
    })?
}

fn snapshot(store: &SqliteStore) -> Result<bool, CoreError> {
    let mut pending = PENDING.lock().unwrap_or_else(|error| error.into_inner());
    pending.retain(|_, completion| completion.actor.is_alive());
    let failures = pending
        .iter()
        .filter(|(_, completion)| completion.actor.matches(store))
        .filter_map(|(id, completion)| {
            completion
                .error
                .as_ref()
                .map(|error| (id.clone(), error.clone()))
        })
        .collect::<Vec<_>>();
    if !failures.is_empty() {
        for (id, _) in &failures {
            pending.remove(id);
        }
        return Err(CoreError::new("command_completion_failed", &failures[0].1));
    }
    Ok(!pending
        .values()
        .any(|completion| completion.actor.matches(store)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn native_completion_drain_uses_actor_identity_and_waits_for_completion() {
        let first = SqliteStore::memory(8).unwrap();
        let second = SqliteStore::memory(8).unwrap();
        assert!(first.same_actor(&first.clone()));
        assert!(!first.same_actor(&second));
        let completion = CompletionGuard::register(&first);
        drain_native_command_completions(&second, Duration::from_millis(20))
            .await
            .unwrap();
        let error = drain_native_command_completions(&first, Duration::from_millis(20))
            .await
            .unwrap_err();
        assert_eq!(error.code, "command_drain_timeout");
        completion.finish(None);
        drain_native_command_completions(&first, Duration::from_secs(1))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn native_completion_drain_reports_a_failed_completion() {
        let store = SqliteStore::memory(8).unwrap();
        CompletionGuard::register(&store).finish(Some("receipt persistence failed".into()));
        let error = drain_native_command_completions(&store, Duration::from_secs(1))
            .await
            .unwrap_err();
        assert_eq!(error.code, "command_completion_failed");
    }

    #[tokio::test]
    async fn native_completion_drain_reports_a_dropped_completion_task() {
        let store = SqliteStore::memory(8).unwrap();
        drop(CompletionGuard::register(&store));
        let error = drain_native_command_completions(&store, Duration::from_secs(1))
            .await
            .unwrap_err();
        assert_eq!(error.code, "command_completion_failed");
    }

    #[tokio::test]
    async fn failed_native_completion_tracker_does_not_retain_database_ownership() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("store.sqlite");
        let store = SqliteStore::open(&path, 8).unwrap();
        let identity = store.actor_identity();
        CompletionGuard::register(&store).finish(Some("fixture completion failure".into()));
        drop(store);
        assert!(!identity.is_alive());
        let reopened = SqliteStore::open(&path, 8)
            .expect("tracker must not retain the old actor's ownership lock");
        assert!(!identity.matches(&reopened));
        drain_native_command_completions(&reopened, Duration::from_millis(20))
            .await
            .unwrap();
    }
}
