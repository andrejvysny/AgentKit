//! Serialises mutations that target the same file.
//! Ported from `packages/toolpacks-coding/src/mutation-queue.ts`
//! (adapted from Pi, MIT, Copyright (c) 2025 Mario Zechner).
//!
//! Operations on the same realpath run one at a time; others stay parallel.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};

use tokio::sync::Mutex as AsyncMutex;
use tokio_util::sync::CancellationToken;

use crate::errors::CodingError;

static QUEUES: LazyLock<Mutex<HashMap<String, Arc<Tail>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Serialises registrations the way the TS module-global promise chain does:
/// whoever registers first is the first to join its key's queue (FIFO).
static REGISTRATION: LazyLock<AsyncMutex<()>> = LazyLock::new(|| AsyncMutex::new(()));

fn io_error(error: &std::io::Error) -> CodingError {
    CodingError::new("io_error", format!("{error}"))
}

fn is_missing_path_error(error: &std::io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(libc::ENOENT) | Some(libc::ENOTDIR)
    )
}

/// `path.resolve(filePath)`: anchor relative paths at the cwd, then apply
/// `.`/`..`/empty-segment normalisation without touching the filesystem.
fn resolve_path(file_path: &str) -> Result<String, CodingError> {
    let joined = if file_path.starts_with('/') {
        file_path.to_owned()
    } else {
        let cwd = std::env::current_dir().map_err(|error| io_error(&error))?;
        format!("{}/{}", cwd.display(), file_path)
    };
    let mut parts: Vec<&str> = Vec::new();
    for part in joined.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    Ok(format!("/{}", parts.join("/")))
}

async fn queue_key(file_path: &str) -> Result<String, CodingError> {
    let resolved = resolve_path(file_path)?;
    match tokio::fs::canonicalize(&resolved).await {
        Ok(path) => Ok(path.to_string_lossy().into_owned()),
        Err(error) if is_missing_path_error(&error) => Ok(resolved),
        Err(error) => Err(io_error(&error)),
    }
}

/// One link in a key's chain: wait for every earlier link, then for our own
/// release (TS's `current.then(() => next)`).
struct Tail {
    prev: Option<Arc<Tail>>,
    done: CancellationToken,
}

impl Tail {
    fn wait(&self) -> futures::future::BoxFuture<'_, ()> {
        Box::pin(async move {
            if let Some(prev) = &self.prev {
                prev.wait().await;
            }
            self.done.cancelled().await;
        })
    }
}

/// Releases the slot on drop — the `finally` that runs even if `f` panics.
struct Guard {
    key: String,
    tail: Arc<Tail>,
}

impl Drop for Guard {
    fn drop(&mut self) {
        self.tail.done.cancel();
        let mut queues = QUEUES.lock().expect("mutation queues");
        if queues
            .get(&self.key)
            .is_some_and(|tail| Arc::ptr_eq(tail, &self.tail))
        {
            queues.remove(&self.key);
        }
    }
}

