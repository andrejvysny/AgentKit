use agentkit_host::{
    AgentSessionService, AgentSessionServiceDeps, CreateSessionInput, SystemRuntime,
};
use agentkit_store::{ClaimInput, ClaimedTask, SqliteStore, SubmitTask};
use serde_json::{Value, json};
use std::sync::Arc;

async fn fixture() -> (SqliteStore, AgentSessionService) {
    let store = SqliteStore::memory(16).unwrap();
    let service = AgentSessionService::new(AgentSessionServiceDeps {
        store: store.clone(),
        runtime: Arc::new(SystemRuntime),
        plan_approval: None,
        wake: None,
    });
    service
        .create_session(&CreateSessionInput {
            session_id: Some("session".into()),
            profile_id: "p".into(),
            provider_id: Some("provider".into()),
            model: Some("old".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    (store, service)
}

async fn submit(store: &SqliteStore, id: &str, model: &str, context: Value, now: i64) {
    store
        .submit(SubmitTask {
            task_id: id.into(),
            kind: "chat.turn".into(),
            scope_id: "session".into(),
            payload: json!({"model":model, "providerId":"provider", "sessionContext":context}),
            priority: 0,
            now,
            available_at: None,
            depends_on: vec![],
            parent_task_id: None,
            exclusive_scope: false,
        })
        .await
        .unwrap();
}

async fn claim(store: &SqliteStore) -> ClaimedTask {
    store
        .claim(ClaimInput {
            owner_id: "worker".into(),
            now: 1000,
            ttl_ms: 60000,
            kinds: vec![],
            busy_scopes: vec![],
            max_attempts: 3,
        })
        .await
        .unwrap()
        .unwrap()
}

async fn catalogue(store: &SqliteStore, namespace: &str, rows: Value) {
    store
        .set_setting(namespace, "provider", rows)
        .await
        .unwrap();
}

#[tokio::test]
async fn usage_uses_admission_catalogue_precedence() {
    let (store, service) = fixture().await;
    submit(&store, "turn", "old", Value::Null, 1000).await;
    catalogue(
        &store,
        "http.provider-catalog",
        json!([{"modelId":"old", "contextWindowTokens":8192}]),
    )
    .await;
    catalogue(
        &store,
        agentkit_store::PROVIDER_MODELS_NAMESPACE,
        json!([{"modelId":"old", "contextWindowTokens":4096}]),
    )
    .await;
    let usage = service.get_usage("session").await.unwrap();
    assert_eq!(usage["contextWindowTokens"], 4096);
    assert_eq!(usage["contextWindowSource"], "model");
}

#[tokio::test]
async fn usage_model_switch_keeps_running_turn_before_queued_future_settings() {
    let (store, service) = fixture().await;
    catalogue(&store, agentkit_store::PROVIDER_MODELS_NAMESPACE,
        json!([{"modelId":"old", "contextWindowTokens":4096},{"modelId":"new", "contextWindowTokens":8192}])).await;
    submit(
        &store,
        "old-turn",
        "old",
        json!({"windowTokens":2048}),
        1000,
    )
    .await;
    let claimed = claim(&store).await;
    service
        .set_model("session", None, "new", None)
        .await
        .unwrap();
    submit(&store, "new-turn", "new", Value::Null, 1001).await;
    let active = service.get_usage("session").await.unwrap();
    assert_eq!(active["contextTurnId"], "old-turn");
    assert_eq!(active["contextWindowTokens"], 2048);
    assert_eq!(active["contextWindowSource"], "descriptor");
    store.cancel_task("old-turn", 1002).await.unwrap();
    store
        .settle(agentkit_store::SettleTask {
            fence: claimed.lease.fence(),
            now: 1003,
            status: agentkit_store::TaskStatus::Cancelled,
            error: None,
            events: vec![],
            messages: vec![],
        })
        .await
        .unwrap();
    let future = service.get_usage("session").await.unwrap();
    assert_eq!(future["contextTurnId"], "new-turn");
    assert_eq!(future["contextWindowTokens"], 8192);
}

#[tokio::test]
async fn settled_admission_remains_authoritative_after_catalogue_changes() {
    let (store, service) = fixture().await;
    submit(&store, "turn", "old", Value::Null, 1000).await;
    let task = claim(&store).await;
    let admission = json!({"requestId":"request", "windowTokens":6000, "windowSource":"model", "reserveTokens":3000});
    store.append_events(task.lease.fence(), vec![json!({
        "type":"run.context.admitted", "seq":0, "eventId":"admission", "timestamp":"2026-11-05T00:00:00.000Z",
        "contractVersion":"0.7.0", "data":admission,
    })], 1001).await.unwrap();
    store.cancel_task("turn", 1002).await.unwrap();
    store
        .settle(agentkit_store::SettleTask {
            fence: task.lease.fence(),
            now: 1003,
            status: agentkit_store::TaskStatus::Cancelled,
            error: None,
            events: vec![],
            messages: vec![],
        })
        .await
        .unwrap();
    catalogue(
        &store,
        agentkit_store::PROVIDER_MODELS_NAMESPACE,
        json!([{"modelId":"old", "contextWindowTokens":12000}]),
    )
    .await;
    service
        .set_model("session", None, "new", None)
        .await
        .unwrap();
    let usage = service.get_usage("session").await.unwrap();
    assert_eq!(usage["contextWindowTokens"], 6000);
    assert_eq!(usage["contextAdmission"], admission);
}

#[tokio::test]
async fn session_without_an_admitted_turn_has_unknown_window() {
    let (_, service) = fixture().await;
    let usage = service.get_usage("session").await.unwrap();
    assert_eq!(usage["contextWindowTokens"], Value::Null);
    assert_eq!(usage["contextWindowSource"], "unknown");
}
