//! The session prefix record (ADR 0028) on the live session path: the system
//! segments reach the provider as separate tagged blocks in segment order, the
//! frozen head is recorded on the first turn, unchanged turns keep it, and an
//! edited rules file re-freezes it under a new revision with a durable warning.
#![cfg(unix)]

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use agentkit_chat::*;
use agentkit_core::{ChatProvider, ChatRequest, ProviderEvent, ProviderStream, ToolRegistry};
use agentkit_engine::{SharedRuntime, SystemRuntime, TaskExecutor};

use agentkit_sessions::*;
use agentkit_store::{SqliteStore, TaskStatus};
use async_trait::async_trait;
use serde_json::json;
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

struct Answer;
#[async_trait]
impl ChatProvider for Answer {
    fn id(&self) -> &str {
        "test"
    }
    fn stream_chat(&self, _input: ChatRequest, _: CancellationToken) -> ProviderStream {
        Box::pin(futures::stream::iter(vec![Ok(ProviderEvent::Completed {
            content: "answer".into(),
            finish_reason: "stop".into(),
            tool_calls: vec![],
        })]))
    }
}

struct Capturing {
    inner: Answer,
    requests: Arc<Mutex<Vec<ChatRequest>>>,
}
#[async_trait]
impl ChatProvider for Capturing {
    fn id(&self) -> &str {
        "test"
    }
    fn stream_chat(&self, input: ChatRequest, cancel: CancellationToken) -> ProviderStream {
        self.requests.lock().unwrap().push(input.clone());
        self.inner.stream_chat(input, cancel)
    }
}

fn workspace() -> TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("CLAUDE.md"), b"rule one\n").expect("claude");
    dir
}

async fn drive(host: &AgentHost, store: &SqliteStore, turn_id: &str) -> TaskStatus {
    for _ in 0..10 {
        // A session turn claims through the ordinary task pipeline.
        host.run_once().await.expect("run");
        let task = store
            .get_task(turn_id)
            .await
            .expect("task")
            .expect("turn task exists");
        if task.status.is_terminal() {
            return task.status;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("turn did not settle");
}

async fn send(service: &AgentSessionService<SqliteStore>, input_id: &str) -> String {
    let sent = service
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
    match sent {
        AgentSendResult::Turn { turn_id, .. } => turn_id,
        AgentSendResult::Queued { .. } => panic!("an idle session starts the turn"),
    }
}

async fn prefix_of(store: &SqliteStore) -> Option<agentkit_contracts::AiSessionPrefix> {
    let chat = store.get_chat("s1").await.unwrap().unwrap();
    read_session_prefix(&chat.metadata)
}

async fn refreeze_warnings(store: &SqliteStore, turn: &str) -> Vec<agentkit_engine::EventRecord> {
    store
        .events(turn, None, 1000)
        .await
        .unwrap()
        .into_iter()
        .filter(|event| {
            event.event["type"] == "run.warning" && event.event["data"]["code"] == "prefix_refrozen"
        })
        .collect()
}

fn leading_system(request: &ChatRequest) -> Vec<(Option<agentkit_core::Segment>, String)> {
    request
        .messages
        .iter()
        .take_while(|message| message.role == "system")
        .map(|message| {
            (
                agentkit_core::segment_of(message),
                message.content.as_str().expect("system text").to_owned(),
            )
        })
        .collect()
}

#[tokio::test]
async fn the_prefix_is_recorded_once_kept_while_unchanged_and_refrozen_on_edit() {
    let dir = workspace();
    let store = SqliteStore::memory(64).expect("store");
    let requests = Arc::new(Mutex::new(vec![]));
    let executor: Arc<dyn TaskExecutor<agentkit_store::SqliteStore>> = Arc::new(
        ChatExecutor::new(
            Arc::new(Capturing {
                inner: Answer,
                requests: Arc::clone(&requests),
            }),
            Arc::new(ToolRegistry::new()),
        )
        .with_options(ChatExecutorOptions {
            project_instructions: Some(Arc::new(FileProjectInstructions)),
            ..Default::default()
        }),
    );
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
        runtime: Arc::clone(&runtime),
        plan_approval: None,
        wake: Some(host.wake_handle()),
    });
    host.set_session_flush_planner(Arc::new(service.clone()))
        .await;
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
            workspace: Some(json!({ "root": dir.path().display().to_string(), "trusted": true })),
            context: None,
            skills: None,
            turn_kind: None,
            execution_mode: None,
            metadata: None,
        })
        .await
        .unwrap();
    assert!(
        prefix_of(&store).await.is_none(),
        "nothing is frozen before a turn"
    );

    let first = send(&service, "in1").await;
    assert_eq!(drive(&host, &store, &first).await, TaskStatus::Completed);
    let frozen = prefix_of(&store)
        .await
        .expect("the first turn records the prefix");
    assert_eq!(frozen.revision, 1.0);
    assert_eq!(frozen.frozen_at_task, first);
    assert_eq!(frozen.memory_core_digest, agentkit_kernel::sha256_hex(""));

    let second = send(&service, "in2").await;
    assert_eq!(drive(&host, &store, &second).await, TaskStatus::Completed);
    assert_eq!(
        prefix_of(&store).await.expect("kept"),
        frozen,
        "an unchanged head keeps the record, including the task that froze it"
    );

    std::fs::write(dir.path().join("CLAUDE.md"), b"rule two\n").expect("edit rules");
    let third = send(&service, "in3").await;
    assert_eq!(drive(&host, &store, &third).await, TaskStatus::Completed);
    let refrozen = prefix_of(&store).await.expect("refrozen");
    assert_eq!(refrozen.revision, 2.0);
    assert_eq!(refrozen.frozen_at_task, third);
    assert_ne!(refrozen.rules_digest, frozen.rules_digest);
    assert_eq!(refrozen.system_digest, frozen.system_digest);
    assert_ne!(refrozen.cache_key, frozen.cache_key);

    assert!(refreeze_warnings(&store, &first).await.is_empty());
    assert!(refreeze_warnings(&store, &second).await.is_empty());
    let report = refreeze_warnings(&store, &third).await;
    assert_eq!(report.len(), 1, "a re-freeze is a durable event");
    assert_eq!(report[0].event["data"]["changed"], json!(["rules"]));
    assert_eq!(report[0].event["data"]["revision"], 2);

    // CE-01: system segments are separate tagged blocks in segment order.
    let requests = requests.lock().unwrap();
    use agentkit_core::Segment::Rules;
    let blocks = leading_system(&requests[0]);
    let segments: Vec<_> = blocks.iter().map(|block| block.0).collect();
    assert!(
        segments.iter().all(Option::is_some),
        "every block is tagged"
    );
    assert!(
        segments.windows(2).all(|pair| pair[0] < pair[1]),
        "segment order"
    );
    let (last_segment, last_text) = blocks.last().expect("a rules block");
    assert_eq!(*last_segment, Some(Rules));
    assert!(last_text.contains("rule one"));
    assert_eq!(
        leading_system(&requests[0]),
        leading_system(&requests[1]),
        "the head is byte-identical across unchanged turns"
    );
    assert!(leading_system(&requests[2])[0].1.contains("rule two"));
}
