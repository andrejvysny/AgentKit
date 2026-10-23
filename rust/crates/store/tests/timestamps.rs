use agentkit_store::*;
use serde_json::{Value, json};

fn fixture() -> Value {
    serde_json::from_str(include_str!("fixtures/conversation-timestamps.json")).unwrap()
}

fn chat(id: &str, timestamp: ConversationTimestamp) -> ChatRecord {
    ChatRecord {
        id: id.into(),
        title: None,
        metadata: json!({}),
        archived: false,
        created_at: timestamp.clone(),
        updated_at: timestamp,
    }
}

fn message(id: &str, timestamp: ConversationTimestamp) -> MessageRecord {
    MessageRecord {
        id: format!("{id}-message"),
        chat_id: id.into(),
        run_id: None,
        role: "user".into(),
        content: json!("Imported"),
        parent_message_id: None,
        tool_call_id: None,
        tool_calls: None,
        model_result_json: None,
        metadata: json!({}),
        order_key: 0,
        depth: 0,
        branch_index: 0,
        active: true,
        created_at: timestamp,
    }
}

#[test]
fn epoch_text_matches_javascript_date_extremes_and_timeclip_results() {
    let fixture = fixture();
    for case in fixture["epochs"]
        .as_array()
        .unwrap()
        .iter()
        .chain(fixture["timeClip"].as_array().unwrap())
    {
        let milliseconds = case["milliseconds"].as_i64().unwrap();
        let timestamp = ConversationTimestamp::from_epoch_millis(milliseconds).unwrap();
        assert_eq!(timestamp.epoch_millis(), Some(milliseconds));
        assert_eq!(
            timestamp.wire_text().unwrap(),
            case["text"].as_str().unwrap()
        );
    }
    assert!(serde_json::from_str::<ConversationTimestamp>("1234.75").is_err());
    assert!(serde_json::from_str::<ConversationTimestamp>("-1234.75").is_err());
}

#[test]
fn out_of_range_epoch_returns_typed_error_without_panicking() {
    for milliseconds in [
        i64::MIN,
        -8_640_000_000_000_001,
        8_640_000_000_000_001,
        i64::MAX,
    ] {
        assert_eq!(
            ConversationTimestamp::from_epoch_millis(milliseconds),
            Err(TimestampOutOfRange)
        );
        assert_eq!(
            ConversationTimestamp::EpochMillis(milliseconds).wire_text(),
            Err(TimestampOutOfRange)
        );
    }
}

#[tokio::test]
async fn imported_timestamp_text_survives_memory_file_and_reopen_without_canonicalization() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("timestamps.db");
    for store in [
        SqliteStore::memory(4).unwrap(),
        SqliteStore::open(&path, 4).unwrap(),
    ] {
        import_text(&store).await;
        assert_text(&store).await;
    }
    let reopened = SqliteStore::open(&path, 4).unwrap();
    assert_text(&reopened).await;
}

async fn import_text(store: &SqliteStore) {
    for (index, timestamp) in fixture()["imports"].as_array().unwrap().iter().enumerate() {
        let timestamp: ConversationTimestamp = timestamp.as_str().unwrap().into();
        let id = index.to_string();
        store
            .import_conversation(ImportConversation {
                chat: chat(&id, timestamp.clone()),
                messages: vec![message(&id, timestamp)],
            })
            .await
            .unwrap();
    }
}

async fn assert_text(store: &SqliteStore) {
    let fixture = fixture();
    for (index, text) in fixture["imports"].as_array().unwrap().iter().enumerate() {
        let id = index.to_string();
        let chat = store.get_chat(&id).await.unwrap().unwrap();
        assert_eq!(chat.created_at.wire_text().unwrap(), text.as_str().unwrap());
        assert_eq!(chat.updated_at, chat.created_at);
        assert_eq!(chat.created_at.epoch_millis(), None);
        let message = store
            .get_message(&format!("{id}-message"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(message.created_at, chat.created_at);
    }
    let order: Vec<String> = store
        .list_chats(true)
        .await
        .unwrap()
        .into_iter()
        .map(|chat| chat.updated_at.wire_text().unwrap().into_owned())
        .collect();
    assert_eq!(json!(order), fixture["importOrder"]["sqlite"]);
    assert_eq!(
        fixture["importOrder"]["memory"],
        fixture["importOrder"]["sqlite"]
    );
}

#[tokio::test]
async fn existing_numeric_records_remain_readable_and_new_native_clock_writes_stay_typed() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("legacy.db");
    let store = SqliteStore::open(&path, 4).unwrap();
    store
        .create_chat("legacy", None, json!({}), 1000)
        .await
        .unwrap();
    let raw: Value = serde_json::from_str(
        &rusqlite::Connection::open(&path)
            .unwrap()
            .query_row::<String, _, _>("SELECT record FROM chats WHERE id='legacy'", [], |row| {
                row.get(0)
            })
            .unwrap(),
    )
    .unwrap();
    assert_eq!(raw["createdAt"], 1000);
    drop(store);
    let reopened = SqliteStore::open(&path, 4).unwrap();
    let record = reopened.get_chat("legacy").await.unwrap().unwrap();
    assert_eq!(record.created_at, ConversationTimestamp::EpochMillis(1000));
    assert_eq!(
        record.created_at.wire_text().unwrap(),
        "1970-01-01T00:00:01.000Z"
    );
    assert!(matches!(
        reopened
            .create_chat("invalid", None, json!({}), i64::MAX)
            .await,
        Err(StoreError::InvalidInput(_))
    ));
    assert!(reopened.get_chat("invalid").await.unwrap().is_none());
}

#[tokio::test]
async fn imported_unicode_text_uses_documented_sqlite_binary_order() {
    let store = SqliteStore::memory(4).unwrap();
    let fixture = fixture();
    let divergence = &fixture["localeDivergence"];
    assert_ne!(divergence["memory"], divergence["sqlite"]);
    for (index, text) in divergence["input"].as_array().unwrap().iter().enumerate() {
        store
            .import_conversation(ImportConversation {
                chat: chat(&index.to_string(), text.as_str().unwrap().into()),
                messages: vec![],
            })
            .await
            .unwrap();
    }
    let order: Vec<String> = store
        .list_chats(true)
        .await
        .unwrap()
        .into_iter()
        .map(|chat| chat.created_at.wire_text().unwrap().into_owned())
        .collect();
    assert_eq!(json!(order), divergence["sqlite"]);
}
