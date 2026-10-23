mod support;
use agentkit_contracts::parse_javascript_json;
use agentkit_store::*;
use serde_json::{Value, json};
use support::*;

fn markers() -> Value {
    parse_javascript_json(r#"[{"$serde_json::private::Number":"1"},{"$serde_json::private::Number":"1e400"},{"$serde_json::private::Number":"ordinary"},{"$serde_json::private::Number":1},{"$serde_json::private::Number":{"nested":true}},{"$serde_json::private::Number":"2","other":true},{"other":true,"$serde_json::private::Number":"3"}]"#).unwrap()
}

async fn roundtrip(store: &SqliteStore) {
    let values = markers();
    store
        .create_chat("chat", None, values.clone(), 1000)
        .await
        .unwrap();
    let mut task = task("run", "chat");
    task.payload = values.clone();
    let mut message = message("anchor", "chat", Some("run"));
    message.content = values.clone();
    message.metadata = values.clone();
    message.tool_calls = Some(values.clone());
    let input = serde_json::to_string(&message).unwrap();
    assert_eq!(
        serde_json::from_str::<MessageInput>(&input)
            .unwrap()
            .content,
        values
    );
    assert_eq!(
        serde_json::from_value::<MessageInput>(parse_javascript_json(&input).unwrap())
            .unwrap()
            .metadata,
        values
    );
    store.submit_turn(task, vec![message]).await.unwrap();
    let lease = store.claim(claim(1000)).await.unwrap().unwrap().lease;
    store
        .update_progress(lease.fence(), values.clone(), 1001)
        .await
        .unwrap();
    store
        .checkpoint_runtime_state(lease.fence(), None, values.clone(), 1001)
        .await
        .unwrap();
    let mut input = event(0, "markers");
    input["data"] = values.clone();
    store
        .append_events(lease.fence(), vec![input], 1001)
        .await
        .unwrap();
    store
        .enqueue_outbox(
            OutboxInput {
                id: Some("markers".into()),
                topic: "test".into(),
                run_id: None,
                payload: values.clone(),
                available_at: 1000,
            },
            1000,
        )
        .await
        .unwrap();
    store
        .set_setting("markers", "array", values.clone())
        .await
        .unwrap();
    for (index, value) in values.as_array().unwrap().iter().enumerate() {
        let key = index.to_string();
        store
            .set_setting("markers", &key, value.clone())
            .await
            .unwrap();
        assert_eq!(
            store.get_setting("markers", &key).await.unwrap(),
            Some(value.clone())
        );
    }
    let task = store.get_task("run").await.unwrap().unwrap();
    assert_eq!(task.payload, values);
    assert_eq!(task.progress, Some(values.clone()));
    assert_eq!(task.runtime_state, Some(values.clone()));
    assert_eq!(
        store.get_chat("chat").await.unwrap().unwrap().metadata,
        values
    );
    let message = store.get_message("anchor").await.unwrap().unwrap();
    assert_eq!(message.content, values);
    assert_eq!(message.metadata, values);
    assert_eq!(message.tool_calls, Some(values.clone()));
    assert_eq!(
        store.events("run", None, 10).await.unwrap()[0].event["data"],
        values
    );
    assert_eq!(
        store
            .outbox()
            .await
            .unwrap()
            .into_iter()
            .find(|event| event.id == "markers")
            .unwrap()
            .payload,
        values
    );
    assert_eq!(
        store.get_setting("markers", "array").await.unwrap(),
        Some(values.clone())
    );
    assert_eq!(store.list_settings("markers").await.unwrap().len(), 8);
    continuation(store, &lease, values).await;
}

async fn continuation(store: &SqliteStore, lease: &Lease, items: Value) {
    let scope = json!({"providerId":"p","protocol":"responses","connectionId":"c","generation":0,"chatId":"chat","branchId":"b","model":"m"});
    store
        .bind_run("run", "chat", scope.clone(), lease.fence(), 1001)
        .await
        .unwrap();
    let record = ProviderContinuationRecord {
        run_id: "run".into(),
        anchor_message_id: "anchor".into(),
        state: json!({"version":1,"scope":scope,"messageCount":1,"messageDigest":"a".repeat(64),"inputItems":items}),
    };
    store
        .put_continuation(record.clone(), lease.fence(), 1002)
        .await
        .unwrap();
    assert_eq!(store.get_continuation("run").await.unwrap(), Some(record));
}

#[tokio::test]
async fn private_marker_objects_survive_typed_memory_and_file_records_and_reopen() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("markers.db");
    roundtrip(&SqliteStore::memory(8).unwrap()).await;
    let store = SqliteStore::open(&path, 8).unwrap();
    roundtrip(&store).await;
    drop(store);
    let reopened = SqliteStore::open(&path, 8).unwrap();
    assert_eq!(
        reopened.get_task("run").await.unwrap().unwrap().payload,
        markers()
    );
    assert_eq!(
        reopened.get_setting("markers", "0").await.unwrap(),
        Some(markers()[0].clone())
    );
    assert_eq!(
        reopened
            .get_continuation("run")
            .await
            .unwrap()
            .unwrap()
            .state["inputItems"],
        markers()
    );
}
