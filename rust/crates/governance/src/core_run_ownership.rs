use super::native_command_drain::CompletionGuard;
use agentkit_core::RunResult;
use agentkit_store::SqliteStore;
use tokio::task::{AbortHandle, JoinError, JoinHandle};

/// Host-owned producer lifetime. Public core `run_chat` drop semantics remain
/// unchanged; dropping this host scope aborts its producer and keeps the join
/// registered until every producer-owned context has actually been released.
pub(crate) struct OwnedCoreRun {
    producer: AbortHandle,
    join: JoinHandle<Result<RunResult, JoinError>>,
    finished: bool,
}

impl OwnedCoreRun {
    pub(crate) fn new(store: &SqliteStore, producer: JoinHandle<RunResult>) -> Self {
        let abort = producer.abort_handle();
        let completion = CompletionGuard::register(store);
        let join = tokio::spawn(async move {
            let result = producer.await;
            let error = result
                .as_ref()
                .err()
                .filter(|error| !error.is_cancelled())
                .map(ToString::to_string);
            completion.finish(error);
            result
        });
        Self {
            producer: abort,
            join,
            finished: false,
        }
    }

    pub(crate) async fn result(&mut self) -> Result<RunResult, JoinError> {
        let result = (&mut self.join).await?;
        self.finished = true;
        result
    }
}

impl Drop for OwnedCoreRun {
    fn drop(&mut self) {
        if !self.finished {
            self.producer.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drain_native_command_completions;
    use std::{
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        time::Duration,
    };
    use tokio::sync::Notify;

    struct Dropped(Arc<AtomicBool>);
    impl Drop for Dropped {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    #[tokio::test]
    async fn dropped_host_core_scope_joins_producer_before_runtime_drain() {
        let store = SqliteStore::memory(8).unwrap();
        let released = Arc::new(AtomicBool::new(false));
        let entered = Arc::new(Notify::new());
        let producer = {
            let (store, released, entered) = (store.clone(), released.clone(), entered.clone());
            tokio::spawn(async move {
                let _store = store;
                let _released = Dropped(released);
                entered.notify_one();
                std::future::pending::<RunResult>().await
            })
        };
        let scope = OwnedCoreRun::new(&store, producer);
        entered.notified().await;
        drop(scope);
        drain_native_command_completions(&store, Duration::from_secs(1))
            .await
            .unwrap();
        assert!(
            released.load(Ordering::SeqCst),
            "producer contexts must be dropped before drain resolves"
        );
    }

    #[tokio::test]
    async fn core_producer_panic_remains_a_drain_error() {
        let store = SqliteStore::memory(8).unwrap();
        let producer = tokio::spawn(async {
            panic!("fixture producer panic");
        });
        let mut scope = OwnedCoreRun::new(&store, producer);
        assert!(scope.result().await.unwrap_err().is_panic());
        let error = drain_native_command_completions(&store, Duration::from_secs(1))
            .await
            .unwrap_err();
        assert_eq!(error.code, "command_completion_failed");
    }
}
