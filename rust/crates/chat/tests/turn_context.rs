//! T4.4: the per-turn context hook. The line reaches the turn's user
//! message only (never the system prompt, never the stored history), and the
//! default hook recommends toolsets from the session's toolset view.

use agentkit_chat::*;
use agentkit_core::{
    CancellationToken, ChatProvider, ChatRequest, CoreError, ProviderEvent, ProviderStream, Tool,
    ToolContext, ToolDefinition, ToolRegistry, ToolResult, Toolset,
};
use agentkit_engine::{SharedRuntime, SystemRuntime, TaskExecutor};
use agentkit_sessions::*;
use agentkit_store::{SqliteStore, TaskStatus};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

fn definition(name: &str) -> ToolDefinition {
    serde_json::from_value(json!({
        "name": name, "version": "1", "effect": "read", "capability": "notes.access",
        "description": format!("{name} for notes."),
        "inputSchema": {"type": "object", "properties": {}, "additionalProperties": false}
    }))
    .unwrap()
}

struct Body(ToolDefinition);

#[async_trait]
impl Tool for Body {
    fn definition(&self) -> &ToolDefinition {
        &self.0
    }
    async fn execute(&self, context: ToolContext, _: Value) -> Result<ToolResult, CoreError> {
        Ok(ToolResult {
            ok: true,
            data: Some(json!({})),
            sources: vec![],
            warnings: vec![],
            truncated: false,
            limits: context.limits,
            model_data: None,
            summary: None,
            status: None,
            model_content: None,
        })
    }
}

/// Records the system text and the last user message of every request.
struct Capturing {
    seen: Arc<Mutex<Vec<(String, String)>>>,
}

#[async_trait]
impl ChatProvider for Capturing {
    fn id(&self) -> &str {
        "test"
    }
    fn stream_chat(&self, input: ChatRequest, _: CancellationToken) -> ProviderStream {
        let system = input
            .messages
            .iter()
            .filter(|m| m.role == "system")
            .map(|m| m.content.to_string())
            .collect::<String>();
        let user = input
            .messages
            .iter()
            .rev()
            .find(|m| m.role == "user")
            .map(|m| m.content.to_string())
            .unwrap_or_default();
        self.seen.lock().unwrap().push((system, user));
        Box::pin(futures::stream::iter(vec![Ok(ProviderEvent::Completed {
            content: "answer".into(),
            finish_reason: "stop".into(),
            tool_calls: vec![],
        })]))
    }
}

struct FixedLine;

#[async_trait]
impl TurnContextHook for FixedLine {
    async fn line(&self, input: &TurnContextInput<'_>) -> Option<String> {
        Some(format!("HINT for {}", input.request))
    }
}

#[tokio::test]
async fn the_line_is_appended_to_the_user_message_and_nowhere_else() {
    let store = SqliteStore::memory(64).expect("store");
    let seen = Arc::new(Mutex::new(vec![]));
    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(Body(definition("notes_read")))).unwrap();
    let executor: Arc<dyn TaskExecutor<SqliteStore>> = Arc::new(
        ChatExecutor::new(
            Arc::new(Capturing { seen: Arc::clone(&seen) }),
            Arc::new(registry),
        )
        .with_options(ChatExecutorOptions {
            turn_context: Some(Arc::new(FixedLine)),
            ..Default::default()
        }),
    );
    let runtime: SharedRuntime = Arc::new(SystemRuntime);
    let host = agentkit_engine::AgentHost::new(
        store.clone(),
        HashMap::from([(CHAT_TURN_TASK_KIND.to_owned(), executor)]),
        Arc::clone(&runtime),
        agentkit_engine::HostOptions::default(),
    )
    .unwrap();
    let service = AgentSessionService::new(AgentSessionServiceDeps {
        store: store.clone(),
        runtime,
        plan_approval: None,
        wake: Some(host.wake_handle()),
    });
    service
        .create_session(&CreateSessionInput {
            session_id: Some("s1".into()),
            title: None,
            profile_id: "coding".into(),
            profile_digest: None,
            reasoning_effort: None,
            content_selection: None,
            provider_id: None,
            model: Some("model".into()),
            workspace: None,
            context: None,
            skills: None,
            turn_kind: None,
            execution_mode: None,
            metadata: None,
        })
        .await
        .unwrap();
    host.set_session_flush_planner(Arc::new(service.clone())).await;
    let sent = service
        .send(
            "s1",
            &SendInput {
                frozen_payload: None,
                content: json!("read my notes"),
                input_id: Some("in1".into()),
                when_busy: WhenBusy::Reject,
                turn_id: None,
                on_settled: None,
            },
        )
        .await
        .unwrap();
    let AgentSendResult::Turn { turn_id, .. } = sent else {
        panic!("an idle session starts the turn");
    };
    for _ in 0..10 {
        host.run_once().await.expect("run");
        let task = store.get_task(&turn_id).await.unwrap().unwrap();
        if task.status.is_terminal() {
            assert_eq!(task.status, TaskStatus::Completed);
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let (system, user) = seen.lock().unwrap()[0].clone();
    assert!(user.contains("read my notes"), "{user}");
    assert!(user.contains("HINT for read my notes"), "{user}");
    assert!(!system.contains("HINT"), "never S1-S4: {system}");
    let stored = store.messages("s1", true).await.unwrap();
    assert!(
        stored.iter().all(|m| !m.content.to_string().contains("HINT")),
        "the line is not persisted"
    );
}

fn view_and_tools() -> (agentkit_core::ToolsetView, Vec<ToolDefinition>) {
    let mut registry = ToolRegistry::new();
    registry.register_toolset(
        Toolset::new("shell", "Shell", "Run shell commands and tests.").with_tools(&["shell_run"]),
    );
    registry.register_toolset(
        Toolset::new("calendar", "Calendar", "Read and write calendar events.")
            .with_tools(&["calendar_list"]),
    );
    let tools = vec![definition("shell_run")];
    (registry.toolset_view(&tools, None), tools)
}

#[tokio::test]
async fn the_default_hook_recommends_unloaded_toolsets_from_the_view() {
    let (view, tools) = view_and_tools();
    let hook = ToolsetRecommendation::new();
    let input = TurnContextInput {
        chat_id: "c1",
        request: "add a calendar event for monday",
        tools: &tools,
        view: &view,
    };
    let line = hook.line(&input).await.expect("a line");
    assert!(line.starts_with("Likely useful toolsets: calendar"), "{line}");
    assert!(!line.contains('\n'));
    // A toolset that is already loaded is not suggested again.
    let loaded = TurnContextInput { request: "run the tests in the shell", ..input };
    let line = hook.line(&loaded).await;
    assert!(line.is_none_or(|l| !l.contains("shell")), "loaded set suggested");
    // Nothing relevant, no line.
    let none = TurnContextInput { request: "zzz qqq", ..loaded };
    assert!(hook.line(&none).await.is_none());
}
