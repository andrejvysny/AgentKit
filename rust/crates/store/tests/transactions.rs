mod support;
use agentkit_store::*;
use serde_json::json;
use std::time::Duration;
use support::{claim, task};

#[tokio::test]
async fn scoped_operations_commit_across_ports_and_nested_views() {
    let store = SqliteStore::memory(8)
        .unwrap()
        .with_transaction_gate_timeout(Duration::from_millis(100));
    let tx = store.begin_transaction().await.unwrap();
    let view = tx.view();
    view.set_setting("test", "key", json!(1)).await.unwrap();
    let nested = view.begin_transaction().await.unwrap();
    nested.view().upsert_provider(json!({"id":"p","label":"p","kind":"custom","baseUrl":"url","defaultModel":"m","enabled":true})).await.unwrap();
    nested.commit().await.unwrap();
    assert!(matches!(
        store.get_setting("test", "key").await,
        Err(StoreError::TransactionGateTimeout)
    ));
    tx.commit().await.unwrap();
    assert_eq!(
        store.get_setting("test", "key").await.unwrap(),
        Some(json!(1))
    );
    assert!(store.get_provider("p").await.unwrap().is_some());
    assert!(matches!(
        view.get_setting("test", "key").await,
        Err(StoreError::TransactionScopeExpired)
    ));
}

#[tokio::test]
async fn dropped_owner_rolls_back_and_root_waits_for_release() {
    let store = SqliteStore::memory(8).unwrap();
    let tx = store.begin_transaction().await.unwrap();
    let view = tx.view();
    view.set_setting("test", "key", json!(1)).await.unwrap();
    view.create_chat("chat", None, json!({}), 0).await.unwrap();
    let root = store.clone();
    let waiting = tokio::spawn(async move { root.get_setting("test", "key").await });
    tokio::task::yield_now().await;
    assert!(!waiting.is_finished());
    drop(tx);
    assert_eq!(waiting.await.unwrap().unwrap(), None);
    assert!(store.get_chat("chat").await.unwrap().is_none());
    assert!(matches!(
        view.set_setting("test", "late", json!(2)).await,
        Err(StoreError::TransactionScopeExpired)
    ));
}

#[tokio::test]
async fn savepoint_failure_rolls_back_only_its_operation() {
    let store = SqliteStore::memory(8).unwrap();
    let tx = store.begin_transaction().await.unwrap();
    let view = tx.view();
    view.set_setting("test", "before", json!(1)).await.unwrap();
    let result = view
        .compare_and_set_settings(
            "test",
            "missing",
            Some(json!("wrong")),
            vec![SettingWrite {
                namespace: "test".into(),
                key: "bad".into(),
                value: Some(json!(2)),
            }],
        )
        .await
        .unwrap();
    assert!(matches!(result, SettingsCommit::Conflict));
    view.set_setting("test", "after", json!(3)).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(
        store.get_setting("test", "before").await.unwrap(),
        Some(json!(1))
    );
    assert_eq!(store.get_setting("test", "bad").await.unwrap(), None);
    assert_eq!(
        store.get_setting("test", "after").await.unwrap(),
        Some(json!(3))
    );
}

#[tokio::test]
async fn unrelated_root_write_waits_until_commit() {
    let store = SqliteStore::memory(8).unwrap();
    let tx = store.begin_transaction().await.unwrap();
    let root = store.clone();
    let write = tokio::spawn(async move { root.set_setting("test", "root", json!(1)).await });
    tokio::task::yield_now().await;
    assert!(!write.is_finished());
    tx.commit().await.unwrap();
    write.await.unwrap().unwrap();
    assert_eq!(
        store.get_setting("test", "root").await.unwrap(),
        Some(json!(1))
    );
}

#[tokio::test]
async fn explicit_and_nested_rollback_preserve_transaction_boundaries() {
    let store = SqliteStore::memory(8).unwrap();
    let tx = store.begin_transaction().await.unwrap();
    let view = tx.view();
    view.set_setting("test", "outer", json!(1)).await.unwrap();
    let nested = view.begin_transaction().await.unwrap();
    nested
        .view()
        .set_setting("test", "nested", json!(2))
        .await
        .unwrap();
    nested.rollback().await.unwrap();
    assert_eq!(
        view.get_setting("test", "nested").await.unwrap(),
        Some(json!(2))
    );
    tx.rollback().await.unwrap();
    assert_eq!(store.get_setting("test", "outer").await.unwrap(), None);
    assert_eq!(store.get_setting("test", "nested").await.unwrap(), None);
    assert!(matches!(
        view.get_setting("test", "nested").await,
        Err(StoreError::TransactionScopeExpired)
    ));
}

#[test]
fn root_wait_timeout_works_without_tokio_time_driver() {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap()
        .block_on(async {
            let store = SqliteStore::memory(8)
                .unwrap()
                .with_transaction_gate_timeout(Duration::from_millis(20));
            let tx = store.begin_transaction().await.unwrap();
            assert!(matches!(
                store.get_setting("test", "key").await,
                Err(StoreError::TransactionGateTimeout)
            ));
            drop(tx);
            store.flush().await.unwrap();
        });
}

