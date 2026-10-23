mod support;
use agentkit_store::*;
use serde_json::json;
use support::*;

#[tokio::test]
async fn branches_and_chain_appends_preserve_one_live_path() {
    let store = SqliteStore::memory(4).unwrap();
    store
        .create_chat("chat", None, json!({}), 1000)
        .await
        .unwrap();
    store
        .append_message(message("root", "chat", None), None, 1000)
        .await
        .unwrap();
    store
        .append_message(message("first", "chat", None), None, 1000)
        .await
        .unwrap();
    let mut branch = message("second", "chat", None);
    branch.parent_message_id = Some("root".into());
    store.append_message(branch, None, 1000).await.unwrap();
    let live = store.messages("chat", false).await.unwrap();
    assert_eq!(
        live.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
        vec!["root", "second"]
    );
    let mut late = message("late", "chat", None);
    late.parent_message_id = Some("first".into());
    late.activate = false;
    assert!(!store.append_message(late, None, 1001).await.unwrap().active);
    let path = store.activate_path("first").await.unwrap();
    assert_eq!(path.last().unwrap().id, "late");
    let mut next = message("next", "chat", None);
    next.parent_message_id = Some("late".into());
    next.activate = false;
    assert!(store.append_message(next, None, 1001).await.unwrap().active);
    let mut sibling = message("sibling", "chat", None);
    sibling.parent_message_id = Some("late".into());
    sibling.activate = false;
    assert!(
        !store
            .append_message(sibling, None, 1001)
            .await
            .unwrap()
            .active
    );
    assert_eq!(
        store
            .messages("chat", false)
            .await
            .unwrap()
            .last()
            .unwrap()
            .id,
        "next"
    );
}

#[tokio::test]
async fn import_validates_before_writes_and_derives_tree_fields() {
    let store = SqliteStore::memory(4).unwrap();
    store
        .create_chat("source", None, json!({}), 1000)
        .await
        .unwrap();
    let a = store
        .append_message(message("a", "source", None), None, 1000)
        .await
        .unwrap();
    let b = store
        .append_message(message("b", "source", None), None, 1000)
        .await
        .unwrap();
    let chat = ChatRecord {
        id: "imported".into(),
        title: None,
        metadata: json!({}),
        archived: false,
        created_at: 1000.into(),
        updated_at: 1000.into(),
    };
    let mut imported_a = a.clone();
    imported_a.id = "import-a".into();
    imported_a.order_key = 999;
    imported_a.depth = 999;
    let mut imported_b = b.clone();
    imported_b.id = "import-b".into();
    imported_b.parent_message_id = Some("missing".into());
    assert!(
        store
            .import_conversation(ImportConversation {
                chat: chat.clone(),
                messages: vec![imported_a.clone(), imported_b.clone()]
            })
            .await
            .is_err()
    );
    assert!(store.get_chat("imported").await.unwrap().is_none());
    imported_b.parent_message_id = Some("import-a".into());
    store
        .import_conversation(ImportConversation {
            chat,
            messages: vec![imported_a, imported_b],
        })
        .await
        .unwrap();
    let messages = store.messages("imported", true).await.unwrap();
    assert_eq!(messages[0].order_key, 1);
    assert_eq!(messages[0].depth, 0);
    assert_eq!(messages[1].depth, 1);
    let (fork, messages) = store
        .fork_chat("imported", "import-b", "fork", 1001)
        .await
        .unwrap();
    assert!(!fork.archived);
    assert_eq!(messages.len(), 2);
    assert_ne!(messages[0].id, "import-a");
}

