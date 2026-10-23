mod support;
use agentkit_store::*;
use serde_json::json;
use support::*;

#[tokio::test]
async fn outbox_claim_is_exclusive_fenced_and_attempt_bounded() {
    let store = SqliteStore::memory(4).unwrap();
    store
        .enqueue_outbox(
            OutboxInput {
                id: Some("event".into()),
                topic: "test".into(),
                run_id: None,
                payload: json!({"lossless":[1,2]}),
                available_at: 1000,
            },
            1000,
        )
        .await
        .unwrap();
    let first = store
        .claim_outbox("one", 1000, 10, 10, 2)
        .await
        .unwrap()
        .remove(0);
    assert!(
        store
            .claim_outbox("two", 1001, 10, 10, 2)
            .await
            .unwrap()
            .is_empty()
    );
    let second = store
        .claim_outbox("two", 1010, 10, 10, 2)
        .await
        .unwrap()
        .remove(0);
    assert!(
        store
            .publish_outbox("event", first.claim_token.as_ref().unwrap(), 1011)
            .await
            .is_err()
    );
    store
        .fail_outbox(
            "event",
            second.claim_token.as_ref().unwrap(),
            1011,
            "consumer refused".into(),
            1012,
        )
        .await
        .unwrap();
    assert!(
        store
            .claim_outbox("three", 1020, 10, 10, 2)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        store.outbox().await.unwrap()[0].last_error.as_deref(),
        Some("consumer refused")
    );
    assert_eq!(store.prune_outbox(1100, 2).await.unwrap(), 1);
}

#[tokio::test]
async fn continuation_requires_exact_scope_current_lease_and_run_owned_anchor() {
    let store = SqliteStore::memory(4).unwrap();
    store
        .create_chat("chat", None, json!({}), 1000)
        .await
        .unwrap();
    store
        .submit_turn(
            task("run", "chat"),
            vec![
                message("anchor", "chat", Some("run")),
                message("foreign", "chat", Some("other")),
            ],
        )
        .await
        .unwrap_err();
    store
        .submit_turn(
            task("run", "chat"),
            vec![message("anchor", "chat", Some("run"))],
        )
        .await
        .unwrap();
    let lease = store.claim(claim(1000)).await.unwrap().unwrap().lease;
    let scope = json!({"chatId":"chat","providerId":"test","protocol":"responses","connectionId":"connection","generation":0,"branchId":"branch","model":"m"});
    store
        .bind_run("run", "chat", scope.clone(), lease.fence(), 1001)
        .await
        .unwrap();
    assert!(
        store
            .bind_run(
                "run",
                "chat",
                json!({"different":true}),
                lease.fence(),
                1001
            )
            .await
            .is_err()
    );
    let state = ProviderContinuationRecord {
        run_id: "run".into(),
        anchor_message_id: "anchor".into(),
        state: json!({"version":1,"scope":scope,"messageCount":1,"messageDigest":"a".repeat(64),"inputItems":[{"opaque":"kept"}]}),
    };
    store
        .put_continuation(state.clone(), lease.fence(), 1002)
        .await
        .unwrap();
    assert_eq!(
        store.get_continuation("run").await.unwrap(),
        Some(state.clone())
    );
    assert_eq!(
        store
            .continuations_by_anchors("chat", vec!["anchor".into()])
            .await
            .unwrap(),
        vec![state.clone()]
    );
    assert!(
        store
            .put_continuation(state, lease.fence(), 1100)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn settings_are_namespace_isolated_and_shared_by_memory_clones() {
    let store = SqliteStore::memory(4).unwrap();
    let clone = store.clone();
    store
        .set_setting("providers", "main", json!({"enabled":true}))
        .await
        .unwrap();
    clone
        .set_setting("mcp", "main", json!({"command":"server"}))
        .await
        .unwrap();
    assert_eq!(
        clone.list_settings("providers").await.unwrap(),
        vec![("main".into(), json!({"enabled":true}))]
    );
    assert!(store.delete_setting("providers", "main").await.unwrap());
    assert!(
        clone
            .get_setting("providers", "main")
            .await
            .unwrap()
            .is_none()
    );
    assert!(store.get_setting("mcp", "main").await.unwrap().is_some());
}

#[tokio::test]
async fn actor_binding_accepts_first_owner_or_exact_replay_only() {
    let store = SqliteStore::memory(2).unwrap();
    let other = store.clone();
    let (first, second) = tokio::join!(
        store.bind_setting("internal", "owner", json!("first")),
        other.bind_setting("internal", "owner", json!("second"))
    );
    assert_ne!(first.unwrap(), second.unwrap());
    let owner = store
        .get_setting("internal", "owner")
        .await
        .unwrap()
        .unwrap();
    assert!(
        store
            .bind_setting("internal", "owner", owner.clone())
            .await
            .unwrap()
    );
    assert_eq!(
        store.get_setting("internal", "owner").await.unwrap(),
        Some(owner)
    );
}

#[tokio::test]
async fn continuation_rejects_extra_fields_and_detects_missing_previously_saved_state() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("store.db");
    let store = SqliteStore::open(&path, 2).unwrap();
    store
        .create_chat("chat", None, json!({}), 1000)
        .await
        .unwrap();
    store
        .submit_turn(
            task("run", "chat"),
            vec![message("anchor", "chat", Some("run"))],
        )
        .await
        .unwrap();
    let lease = store.claim(claim(1000)).await.unwrap().unwrap().lease;
    let scope = json!({"providerId":"p","protocol":"responses","connectionId":"c","generation":0.0,"chatId":"chat","branchId":"b","model":"m"});
    store
        .bind_run("run", "chat", scope.clone(), lease.fence(), 1001)
        .await
        .unwrap();
    let mut state = ProviderContinuationRecord {
        run_id: "run".into(),
        anchor_message_id: "anchor".into(),
        state: json!({"version":1.0,"scope":scope,"messageCount":0.0,"messageDigest":"b".repeat(64),"inputItems":[]}),
    };
    store
        .put_continuation(state.clone(), lease.fence(), 1002)
        .await
        .unwrap();
    state.state["unexpected"] = json!(true);
    assert!(
        store
            .put_continuation(state, lease.fence(), 1002)
            .await
            .is_err()
    );
    drop(store);
    {
        let connection = rusqlite::Connection::open(&path).unwrap();
        connection.execute("DELETE FROM continuations", []).unwrap();
    }
    let reopened = SqliteStore::open(&path, 2).unwrap();
    assert!(
        matches!(reopened.get_continuation("run").await,Err(StoreError::InvalidInput(message)) if message=="provider_continuation_state_missing")
    );
}
