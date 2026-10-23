#![cfg(feature = "http")]
#[path = "chat_continuation_matrix/support.rs"]
mod support;
use agentkit_store::{MessageInput, TaskStatus};
use serde_json::json;
use support::*;

async fn scope_change(case: &str) {
    let fixture = Fixture::start(success()).await;
    let saved;
    {
        let store = fixture.open();
        store.create_chat("chat", None, json!({}), 0).await.unwrap();
        let host = fixture.host(store.clone(), identity());
        turn(&host, "first", "model").await;
        assert_eq!(
            store.get_task("first").await.unwrap().unwrap().status,
            TaskStatus::Completed
        );
        saved = store.get_continuation("first").await.unwrap().unwrap();
    }
    {
        let store = fixture.open();
        let mut changed = identity();
        let mut model = "model";
        match case {
            "provider" => changed.provider_id = "other-provider".into(),
            "account" => changed.connection_id = "other-account".into(),
            "generation" => changed.generation += 1,
            "protocol" => changed.protocol = "chat-completions".into(),
            "model" => model = "other-model",
            "branch" => branch(&store).await,
            _ => unreachable!(),
        }
        let host = fixture.host(store.clone(), changed);
        turn(&host, "changed", model).await;
        assert_failed(&store, "changed", "provider_continuation_scope_mismatch").await;
        assert_rejected_scope(&fixture, &store, &saved.state, case).await;
    }
}
async fn assert_rejected_scope(
    fixture: &Fixture,
    store: &agentkit_store::SqliteStore,
    saved: &serde_json::Value,
    case: &str,
) {
    assert_eq!(
        fixture.request_count(),
        1,
        "{case}: must fail before provider dispatch"
    );
    assert_eq!(fixture.tool_count(), 0);
    assert!(store.get_continuation("changed").await.unwrap().is_none());
    assert_eq!(
        &store
            .get_continuation("first")
            .await
            .unwrap()
            .unwrap()
            .state,
        saved
    );
    let public = serde_json::to_string(&store.messages("chat", false).await.unwrap()).unwrap();
    assert!(!public.contains("private-secret"));
    assert!(!public.contains("encrypted_content"));
}
async fn branch(store: &agentkit_store::SqliteStore) {
    let parent = store
        .messages("chat", true)
        .await
        .unwrap()
        .last()
        .unwrap()
        .id
        .clone();
    for id in ["sibling-zero", "sibling-one"] {
        let message = store
            .append_message(
                MessageInput {
                    id: Some(id.into()),
                    chat_id: "chat".into(),
                    parent_message_id: Some(parent.clone()),
                    content: "branch question".into(),
                    ..Default::default()
                },
                None,
                1,
            )
            .await
            .unwrap();
        if id == "sibling-one" {
            assert_eq!(message.branch_index, 1);
        }
    }
}
#[tokio::test]
async fn reopened_continuation_rejects_provider_change() {
    scope_change("provider").await;
}
#[tokio::test]
async fn reopened_continuation_rejects_account_change() {
    scope_change("account").await;
}
#[tokio::test]
async fn reopened_continuation_rejects_generation_change() {
    scope_change("generation").await;
}
#[tokio::test]
async fn reopened_continuation_rejects_protocol_change() {
    scope_change("protocol").await;
}
#[tokio::test]
async fn reopened_continuation_rejects_model_change() {
    scope_change("model").await;
}
#[tokio::test]
async fn reopened_continuation_rejects_branch_change() {
    scope_change("branch").await;
}

async fn stream_failure(response: String) {
    let fixture = Fixture::start(response).await;
    {
        let store = fixture.open();
        store.create_chat("chat", None, json!({}), 0).await.unwrap();
        let host = fixture.host(store.clone(), identity());
        turn(&host, "failed", "model").await;
        assert_eq!(
            store.get_task("failed").await.unwrap().unwrap().status,
            TaskStatus::Failed
        );
        assert!(store.get_continuation("failed").await.unwrap().is_none());
        assert_eq!(fixture.tool_count(), 0);
        let events = store.events("failed", None, 1000).await.unwrap();
        assert!(!events.iter().any(|event| matches!(
            event.event["type"].as_str(),
            Some("run.tool.requested" | "run.tool.running" | "run.message.completed")
        )));
        let public = serde_json::to_string(&store.messages("chat", false).await.unwrap()).unwrap();
        assert!(!public.contains("private-secret"));
        assert!(!public.contains("encrypted_content"));
    }
    {
        let store = fixture.open();
        let host = fixture.host(store.clone(), identity());
        assert!(!host.run_once().await.unwrap());
        assert_eq!(
            fixture.request_count(),
            1,
            "Restart must not replay failed private work statelessly"
        );
        assert_eq!(fixture.tool_count(), 0);
        assert!(store.get_continuation("failed").await.unwrap().is_none());
        assert_eq!(
            store.get_task("failed").await.unwrap().unwrap().status,
            TaskStatus::Failed
        );
    }
}
#[tokio::test]
async fn private_stream_eof_without_terminal_saves_nothing_and_runs_no_tool() {
    let mut response =
        frame(json!({"type":"response.output_item.done","output_index":0,"item":reasoning()}));
    response.push_str(&frame(
        json!({"type":"response.output_item.done","output_index":1,"item":function()}),
    ));
    stream_failure(response).await;
}
#[tokio::test]
async fn private_incomplete_terminal_saves_nothing_and_runs_no_tool() {
    stream_failure(frame(json!({"type":"response.incomplete","response":{"status":"incomplete","output":[reasoning(),function()]}}))).await;
}
#[tokio::test]
async fn private_completed_then_late_failure_saves_nothing_and_runs_no_tool() {
    let mut response = completed(vec![reasoning(), function()]);
    response.push_str(&frame(
        json!({"type":"error","code":"late_failure","message":"late provider failure"}),
    ));
    stream_failure(response).await;
}
#[tokio::test]
async fn missing_private_state_after_reopen_fails_before_stateless_fallback() {
    let fixture = Fixture::start(success()).await;
    {
        let store = fixture.open();
        store.create_chat("chat", None, json!({}), 0).await.unwrap();
        let host = fixture.host(store.clone(), identity());
        turn(&host, "first", "model").await;
        assert!(store.get_continuation("first").await.unwrap().is_some());
    }
    {
        let db = rusqlite::Connection::open(&fixture.path).unwrap();
        assert_eq!(
            db.execute("DELETE FROM continuations WHERE run_id='first'", [])
                .unwrap(),
            1
        );
    }
    {
        let store = fixture.open();
        let host = fixture.host(store.clone(), identity());
        turn(&host, "next", "model").await;
        assert_failed(&store, "next", "provider_continuation_state_missing").await;
        assert_eq!(fixture.request_count(), 1);
        assert_eq!(fixture.tool_count(), 0);
    }
}