#[tokio::test]
async fn search_includes_all_text_parts_and_inactive_branches_but_hides_private_records() {
    let store = SqliteStore::memory(4).unwrap();
    store
        .create_chat("chat", None, json!({}), 1000)
        .await
        .unwrap();
    let mut multi = message("multi", "chat", None);
    multi.content = json!([{"type":"text","text":"first"},{"type":"text","text":"quartz beacon"}]);
    store.append_message(multi, None, 1000).await.unwrap();
    let mut other = message("other", "chat", None);
    other.parent_message_id = Some("multi".into());
    other.content = json!("quartz beacon quartz beacon");
    store.append_message(other, None, 1000).await.unwrap();
    let mut private = message("private", "chat", None);
    private.content = json!("quartz beacon");
    private.metadata = json!({"internal":true});
    store.append_message(private, None, 1000).await.unwrap();
    let hits = store
        .search_messages("quartz beacon", None, 10)
        .await
        .unwrap();
    assert_eq!(hits.len(), 2);
    assert_eq!(hits[0].message.id, "other");
    assert!(
        store
            .search_messages("***", None, 10)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn deletion_refuses_live_scope_and_cleans_durable_owned_state() {
    let store = SqliteStore::memory(4).unwrap();
    store
        .create_chat("chat", None, json!({}), 1000)
        .await
        .unwrap();
    store
        .submit_turn(task("run", "chat"), vec![message("a", "chat", Some("run"))])
        .await
        .unwrap();
    let lease = store.claim(claim(1000)).await.unwrap().unwrap().lease;
    assert!(matches!(
        store.delete_chat("chat").await,
        Err(StoreError::ScopeBusy(_))
    ));
    finish(&store, lease, TaskStatus::Completed, 1001).await;
    store.delete_chat("chat").await.unwrap();
    assert!(store.get_task("run").await.unwrap().is_none());
    assert!(store.get_chat("chat").await.unwrap().is_none());
    assert!(store.outbox().await.unwrap().is_empty());
}

#[tokio::test]
async fn fork_refuses_inactive_branch_and_omits_placeholder_and_run_identity() {
    let store = SqliteStore::memory(2).unwrap();
    store
        .create_chat("chat", Some("title".into()), json!({}), 1000)
        .await
        .unwrap();
    store
        .append_message(message("root", "chat", Some("run")), None, 1000)
        .await
        .unwrap();
    store
        .append_message(message("old", "chat", Some("run")), None, 1000)
        .await
        .unwrap();
    let mut placeholder = message("placeholder", "chat", Some("run"));
    placeholder.parent_message_id = Some("root".into());
    placeholder.metadata = json!({"placeholder":true});
    store.append_message(placeholder, None, 1000).await.unwrap();
    assert!(
        matches!(store.fork_chat("chat","old","bad",1001).await,Err(StoreError::InvalidInput(message)) if message=="invalid_fork_point")
    );
    let (chat, messages) = store
        .fork_chat("chat", "placeholder", "copy", 1001)
        .await
        .unwrap();
    assert_eq!(chat.title.as_deref(), Some("Fork of title"));
    assert_eq!(messages.len(), 1);
    assert!(messages[0].run_id.is_none());
    assert!(messages[0].metadata.get("placeholder").is_none());
    store.submit(task("queued", "chat")).await.unwrap();
    assert!(matches!(
        store.delete_chat("chat").await,
        Err(StoreError::ScopeBusy(_))
    ));
}

#[tokio::test]
async fn provider_order_keeps_user_before_each_linked_tool_pass() {
    let store = SqliteStore::memory(2).unwrap();
    store
        .create_chat("chat", None, json!({}), 1000)
        .await
        .unwrap();
    store
        .append_message(message("user", "chat", Some("run")), None, 1000)
        .await
        .unwrap();
    let mut answer = message("answer", "chat", Some("run"));
    answer.role = "assistant".into();
    store.append_message(answer, None, 1000).await.unwrap();
    for (name, call) in [("internal1", "call1"), ("internal2", "call2")] {
        let mut assistant = message(name, "chat", Some("run"));
        assistant.role = "assistant".into();
        assistant.metadata = json!({"internal":true});
        assistant.tool_calls = Some(json!([{"id":call}]));
        store.append_message(assistant, None, 1000).await.unwrap();
    }
    for (name, call) in [("result2", "call2"), ("result1", "call1")] {
        let mut result = message(name, "chat", Some("run"));
        result.role = "tool".into();
        result.tool_call_id = Some(call.into());
        store.append_message(result, None, 1000).await.unwrap();
    }
    let records = order_messages_for_provider(store.messages("chat", false).await.unwrap());
    assert_eq!(
        records.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
        vec![
            "user",
            "internal1",
            "result1",
            "internal2",
            "result2",
            "answer"
        ]
    );
}
