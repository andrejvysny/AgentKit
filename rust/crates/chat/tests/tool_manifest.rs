//! The frozen tool manifest on the live session path (roadmap 1.3, CE-04):
//! the tools array of turn two is byte-identical to turn one however the
//! contributors changed, and a late tool joins the deferred catalog.
#![cfg(unix)]

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use agentkit_chat::*;
use agentkit_core::{
    ChatProvider, ChatRequest, CoreError, ProviderEvent, ProviderStream, Tool, ToolContext,
    ToolDefinition, ToolRegistry, ToolResult,
};
use agentkit_engine::{SharedRuntime, SystemRuntime, TaskExecutor};
use agentkit_sessions::*;
use agentkit_store::{SqliteStore, TaskStatus};
use async_trait::async_trait;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

struct Capturing {
    tools: Arc<Mutex<Vec<Vec<ToolDefinition>>>>,
}
#[async_trait]
impl ChatProvider for Capturing {
    fn id(&self) -> &str {
        "test"
    }
    fn stream_chat(&self, input: ChatRequest, _: CancellationToken) -> ProviderStream {
        self.tools.lock().unwrap().push(input.tools.clone());
        Box::pin(futures::stream::iter(vec![Ok(ProviderEvent::Completed {
            content: "answer".into(),
            finish_reason: "stop".into(),
            tool_calls: vec![],
        })]))
    }
}

struct Read {
    definition: ToolDefinition,
}
#[async_trait]
impl Tool for Read {
    fn definition(&self) -> &ToolDefinition {
        &self.definition
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

fn registry(names: &[&str]) -> Arc<ToolRegistry> {
    let mut registry = ToolRegistry::new();
    for name in names {
        let definition = serde_json::from_value(json!({"name":name,"version":"1","effect":"read",
            "capability":"test","description":format!("{name} tool"),"inputSchema":{"type":"object"}}))
        .unwrap();
        registry.register(Arc::new(Read { definition })).unwrap();
    }
    Arc::new(registry)
}

struct Rig {
    host: AgentHost,
    service: AgentSessionService<SqliteStore>,
    tools: Arc<Mutex<Vec<Vec<ToolDefinition>>>>,
}

fn rig(store: &SqliteStore, tools: Arc<ToolRegistry>) -> Rig {
    let seen = Arc::new(Mutex::new(vec![]));
    let executor: Arc<dyn TaskExecutor<SqliteStore>> = Arc::new(ChatExecutor::new(
        Arc::new(Capturing {
            tools: Arc::clone(&seen),
        }),
        tools,
    ));
    let runtime: SharedRuntime = Arc::new(SystemRuntime);
    let host = AgentHost::new(
        store.clone(),
        HashMap::from([(CHAT_TURN_TASK_KIND.to_owned(), executor)]),
        Arc::clone(&runtime),
        HostOptions::default(),
    )
    .unwrap();
    let service = AgentSessionService::new(AgentSessionServiceDeps {
        store: store.clone(),
        runtime,
        plan_approval: None,
        wake: Some(host.wake_handle()),
    });
    Rig {
        host,
        service,
        tools: seen,
    }
}

async fn turn(rig: &Rig, store: &SqliteStore, input_id: &str) -> String {
    rig.host
        .set_session_flush_planner(Arc::new(rig.service.clone()))
        .await;
    let sent = rig
        .service
        .send(
            "s1",
            &SendInput {
                frozen_payload: None,
                content: json!("hi"),
                input_id: Some(input_id.into()),
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
        rig.host.run_once().await.expect("run");
        let task = store.get_task(&turn_id).await.unwrap().unwrap();
        if task.status.is_terminal() {
            assert_eq!(task.status, TaskStatus::Completed);
            return turn_id;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("turn did not settle");
}

async fn warnings(store: &SqliteStore, turn: &str, code: &str) -> Vec<Value> {
    store
        .events(turn, None, 1000)
        .await
        .unwrap()
        .into_iter()
        .filter(|event| event.event["type"] == "run.warning" && event.event["data"]["code"] == code)
        .map(|event| event.event["data"].clone())
        .collect()
}

fn names(tools: &[ToolDefinition]) -> Vec<&str> {
    tools.iter().map(|tool| tool.name.as_str()).collect()
}

#[tokio::test]
async fn the_tools_array_is_frozen_sorted_and_late_tools_are_deferred() {
    let store = SqliteStore::memory(64).expect("store");
    // Turn one: contributors answer in reverse name order.
    let first = rig(&store, registry(&["read_file", "dir_list", "file_finder"]));
    first
        .service
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
    let one = turn(&first, &store, "in1").await;
    let sent_one = first.tools.lock().unwrap()[0].clone();
    assert_eq!(names(&sent_one), ["dir_list", "file_finder", "read_file"]);
    let chat = store.get_chat("s1").await.unwrap().unwrap();
    let frozen = read_session_prefix(&chat.metadata).expect("prefix recorded");
    assert!(!frozen.tool_manifest_hash.is_absent());

    // Turn two: file_finder timed out, text_search joined late. (The tools
    // are not named after coding ids: a new session presents those through
    // the common dialect, T3.1.)
    let second = rig(&store, registry(&["text_search", "read_file", "dir_list"]));
    let two = turn(&second, &store, "in2").await;
    let sent_two = second.tools.lock().unwrap()[0].clone();
    assert_eq!(
        serde_json::to_string(&sent_two).unwrap(),
        serde_json::to_string(&sent_one).unwrap(),
        "S1 is byte-identical"
    );

    let chat = store.get_chat("s1").await.unwrap().unwrap();
    let kept = read_session_prefix(&chat.metadata).expect("prefix kept");
    assert_eq!(kept.revision, 1.0, "no re-freeze");
    assert_eq!(kept.tools_digest, frozen.tools_digest);
    assert_eq!(kept.tool_manifest_hash, frozen.tool_manifest_hash);
    let agentkit_contracts::Optional::Present(catalog) = &kept.deferred_tools else {
        panic!("the late tool is listed in the catalog");
    };
    assert_eq!(catalog.len(), 1);
    assert_eq!(catalog[0].name, "text_search");

    assert!(warnings(&store, &one, "tool_deferred").await.is_empty());
    assert!(warnings(&store, &two, "prefix_refrozen").await.is_empty());
    assert_eq!(warnings(&store, &two, "tool_deferred").await.len(), 1);
    let gone = warnings(&store, &two, "tool_unavailable").await;
    assert_eq!(gone[0]["tools"], json!(["file_finder"]));
}
