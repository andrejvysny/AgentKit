use super::*;
use serde_json::json;
use std::time::Duration;

fn block_worker(
    store: &SqliteStore,
) -> (std::sync::mpsc::Receiver<()>, std::sync::mpsc::Sender<()>) {
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    store
        .worker
        .send(Box::new(move |_| {
            started_tx.send(()).unwrap();
            release_rx.recv().unwrap();
        }))
        .unwrap();
    (started_rx, release_tx)
}
#[tokio::test]
async fn abandoned_begin_rolls_back_even_if_actor_has_not_started_it() {
    let store = SqliteStore::memory(8).unwrap();
    let (started, release) = block_worker(&store);
    started.recv_timeout(Duration::from_secs(2)).unwrap();
    let root = store.clone();
    let begin = tokio::spawn(async move { root.begin_transaction().await });
    while store.gate.try_lock().is_ok() {
        tokio::task::yield_now().await;
    }
    begin.abort();
    let _ = begin.await;
    release.send(()).unwrap();
    tokio::time::timeout(
        Duration::from_secs(3),
        store.set_setting("test", "key", json!(1)),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        store.get_setting("test", "key").await.unwrap(),
        Some(json!(1))
    );
}
#[tokio::test]
async fn cancelled_commit_stays_owned_until_the_actor_finishes_it() {
    let store = SqliteStore::memory(8).unwrap();
    let tx = store.begin_transaction().await.unwrap();
    tx.view()
        .set_setting("test", "key", json!(1))
        .await
        .unwrap();
    let scope = tx.view.scope.as_ref().unwrap().clone();
    let (started, release) = block_worker(&store);
    started.recv_timeout(Duration::from_secs(2)).unwrap();
    let commit = tokio::spawn(async move { tx.commit().await });
    while scope.state.lock().unwrap().active {
        tokio::task::yield_now().await;
    }
    commit.abort();
    let _ = commit.await;
    release.send(()).unwrap();
    let value = tokio::time::timeout(Duration::from_secs(3), store.get_setting("test", "key"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(value, Some(json!(1)));
}

#[tokio::test]
async fn cancelled_rollback_keeps_gate_until_sql_finishes() {
    let store = SqliteStore::memory(8).unwrap();
    let tx = store.begin_transaction().await.unwrap();
    tx.view()
        .set_setting("test", "key", json!(1))
        .await
        .unwrap();
    let scope = tx.view.scope.as_ref().unwrap().clone();
    let (started, release) = block_worker(&store);
    started.recv_timeout(Duration::from_secs(2)).unwrap();
    let rollback = tokio::spawn(async move { tx.rollback().await });
    while scope.state.lock().unwrap().active {
        tokio::task::yield_now().await;
    }
    rollback.abort();
    let _ = rollback.await;
    let root = store.clone();
    let waiting = tokio::spawn(async move { root.get_setting("test", "key").await });
    tokio::task::yield_now().await;
    assert!(!waiting.is_finished());
    release.send(()).unwrap();
    assert_eq!(waiting.await.unwrap().unwrap(), None);
}

#[tokio::test]
async fn explicit_rollback_failure_poisons_store() {
    let store = SqliteStore::memory(8).unwrap();
    let tx = store.begin_transaction().await.unwrap();
    tx.view()
        .call(|db| {
            db.execute_batch("ROLLBACK")?;
            Ok(())
        })
        .await
        .unwrap();
    assert!(tx.rollback().await.is_err());
    assert!(matches!(
        store.get_setting("x", "y").await,
        Err(StoreError::RecoveryUnreadable(_))
    ));
}

#[tokio::test]
async fn dropped_owner_waits_for_already_queued_scope_work_before_rollback() {
    let store = SqliteStore::memory(8).unwrap();
    let tx = store.begin_transaction().await.unwrap();
    let scope = tx.view.scope.as_ref().unwrap().clone();
    let (started, release) = block_worker(&store);
    started.recv_timeout(Duration::from_secs(2)).unwrap();
    let view = tx.view();
    let operation = tokio::spawn(async move { view.set_setting("test", "key", json!(1)).await });
    while scope.operation_gate.try_lock().is_ok() {
        tokio::task::yield_now().await;
    }
    drop(tx);
    release.send(()).unwrap();
    operation.await.unwrap().unwrap();
    assert_eq!(store.get_setting("test", "key").await.unwrap(), None);
}
#[tokio::test]
async fn failed_commit_and_rollback_poison_the_connection() {
    let store = SqliteStore::memory(8).unwrap();
    let tx = store.begin_transaction().await.unwrap();
    tx.view()
        .call(|db| {
            db.execute_batch("ROLLBACK")?;
            Ok(())
        })
        .await
        .unwrap();
    assert!(tx.commit().await.is_err());
    assert!(matches!(
        store.get_setting("x", "y").await,
        Err(StoreError::RecoveryUnreadable(_))
    ));
}

#[tokio::test]
async fn failed_drop_rollback_poison_and_closed_store_fail_closed() {
    let store = SqliteStore::memory(8).unwrap();
    let tx = store.begin_transaction().await.unwrap();
    tx.view()
        .call(|db| {
            db.execute_batch("ROLLBACK")?;
            Ok(())
        })
        .await
        .unwrap();
    drop(tx);
    assert!(matches!(
        store.get_setting("x", "y").await,
        Err(StoreError::RecoveryUnreadable(_))
    ));

    let mut closed = SqliteStore::memory(8).unwrap();
    Arc::get_mut(&mut closed.worker).unwrap().sender.take();
    assert!(matches!(
        closed.get_setting("x", "y").await,
        Err(StoreError::Closed)
    ));
}

#[tokio::test]
async fn failed_inner_cleanup_cannot_commit_partial_writes() {
    use rusqlite::hooks::{AuthAction, AuthContext, Authorization, TransactionOperation};
    for mode in ["early", "rollback", "commit"] {
        let store = SqliteStore::memory(8).unwrap();
        let tx = store.begin_transaction().await.unwrap();
        let view = tx.view();
        view.call(|db| {
            db.authorizer(Some(|context: AuthContext<'_>| match context.action {
                AuthAction::Savepoint {
                    operation: TransactionOperation::Rollback | TransactionOperation::Release,
                    ..
                } => Authorization::Deny,
                _ => Authorization::Allow,
            }))?;
            Ok(())
        })
        .await
        .unwrap();
        let outcome = view
            .call(move |db| {
                let inner = atomic(db)?;
                inner.execute(
                    "INSERT INTO settings(namespace,key,value) VALUES('fault','partial','1')",
                    [],
                )?;
                match mode {
                    "rollback" => inner.rollback().map_err(StoreError::from),
                    "commit" => inner.commit().map_err(StoreError::from),
                    _ => Err(StoreError::InvalidInput("injected operation error".into())),
                }
            })
            .await;
        assert!(outcome.is_err(), "{mode}");
        assert!(
            matches!(tx.commit().await, Err(StoreError::RecoveryUnreadable(_))),
            "{mode}"
        );
        assert!(
            matches!(
                store.set_setting("fault", "later", json!(1)).await,
                Err(StoreError::RecoveryUnreadable(_))
            ),
            "{mode}"
        );
        let (sender, receiver) = oneshot::channel();
        store
            .worker
            .send_cleanup(Box::new(move |db| {
                let count: i64 = db
                    .query_row(
                        "SELECT count(*) FROM settings WHERE namespace='fault'",
                        [],
                        |row| row.get(0),
                    )
                    .unwrap();
                let _ = sender.send(count);
            }))
            .unwrap();
        assert_eq!(receiver.await.unwrap(), 0, "{mode}");
    }
}

#[tokio::test]
async fn many_waiters_share_one_deadline_service_and_unregister() {
    let store = SqliteStore::memory(8)
        .unwrap()
        .with_transaction_gate_timeout(Duration::from_secs(2));
    let tx = store.begin_transaction().await.unwrap();
    let mut tasks = Vec::new();
    for _ in 0..128 {
        let root = store.clone();
        tasks.push(tokio::spawn(
            async move { root.get_setting("x", "y").await },
        ));
    }
    tokio::time::timeout(Duration::from_secs(1), async {
        while store.worker.deadlines.pending() != 128 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    for task in tasks.drain(..64) {
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
    }
    assert_eq!(store.worker.deadlines.pending(), 64);
    for task in tasks {
        assert!(matches!(
            task.await.unwrap(),
            Err(StoreError::TransactionGateTimeout)
        ));
    }
    assert_eq!(store.worker.deadlines.pending(), 0);
    drop(tx);
    store.flush().await.unwrap();
}

#[tokio::test]
async fn ordinary_contention_never_uses_transaction_deadline() {
    let store = SqliteStore::memory(8)
        .unwrap()
        .with_transaction_gate_timeout(Duration::from_millis(20));
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let root = store.clone();
    let blocked = tokio::spawn(async move {
        root.call(move |_| {
            started_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            Ok(())
        })
        .await
    });
    while started_rx.try_recv().is_err() {
        tokio::task::yield_now().await;
    }
    let root = store.clone();
    let waiting = tokio::spawn(async move { root.get_setting("x", "y").await });
    tokio::time::sleep(Duration::from_millis(60)).await;
    assert!(!waiting.is_finished());
    assert_eq!(store.worker.deadlines.pending(), 0);
    release_tx.send(()).unwrap();
    blocked.await.unwrap().unwrap();
    assert_eq!(waiting.await.unwrap().unwrap(), None);
}

#[tokio::test]
async fn existing_waiter_detects_subsequent_ambient_ownership() {
    let store = SqliteStore::memory(8)
        .unwrap()
        .with_transaction_gate_timeout(Duration::from_millis(40));
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let root = store.clone();
    let blocked = tokio::spawn(async move {
        root.call(move |_| {
            started_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            Ok(())
        })
        .await
    });
    while started_rx.try_recv().is_err() {
        tokio::task::yield_now().await;
    }
    let root = store.clone();
    let begin = tokio::spawn(async move { root.begin_transaction().await });
    tokio::task::yield_now().await;
    let root = store.clone();
    let waiting = tokio::spawn(async move { root.get_setting("x", "y").await });
    tokio::task::yield_now().await;
    release_tx.send(()).unwrap();
    blocked.await.unwrap().unwrap();
    let tx = begin.await.unwrap().unwrap();
    assert!(matches!(
        waiting.await.unwrap(),
        Err(StoreError::TransactionGateTimeout)
    ));
    drop(tx);
    store.flush().await.unwrap();
}

#[tokio::test]
async fn ordinary_work_between_transactions_does_not_reset_deadline() {
    let store = SqliteStore::memory(8)
        .unwrap()
        .with_transaction_gate_timeout(Duration::from_millis(400));
    let initial = store.begin_transaction().await.unwrap();
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let root = store.clone();
    let ordinary = tokio::spawn(async move {
        root.call(move |_| {
            started_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            Ok(())
        })
        .await
    });
    while store.worker.deadlines.pending() < 1 {
        tokio::task::yield_now().await;
    }
    let root = store.clone();
    let next = tokio::spawn(async move { root.begin_transaction().await });
    while store.worker.deadlines.pending() < 2 {
        tokio::task::yield_now().await;
    }
    let root = store.clone();
    let waiting = tokio::spawn(async move { root.get_setting("x", "y").await });
    while store.worker.deadlines.pending() < 3 {
        tokio::task::yield_now().await;
    }
    tokio::time::sleep(Duration::from_millis(250)).await;
    initial.rollback().await.unwrap();
    while started_rx.try_recv().is_err() {
        tokio::task::yield_now().await;
    }
    while store.worker.deadlines.pending() != 0 {
        tokio::task::yield_now().await;
    }
    release_tx.send(()).unwrap();
    ordinary.await.unwrap().unwrap();
    let next = next.await.unwrap().unwrap();
    let result = tokio::time::timeout(Duration::from_millis(300), waiting)
        .await
        .expect("deadline reset")
        .unwrap();
    assert!(matches!(result, Err(StoreError::TransactionGateTimeout)));
    next.rollback().await.unwrap();
}
