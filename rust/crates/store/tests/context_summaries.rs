use agentkit_store::*;
use serde_json::{Value, json};
fn summary(id: &str, session: &str, date: &str) -> Value {
    json!({"summaryId":id,"sessionId":session,"coversThroughMessageId":"a","firstKeptMessageId":"b","summary":"content","sourceHash":"hash","tokensBefore":42,"tokensAfter":7,"model":"model","summarizerRevision":"revision","details":{"readFiles":["a"],"modifiedFiles":["b"]},"createdAt":date,"extra":{"retained":true}})
}
async fn setup(store: &SqliteStore) {
    for row in [
        summary("one", "s", "2026-01-01"),
        summary("two", "s", "2026-01-01"),
        summary("three", "s", "2026-01-02"),
        summary("foreign", "other", "2026-01-03"),
    ] {
        store.save_context_summary(row).await.unwrap();
    }
    let mut duplicate = summary("one", "other", "2030-01-01");
    duplicate["summary"] = json!("changed");
    store.save_context_summary(duplicate).await.unwrap();
    assert_eq!(
        store.list_context_summaries("s").await.unwrap(),
        vec![
            summary("three", "s", "2026-01-02"),
            summary("two", "s", "2026-01-01"),
            summary("one", "s", "2026-01-01")
        ]
    );
}
#[tokio::test]
async fn summaries_are_idempotent_ordered_and_deleted_by_session() {
    let store = SqliteStore::memory(8).unwrap();
    setup(&store).await;
    assert_eq!(
        store.delete_session_context_summaries("s").await.unwrap(),
        3
    );
    assert_eq!(
        store.delete_session_context_summaries("s").await.unwrap(),
        0
    );
    assert_eq!(
        store.list_context_summaries("other").await.unwrap(),
        vec![summary("foreign", "other", "2026-01-03")]
    );
}
#[tokio::test]
async fn summaries_survive_reopen_and_malformed_data_is_not_discarded() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.db");
    let store = SqliteStore::open(&path, 8).unwrap();
    setup(&store).await;
    drop(store);
    let child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--ignored", "--exact", "summaries_reopen_child"])
        .env("AGENTKIT_SUMMARIES_DB", &path)
        .output()
        .unwrap();
    assert!(
        child.status.success(),
        "{}",
        String::from_utf8_lossy(&child.stderr)
    );
    let reopened = SqliteStore::open(&path, 8).unwrap();
    assert_eq!(reopened.list_context_summaries("s").await.unwrap().len(), 3);
    reopened
        .set_setting(CONTEXT_SUMMARIES_NAMESPACE, "bad", json!({"sessionId":"s"}))
        .await
        .unwrap();
    assert!(reopened.list_context_summaries("s").await.is_err());
    assert!(
        reopened
            .delete_session_context_summaries("s")
            .await
            .is_err()
    );
    assert!(
        reopened
            .get_setting(CONTEXT_SUMMARIES_NAMESPACE, "one")
            .await
            .unwrap()
            .is_some()
    );
}

#[test]
#[ignore]
fn summaries_reopen_child() {
    let path = std::env::var_os("AGENTKIT_SUMMARIES_DB").unwrap();
    tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap()
        .block_on(async {
            let store = SqliteStore::open(path, 8).unwrap();
            assert_eq!(
                store.list_context_summaries("s").await.unwrap(),
                vec![
                    summary("three", "s", "2026-01-02"),
                    summary("two", "s", "2026-01-01"),
                    summary("one", "s", "2026-01-01")
                ]
            );
        });
}
