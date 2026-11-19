use super::*;
use agentkit_core::{ChatRequest, ProviderStream};
use agentkit_store::{ClaimInput, SqliteStore, SubmitTask};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

struct Clock;
impl crate::Runtime for Clock {
    fn now_ms(&self) -> i64 {
        1000
    }
    fn id(&self) -> String {
        "test".into()
    }
}
struct Noop;
#[async_trait]
impl ChatProvider for Noop {
    fn id(&self) -> &str {
        "noop"
    }
    fn stream_chat(&self, _: ChatRequest, _: CancellationToken) -> ProviderStream {
        Box::pin(futures::stream::empty())
    }
}

async fn executor_context(store: SqliteStore) -> ExecutionContext {
    store
        .submit(SubmitTask {
            task_id: "run".into(),
            kind: CHAT_TURN_TASK_KIND.into(),
            scope_id: "chat".into(),
            payload: json!({}),
            priority: 0,
            now: 1000,
            available_at: None,
            depends_on: vec![],
            parent_task_id: None,
            exclusive_scope: false,
        })
        .await
        .unwrap();
    let claimed = store
        .claim(ClaimInput {
            owner_id: "worker".into(),
            now: 1000,
            ttl_ms: 60_000,
            kinds: vec![],
            busy_scopes: vec![],
            max_attempts: 3,
        })
        .await
        .unwrap()
        .unwrap();
    ExecutionContext {
        task: claimed.task,
        attempt_id: claimed.lease.attempt_id.clone(),
        cancellation: CancellationToken::new(),
        interruption: CancellationToken::new(),
        suspension: Arc::new(Mutex::new(None)),
        store,
        fence: claimed.lease.fence(),
        runtime: Arc::new(Clock),
        event_gate: Arc::new(Mutex::new(())),
    }
}

#[tokio::test]
async fn tool_calling_setting_and_recorded_capabilities_gate_staging() {
    let store = SqliteStore::memory(2).unwrap();
    let context = executor_context(store.clone()).await;
    let executor = AgentExecutor::new(Arc::new(Noop), Arc::new(ToolRegistry::new()));
    // No settings document, no capability record: tools stage.
    assert!(executor.stage_tools(&context, "p").await.unwrap());
    // "auto" (absent) follows the recorded probe: `toolCalling: false`
    // stages nothing — handing tools over is the exact request shape
    // that fails a chat-only provider.
    store
        .save_provider_capabilities(
            "p",
            json!({"streaming": true, "toolCalling": false, "modelList": true}),
        )
        .await
        .unwrap();
    assert!(!executor.stage_tools(&context, "p").await.unwrap());
    // "on" overrides a wrong probe — the whole reason the knob exists.
    store
        .set_setting("http", "settings", json!({"toolCalling": "on"}))
        .await
        .unwrap();
    assert!(executor.stage_tools(&context, "p").await.unwrap());
    // "off" stages nothing for ANY provider, probe or no probe.
    store
        .set_setting("http", "settings", json!({"toolCalling": "off"}))
        .await
        .unwrap();
    assert!(!executor.stage_tools(&context, "other").await.unwrap());
    // "auto" again: an unrecorded provider stages (absence never disables).
    store
        .set_setting("http", "settings", json!({"toolCalling": "auto"}))
        .await
        .unwrap();
    assert!(executor.stage_tools(&context, "other").await.unwrap());
}
#[tokio::test]
async fn session_tasks_without_frozen_profiles_fail_before_dispatch() {
    let store = SqliteStore::memory(2).unwrap();
    store.create_chat("chat",None,json!({"agentkit.session":{"version":1,"profile":{"id":"coding"},"createdAt":"2026-01-01T00:00:00.000Z"}}),1000).await.unwrap();
    let mut context = executor_context(store).await;
    context.task.payload = json!({"chatId":"chat","model":"model"});
    let executor = AgentExecutor::new(Arc::new(Noop), Arc::new(ToolRegistry::new()));
    assert!(
        executor
            .execute(context)
            .await
            .unwrap_err()
            .to_string()
            .contains("profile_snapshot_required")
    );
}
