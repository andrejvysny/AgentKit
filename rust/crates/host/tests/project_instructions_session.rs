//! Project instructions on the live session path: a workspace whose
//! `AGENTS.md` is a symlink must be skipped WITH a durable
//! `run.warning { code: "project_instructions_unavailable" }` on the turn,
//! the secret behind the symlink must never reach the provider, and the
//! readable `CLAUDE.md` must still be injected (ADR 0017 part 1).
//!
//! The port-level matrix lives in `project_instructions.rs`; this file pins
//! the executor wiring: the warning is emitted through the run's durable
//! event log, so `subscribe`/replay delivers it like any other event.
#![cfg(unix)]

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use agentkit_core::{ChatProvider, ChatRequest, ProviderEvent, ProviderStream, ToolRegistry};
use agentkit_host::*;
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
    std::fs::write(dir.path().join("CLAUDE.md"), b"rule two\n").expect("claude");
    std::fs::write(dir.path().join("secret.md"), b"do-not-leak\n").expect("secret");
    std::os::unix::fs::symlink(dir.path().join("secret.md"), dir.path().join("AGENTS.md"))
        .expect("symlink");
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

#[tokio::test]
async fn a_symlinked_agents_md_skips_with_a_durable_warning_and_never_leaks() {
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
    let sent = service
        .send(
            "s1",
            &SendInput {
                content: json!("hi"),
                input_id: Some("in1".into()),
                when_busy: WhenBusy::Reject,
                turn_id: None,
                on_settled: None,
            },
        )
        .await
        .unwrap();
    let turn_id = match sent {
        AgentSendResult::Turn { turn_id, .. } => turn_id,
        AgentSendResult::Queued { .. } => panic!("an idle session starts the turn"),
    };
    assert_eq!(
        drive(&host, &store, &turn_id).await,
        TaskStatus::Completed,
        "the turn must complete despite the unreadable file"
    );

    let events = store.events(&turn_id, None, 1000).await.expect("events");
    let warning = events
        .iter()
        .find(|event| {
            event.event["type"] == "run.warning"
                && event.event["data"]["code"] == "project_instructions_unavailable"
        })
        .unwrap_or_else(|| {
            panic!(
                "no project-instructions warning in the durable log: {:?}",
                events
                    .iter()
                    .map(|e| e.event["type"].clone())
                    .collect::<Vec<_>>()
            )
        });
    assert!(
        warning.event["data"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("AGENTS.md")),
        "the warning names the skipped file: {warning:#?}"
    );

    let requests = requests.lock().unwrap();
    let system = &requests[0].messages[0];
    assert_eq!(system.role, "system");
    let text = system.content.as_str().expect("system text");
    assert!(
        text.contains("rule two"),
        "the readable file is still injected"
    );
    assert!(
        !text.contains("do-not-leak"),
        "the symlink target never leaks"
    );
    assert!(
        !text.contains("AGENTS.md"),
        "the skipped file leaves no block"
    );
}
