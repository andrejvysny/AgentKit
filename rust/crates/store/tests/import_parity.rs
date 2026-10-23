use agentkit_store::*;
use serde_json::json;

fn chat(id: &str) -> ChatRecord {
    ChatRecord {
        id: id.into(),
        title: Some("Imported".into()),
        metadata: json!({"source":"legacy"}),
        archived: false,
        created_at: 100.into(),
        updated_at: 9999.into(),
    }
}

fn message(id: &str, parent: Option<&str>, active: bool) -> MessageRecord {
    MessageRecord {
        id: id.into(),
        chat_id: "old-chat".into(),
        run_id: Some("old-run".into()),
        role: "user".into(),
        content: json!(id),
        parent_message_id: parent.map(str::to_owned),
        tool_call_id: None,
        tool_calls: None,
        model_result_json: None,
        metadata: json!({}),
        order_key: 999,
        depth: 999,
        branch_index: 999,
        active,
        created_at: 101.into(),
    }
}

#[tokio::test]
async fn each_invalid_shape_reports_exact_source_reason_and_writes_nothing() {
    let store = SqliteStore::memory(8).unwrap();
    store
        .create_chat("kept", Some("original".into()), json!({}), 1)
        .await
        .unwrap();
    let cases = vec![
        (
            "broken_active_chain",
            vec![
                message("root", None, true),
                message("a", Some("root"), true),
                message("b", Some("root"), true),
            ],
        ),
        (
            "broken_active_chain",
            vec![
                message("root", None, false),
                message("a", Some("root"), true),
            ],
        ),
        (
            "active_leaf_has_child",
            vec![
                message("root", None, true),
                message("a", Some("root"), false),
            ],
        ),
        (
            "forward_parent",
            vec![
                message("a", Some("root"), true),
                message("root", None, true),
            ],
        ),
        ("unknown_parent", vec![message("a", Some("missing"), true)]),
        (
            "duplicate_message_id",
            vec![
                message("root", None, true),
                message("root", Some("root"), false),
            ],
        ),
        ("no_active_path", vec![message("root", None, false)]),
    ];
    for (index, (expected, messages)) in cases.into_iter().enumerate() {
        let id = format!("invalid-{index}");
        let error = store
            .import_conversation(ImportConversation {
                chat: chat(&id),
                messages,
            })
            .await
            .unwrap_err();
        assert!(matches!(error,StoreError::InvalidImport {reason} if reason==expected));
        assert!(store.get_chat(&id).await.unwrap().is_none());
        assert!(store.get_message("root").await.unwrap().is_none());
        assert_eq!(store.list_chats(true).await.unwrap().len(), 1);
    }
}

#[tokio::test]
async fn duplicate_chat_is_invalid_import_and_preserves_existing_conversation() {
    let store = SqliteStore::memory(8).unwrap();
    store
        .create_chat("taken", Some("original".into()), json!({"keep":true}), 1)
        .await
        .unwrap();
    store
        .append_message(
            MessageInput {
                id: Some("existing".into()),
                chat_id: "taken".into(),
                content: json!("mine"),
                ..Default::default()
            },
            None,
            2,
        )
        .await
        .unwrap();
    let before = store.get_chat("taken").await.unwrap();
    let error = store
        .import_conversation(ImportConversation {
            chat: chat("taken"),
            messages: vec![message("replacement", None, true)],
        })
        .await
        .unwrap_err();
    assert!(matches!(error,StoreError::InvalidImport {reason} if reason=="duplicate_chat"));
    assert_eq!(store.get_chat("taken").await.unwrap(), before);
    assert_eq!(
        store.messages("taken", true).await.unwrap()[0].content,
        json!("mine")
    );
    assert!(store.get_message("replacement").await.unwrap().is_none());
}

#[tokio::test]
async fn import_strips_source_run_identity_and_normalizes_chat_time_but_keeps_tool_metadata() {
    let store = SqliteStore::memory(8).unwrap();
    let mut assistant = message("assistant", None, true);
    assistant.role = "assistant".into();
    assistant.tool_calls = Some(json!([{"id":"call","name":"lookup","argumentsJson":"{}"}]));
    assistant.metadata = json!({"internal":true,"runId":"opaque-metadata-kept"});
    let expected_calls = assistant.tool_calls.clone();
    let expected_metadata = assistant.metadata.clone();
    let mut tool = message("tool", Some("assistant"), true);
    tool.role = "tool".into();
    tool.tool_call_id = Some("call".into());
    tool.model_result_json = Some("{\"ok\":true}".into());
    tool.metadata = json!({"internal":true,"toolName":"lookup"});
    let imported = store
        .import_conversation(ImportConversation {
            chat: chat("imported"),
            messages: vec![assistant, tool],
        })
        .await
        .unwrap();
    assert_eq!(imported.updated_at, imported.created_at);
    let messages = store.messages("imported", false).await.unwrap();
    assert!(messages.iter().all(|message| message.run_id.is_none()));
    assert_eq!(messages[0].metadata, expected_metadata);
    assert_eq!(messages[0].tool_calls, expected_calls);
    assert_eq!(messages[1].tool_call_id.as_deref(), Some("call"));
    assert_eq!(
        messages[1].model_result_json.as_deref(),
        Some("{\"ok\":true}")
    );
    assert_eq!(messages[0].order_key, 1);
    assert_eq!(messages[1].depth, 1);
    assert!(store.get_continuation("old-run").await.unwrap().is_none());
}