#[tokio::test]
async fn scoped_fence_verification_refuses_stale_lease_and_allows_rollback() {
    let store = SqliteStore::memory(8).unwrap();
    store.submit(task("run", "scope")).await.unwrap();
    let lease = store.claim(claim(1000)).await.unwrap().unwrap().lease;
    store.verify_fence(&lease.fence(), 1001).await.unwrap();
    assert!(
        store
            .get_setting("derived", "summary")
            .await
            .unwrap()
            .is_none()
    );
    let tx = store.begin_transaction().await.unwrap();
    let view = tx.view();
    view.verify_fence(&lease.fence(), 1001).await.unwrap();
    view.set_setting("derived", "summary", json!("uncommitted"))
        .await
        .unwrap();
    assert!(matches!(
        view.verify_fence(&lease.fence(), 1100).await,
        Err(StoreError::LeaseLost(_))
    ));
    drop(tx);
    assert!(
        store
            .get_setting("derived", "summary")
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn flush_releases_transaction_owners_before_immediate_file_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("transaction-reopen.db");
    for iteration in 0..40 {
        let store = SqliteStore::open(&path, 8).unwrap();
        let tx = store.begin_transaction().await.unwrap();
        let view = tx.view();
        view.set_setting("test", "value", json!(iteration))
            .await
            .unwrap();
        drop(view);
        if iteration % 2 == 0 {
            tx.commit().await.unwrap();
        } else {
            drop(tx);
        }
        store.flush().await.unwrap();
        drop(store);
        let reopened = SqliteStore::open(&path, 8).unwrap();
        drop(reopened);
    }
}

#[tokio::test]
async fn compact_request_rechecks_descriptor_and_blocks_queued_and_running_turns() {
    let store = SqliteStore::memory(8).unwrap();
    let descriptor = json!({"version":1,"profile":{"id":"p"},"createdAt":"now"});
    store
        .create_chat("chat", None, json!({"agentkit.session":descriptor}), 1000)
        .await
        .unwrap();
    let stale = json!({"version":1,"profile":{"id":"old"},"createdAt":"now"});
    assert!(matches!(
        store
            .request_compact("chat".into(), "test".into(), stale, 1001)
            .await,
        Err(StoreError::SessionDescriptorChanged(_))
    ));
    assert!(
        store
            .get_chat("chat")
            .await
            .unwrap()
            .unwrap()
            .metadata
            .get(COMPACT_REQUESTED_METADATA_KEY)
            .is_none()
    );
    store.submit(task("run", "chat")).await.unwrap();
    assert!(matches!(
        store
            .request_compact("chat".into(), "test".into(), descriptor.clone(), 1001)
            .await,
        Err(StoreError::ScopeBusy(_))
    ));
    store.claim(claim(1001)).await.unwrap().unwrap();
    assert!(matches!(
        store
            .request_compact("chat".into(), "test".into(), descriptor, 1002)
            .await,
        Err(StoreError::ScopeBusy(_))
    ));
}

#[tokio::test]
async fn queued_root_writer_keeps_fifo_position_before_later_transaction() {
    use std::{
        future::{Future, poll_fn},
        task::Poll,
    };
    let store = SqliteStore::memory(8).unwrap();
    let held = store.begin_transaction().await.unwrap();
    let root = store.clone();
    let (started, ready) = tokio::sync::oneshot::channel();
    let writer = tokio::spawn(async move {
        let write = root.set_setting("test", "order", json!("writer"));
        tokio::pin!(write);
        let mut started = Some(started);
        poll_fn(|cx| match write.as_mut().poll(cx) {
            Poll::Pending => {
                if let Some(started) = started.take() {
                    let _ = started.send(());
                }
                Poll::Pending
            }
            ready => ready,
        })
        .await
    });
    ready.await.unwrap();
    let root = store.clone();
    let later = tokio::spawn(async move { root.begin_transaction().await.unwrap() });
    tokio::task::yield_now().await;
    held.commit().await.unwrap();
    let later = later.await.unwrap();
    assert_eq!(
        later.view().get_setting("test", "order").await.unwrap(),
        Some(json!("writer"))
    );
    later.rollback().await.unwrap();
    writer.await.unwrap().unwrap();
}

fn spawn_when_pending<F>(
    future: F,
) -> (
    tokio::task::JoinHandle<F::Output>,
    tokio::sync::oneshot::Receiver<()>,
)
where
    F: std::future::Future + Send + 'static,
    F::Output: Send + 'static,
{
    use std::task::Poll;
    let (sender, ready) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        tokio::pin!(future);
        let mut sender = Some(sender);
        std::future::poll_fn(|cx| match future.as_mut().poll(cx) {
            Poll::Pending => {
                if let Some(sender) = sender.take() {
                    let _ = sender.send(());
                }
                Poll::Pending
            }
            ready => ready,
        })
        .await
    });
    (task, ready)
}

#[tokio::test]
async fn successive_transactions_do_not_extend_root_wait_deadline() {
    let store = SqliteStore::memory(8)
        .unwrap()
        .with_transaction_gate_timeout(Duration::from_millis(400));
    let initial = store.begin_transaction().await.unwrap();
    let first_root = store.clone();
    let (first, ready) = spawn_when_pending(async move { first_root.begin_transaction().await });
    ready.await.unwrap();
    let second_root = store.clone();
    let (second, ready) = spawn_when_pending(async move { second_root.begin_transaction().await });
    ready.await.unwrap();
    let waiting_root = store.clone();
    let (waiting, ready) =
        spawn_when_pending(async move { waiting_root.get_setting("x", "y").await });
    ready.await.unwrap();
    initial.rollback().await.unwrap();
    let first = first.await.unwrap().unwrap();
    tokio::time::sleep(Duration::from_millis(250)).await;
    first.rollback().await.unwrap();
    let second = second.await.unwrap().unwrap();
    let outcome = tokio::time::timeout(Duration::from_millis(300), waiting)
        .await
        .expect("deadline must not restart")
        .unwrap();
    assert!(matches!(outcome, Err(StoreError::TransactionGateTimeout)));
    second.rollback().await.unwrap();
}
