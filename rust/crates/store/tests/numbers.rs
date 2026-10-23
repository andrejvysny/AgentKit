mod support;
use agentkit_contracts::{javascript_number, parse_javascript_json};
use agentkit_store::*;
use serde_json::{Value, json};
use support::*;

fn numbers() -> Value {
    parse_javascript_json(r#"{"positive":1e400,"negative":-1e400,"nested":[1e400,{"value":-1e400}],"rounded":9007199254740993,"zero":-0}"#).unwrap()
}

fn assert_numbers(value: &Value, memory: bool) {
    assert_eq!(
        javascript_number(&value["rounded"]),
        Some(9_007_199_254_740_992.0)
    );
    for item in [
        &value["positive"],
        &value["negative"],
        &value["nested"][0],
        &value["nested"][1]["value"],
    ] {
        if memory {
            assert!(javascript_number(item).unwrap().is_infinite());
        } else {
            assert!(item.is_null());
        }
    }
    let zero = javascript_number(&value["zero"]).unwrap();
    assert_eq!(zero, 0.0);
    assert_eq!(zero.is_sign_negative(), memory);
}

async fn roundtrip(store: &SqliteStore, memory: bool) {
    store
        .create_chat("chat", None, numbers(), 1000)
        .await
        .unwrap();
    let mut input = task("run", "chat");
    input.payload = numbers();
    let mut msg = message("message", "chat", Some("run"));
    msg.content = numbers();
    msg.metadata = numbers();
    store.submit_turn(input, vec![msg]).await.unwrap();
    store
        .set_setting("numbers", "value", numbers())
        .await
        .unwrap();
    store
        .enqueue_outbox(
            OutboxInput {
                id: Some("numbers".into()),
                topic: "test".into(),
                run_id: None,
                payload: numbers(),
                available_at: 1000,
            },
            1000,
        )
        .await
        .unwrap();
    let lease = store.claim(claim(1000)).await.unwrap().unwrap().lease;
    let mut input_event = event(0, "numbers-event");
    input_event["data"] = numbers();
    store
        .append_events(lease.fence(), vec![input_event], 1001)
        .await
        .unwrap();
    store
        .update_progress(lease.fence(), numbers(), 1001)
        .await
        .unwrap();
    assert_numbers(
        &store.get_chat("chat").await.unwrap().unwrap().metadata,
        memory,
    );
    let record = store.get_message("message").await.unwrap().unwrap();
    assert_numbers(&record.content, memory);
    assert_numbers(&record.metadata, memory);
    let record = store.get_task("run").await.unwrap().unwrap();
    assert_numbers(&record.payload, memory);
    assert_numbers(record.progress.as_ref().unwrap(), memory);
    assert_numbers(
        &store
            .get_setting("numbers", "value")
            .await
            .unwrap()
            .unwrap(),
        memory,
    );
    let record = store
        .outbox()
        .await
        .unwrap()
        .into_iter()
        .find(|item| item.id == "numbers")
        .unwrap();
    assert_numbers(&record.payload, memory);
    assert_numbers(
        &store.events("run", None, 10).await.unwrap()[0].event["data"],
        memory,
    );
}

#[tokio::test]
async fn memory_preserves_structured_clone_infinity_and_signed_zero() {
    roundtrip(&SqliteStore::memory(8).unwrap(), true).await;
}

#[tokio::test]
async fn file_json_stringify_coerces_nonfinite_numbers_and_reopens() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("numbers.db");
    let store = SqliteStore::open(&path, 8).unwrap();
    roundtrip(&store, false).await;
    drop(store);
    let connection = rusqlite::Connection::open(&path).unwrap();
    let persisted: String = connection
        .query_row("SELECT record FROM tasks WHERE id='run'", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert!(!persisted.contains("1e400"));
    assert!(!persisted.contains("Infinity"));
    assert!(!persisted.contains("NaN"));
    drop(connection);
    let reopened = SqliteStore::open(&path, 8).unwrap();
    assert_numbers(
        &reopened
            .get_setting("numbers", "value")
            .await
            .unwrap()
            .unwrap(),
        false,
    );
}

#[tokio::test]
async fn private_continuation_rejects_nonfinite_state_without_overwriting_valid_evidence() {
    let store = SqliteStore::memory(4).unwrap();
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
    let scope = json!({"providerId":"p","protocol":"responses","connectionId":"c","generation":0,"chatId":"chat","branchId":"b","model":"m"});
    store
        .bind_run("run", "chat", scope.clone(), lease.fence(), 1001)
        .await
        .unwrap();
    let state = ProviderContinuationRecord {
        run_id: "run".into(),
        anchor_message_id: "anchor".into(),
        state: json!({"version":1,"scope":scope,"messageCount":1,"messageDigest":"a".repeat(64),"inputItems":[{}]}),
    };
    store
        .put_continuation(state.clone(), lease.fence(), 1002)
        .await
        .unwrap();
    let mut invalid = state.clone();
    invalid.state["inputItems"][0] = numbers();
    assert!(
        matches!(store.put_continuation(invalid,lease.fence(),1003).await,Err(StoreError::InvalidInput(code)) if code=="invalid_provider_continuation")
    );
    assert_eq!(store.get_continuation("run").await.unwrap(), Some(state));
}

#[tokio::test]
async fn normalized_native_integer_overflow_is_rejected_before_any_record_is_written() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("overflow.db");
    for store in [
        SqliteStore::memory(4).unwrap(),
        SqliteStore::open(path, 4).unwrap(),
    ] {
        let mut input = task("overflow", "scope");
        input.available_at = Some(i64::MAX);
        assert!(matches!(
            store.submit(input).await,
            Err(StoreError::Json(_))
        ));
        assert!(store.get_task("overflow").await.unwrap().is_none());
        assert!(store.outbox().await.unwrap().is_empty());
    }
}