/// Operations on the same realpath run one at a time; others stay parallel.
pub async fn with_file_mutation_queue<T, F, Fut>(file_path: &str, f: F) -> Result<T, CodingError>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<T, CodingError>>,
{
    let registration = REGISTRATION.lock().await;
    let key = queue_key(file_path).await?;
    let tail = {
        let mut queues = QUEUES.lock().expect("mutation queues");
        let tail = Arc::new(Tail {
            prev: queues.get(&key).cloned(),
            done: CancellationToken::new(),
        });
        queues.insert(key.clone(), Arc::clone(&tail));
        tail
    };
    drop(registration);
    // Arm the releaser BEFORE waiting: the harness can DROP this future
    // while it waits its turn (core tool timeout/abort), and JS — where the
    // analogous `await current` can never be abandoned — never needs this.
    // Dropping the guard cancels this link's `done`, so successors (whose
    // chain still waits on every EARLIER link, so mutual exclusion holds)
    // are not stranded behind a phantom waiter.
    let _guard = Guard {
        key,
        tail: Arc::clone(&tail),
    };
    if let Some(prev) = &tail.prev {
        prev.wait().await;
    }
    f().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn temp_file(name: &str) -> String {
        let path = std::env::temp_dir().join(format!(
            "ak-mutation-{}-{name}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(path.parent().expect("parent")).expect("create temp dir");
        std::fs::write(&path, b"x").expect("create temp file");
        path.to_string_lossy().into_owned()
    }

    async fn claim(
        active: &Arc<AtomicUsize>,
        peak: &Arc<AtomicUsize>,
        hold: Duration,
    ) -> Result<(), CodingError> {
        let now = active.fetch_add(1, Ordering::SeqCst) + 1;
        peak.fetch_max(now, Ordering::SeqCst);
        tokio::time::sleep(hold).await;
        active.fetch_sub(1, Ordering::SeqCst);
        Ok(())
    }

    use std::time::Duration;

    #[tokio::test(flavor = "multi_thread")]
    async fn same_file_mutations_serialise() {
        let path = temp_file("same");
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let tasks: Vec<_> = (0..4)
            .map(|_| {
                let (path, active, peak) = (path.clone(), Arc::clone(&active), Arc::clone(&peak));
                tokio::spawn(async move {
                    with_file_mutation_queue(&path, || {
                        claim(&active, &peak, Duration::from_millis(20))
                    })
                    .await
                })
            })
            .collect();
        for task in tasks {
            task.await.expect("join").expect("queue");
        }
        assert_eq!(peak.load(Ordering::SeqCst), 1, "one at a time");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn different_files_run_in_parallel() {
        let one = temp_file("par-a");
        let two = temp_file("par-b");
        // Both bodies wait for the other to arrive: only real parallelism
        // lets this finish before the timeout.
        let arrived = Arc::new(tokio::sync::Barrier::new(2));
        let one_task = {
            let (one, arrived) = (one.clone(), Arc::clone(&arrived));
            tokio::spawn(async move {
                with_file_mutation_queue(&one, || async move {
                    arrived.wait().await;
                    Ok(())
                })
                .await
            })
        };
        let two_task = {
            let (two, arrived) = (two.clone(), Arc::clone(&arrived));
            tokio::spawn(async move {
                with_file_mutation_queue(&two, || async move {
                    arrived.wait().await;
                    Ok(())
                })
                .await
            })
        };
        let joined = tokio::time::timeout(Duration::from_secs(2), async {
            one_task.await.expect("join").expect("queue");
            two_task.await.expect("join").expect("queue");
        })
        .await;
        assert!(joined.is_ok(), "different files must not block each other");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_symlink_alias_shares_the_queue() {
        let target = temp_file("alias-target");
        let alias = temp_file("alias-link");
        std::fs::remove_file(&alias).expect("remove");
        std::os::unix::fs::symlink(&target, &alias).expect("symlink");
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let first = {
            let (target, active, peak) = (target.clone(), Arc::clone(&active), Arc::clone(&peak));
            tokio::spawn(async move {
                with_file_mutation_queue(&target, || {
                    claim(&active, &peak, Duration::from_millis(40))
                })
                .await
            })
        };
        tokio::time::sleep(Duration::from_millis(10)).await;
        let second = {
            let (alias, active, peak) = (alias.clone(), Arc::clone(&active), Arc::clone(&peak));
            tokio::spawn(async move {
                with_file_mutation_queue(&alias, || {
                    claim(&active, &peak, Duration::from_millis(40))
                })
                .await
            })
        };
        first.await.expect("join").expect("queue");
        second.await.expect("join").expect("queue");
        assert_eq!(peak.load(Ordering::SeqCst), 1, "alias joins the same queue");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_failing_mutation_releases_the_queue() {
        let path = temp_file("fail");
        let failed = with_file_mutation_queue(&path, || async {
            Err::<(), CodingError>(CodingError::new("file_changed", "boom"))
        })
        .await;
        assert_eq!(failed.expect_err("propagates").code, "file_changed");
        // The slot must be free again.
        with_file_mutation_queue(&path, || async { Ok(()) })
            .await
            .expect("queue is reusable");
        let key = tokio::fs::canonicalize(&path)
            .await
            .expect("canonical")
            .to_string_lossy()
            .into_owned();
        let lingering = QUEUES.lock().expect("queues");
        assert!(!lingering.contains_key(&key), "the key is cleaned up");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn waiters_run_in_registration_order() {
        let path = temp_file("fifo");
        let key = queue_key(&path).await.expect("queue key");
        let (started_tx, started_rx) = tokio::sync::oneshot::channel::<()>();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
        let order = Arc::new(Mutex::new(Vec::new()));
        let blocker = {
            let (path, order) = (path.clone(), Arc::clone(&order));
            tokio::spawn(async move {
                with_file_mutation_queue(&path, || async move {
                    order.lock().expect("order").push("first");
                    let _ = started_tx.send(());
                    let _ = release_rx.await;
                    Ok(())
                })
                .await
            })
        };
        tokio::time::timeout(Duration::from_secs(5), started_rx)
            .await
            .expect("blocker starts promptly")
            .expect("blocker started");
        let mut waiters = Vec::new();
        for (index, name) in ["second", "third"].into_iter().enumerate() {
            let (path, order) = (path.clone(), Arc::clone(&order));
            waiters.push(tokio::spawn(async move {
                let queued = Arc::clone(&order);
                with_file_mutation_queue(&path, move || {
                    let order = queued;
                    async move {
                        order.lock().expect("order").push(name);
                        Ok(())
                    }
                })
                .await
            }));
            // Spawn order is not registration order on a multithreaded
            // runtime. Observe this waiter's actual queue link before
            // spawning the next one; the blocker keeps every link alive.
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    let registered = {
                        let queues = QUEUES.lock().expect("queues");
                        let mut current = queues.get(&key).map(Arc::as_ref);
                        let mut depth = 0;
                        while let Some(tail) = current {
                            depth += 1;
                            current = tail.prev.as_deref();
                        }
                        depth
                    };
                    if registered == index + 2 {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("waiter registered behind the blocker");
        }
        release_tx.send(()).expect("release");
        blocker.await.expect("join").expect("queue");
        for waiter in waiters {
            waiter.await.expect("join").expect("queue");
        }
        assert_eq!(
            *order.lock().expect("order"),
            vec!["first", "second", "third"],
            "FIFO fairness"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_abandoned_waiter_does_not_wedge_the_queue() {
        // The core harness can DROP a queued tool future (tool timeout,
        // abort). JS cannot abandon its `await current`, so only the Rust
        // port must ensure the abandoned link unblocks its successors.
        let path = temp_file("abandon");
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
        let blocker = {
            let path = path.clone();
            tokio::spawn(async move {
                with_file_mutation_queue(&path, || {
                    let (started_tx, release_rx) = (started_tx, release_rx);
                    async move {
                        let _ = started_tx.send(());
                        let _ = release_rx.await;
                        Ok(())
                    }
                })
                .await
            })
        };
        started_rx.await.expect("started");
        let waiter = {
            let path = path.clone();
            tokio::spawn(
                async move { with_file_mutation_queue(&path, || async move { Ok(()) }).await },
            )
        };
        tokio::time::sleep(Duration::from_millis(50)).await;
        waiter.abort();
        waiter.await.expect_err("aborted");
        release_tx.send(()).expect("release");
        blocker.await.expect("join").expect("queue");
        // A later mutation of the same file must not block behind the
        // phantom abandoned link.
        let later = tokio::time::timeout(
            Duration::from_secs(5),
            with_file_mutation_queue(&path, || async move { Ok(()) }),
        )
        .await
        .expect("queue must not be wedged by an abandoned waiter");
        later.expect("queue");
    }
}
