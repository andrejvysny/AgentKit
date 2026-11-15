use agentkit_store::*;
use serde_json::{Value, json};
fn mask(id: &str, session: &str, date: &str) -> Value {
    json!({"maskId":id,"sessionId":session,"coveredCallIds":["c1"],"masks":[{"messageId":"m1","callId":"c1","handle":"out_0123456789abcdef0123456789abcdef","bytes":9000}],"images":[{"messageId":"m0","names":["shot.png"]}],"sourceHash":"hash","tokensBefore":42,"tokensAfter":7,"createdAt":date})
}
#[tokio::test]
async fn masks_are_idempotent_ordered_and_deleted_by_session() {
    let store = SqliteStore::memory(8).unwrap();
    for row in [
        mask("one", "s", "2026-01-01"),
        mask("two", "s", "2026-01-02"),
        mask("foreign", "other", "2026-01-03"),
    ] {
        store.save_context_mask(row).await.unwrap();
    }
    let mut duplicate = mask("one", "other", "2030-01-01");
    duplicate["sourceHash"] = json!("changed");
    // A second record under a taken id is refused, never silently dropped.
    assert!(store.save_context_mask(duplicate).await.is_err());
    assert_eq!(
        store.list_context_masks("s").await.unwrap(),
        vec![
            mask("two", "s", "2026-01-02"),
            mask("one", "s", "2026-01-01")
        ]
    );
    assert_eq!(store.delete_session_context_masks("s").await.unwrap(), 2);
    assert_eq!(store.delete_session_context_masks("s").await.unwrap(), 0);
    assert_eq!(
        store.list_context_masks("other").await.unwrap(),
        vec![mask("foreign", "other", "2026-01-03")]
    );
}
#[tokio::test]
async fn malformed_masks_are_refused() {
    let store = SqliteStore::memory(8).unwrap();
    let mut bad = mask("one", "s", "2026-01-01");
    bad["masks"][0]["bytes"] = json!("many");
    assert!(store.save_context_mask(bad).await.is_err());
}
#[tokio::test]
async fn deleting_a_chat_removes_its_masks_and_summaries() {
    let store = SqliteStore::memory(8).unwrap();
    store.create_chat("s", None, json!({}), 1000).await.unwrap();
    store
        .create_chat("other", None, json!({}), 1000)
        .await
        .unwrap();
    store
        .save_context_mask(mask("one", "s", "2026-01-01"))
        .await
        .unwrap();
    store
        .save_context_mask(mask("kept", "other", "2026-01-02"))
        .await
        .unwrap();
    store
        .save_context_summary(json!({"summaryId":"sum","sessionId":"s","coversThroughMessageId":"a","firstKeptMessageId":"b","summary":"x","sourceHash":"h","model":"m","summarizerRevision":"1","createdAt":"2026-01-01","tokensBefore":2,"tokensAfter":1,"details":{"readFiles":[],"modifiedFiles":[]}}))
        .await
        .unwrap();
    store.delete_chat("s").await.unwrap();
    assert!(store.list_context_masks("s").await.unwrap().is_empty());
    assert!(store.list_context_summaries("s").await.unwrap().is_empty());
    assert_eq!(store.list_context_masks("other").await.unwrap().len(), 1);
}
#[tokio::test]
async fn a_malformed_row_is_skipped_not_fatal() {
    let store = SqliteStore::memory(8).unwrap();
    store
        .set_setting(CONTEXT_MASKS_NAMESPACE, "broken", json!({"sessionId":"s"}))
        .await
        .unwrap();
    store
        .save_context_mask(mask("one", "s", "2026-01-01"))
        .await
        .unwrap();
    assert_eq!(store.list_context_masks("s").await.unwrap().len(), 1);
}
