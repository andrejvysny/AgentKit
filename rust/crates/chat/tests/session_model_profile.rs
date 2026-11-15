//! The model tool profile (T2.1, TU-15) on the live session path: the profile
//! frozen with the session is recorded in the prefix on its first turn, and
//! neither a changed built-in profile nor changed settings reach the running
//! session.
#![cfg(unix)]

use std::{collections::HashMap, sync::Arc};

use agentkit_chat::*;
use agentkit_core::{
    ChatProvider, ChatRequest, ProviderEvent, ProviderStream, QWEN_PROFILE_ID, ToolRegistry,
    builtin_profile, model_profile_digest,
};
use agentkit_engine::{SharedRuntime, SystemRuntime, TaskExecutor};
use agentkit_sessions::*;
use agentkit_store::{SqliteStore, TaskStatus};
use async_trait::async_trait;
use serde_json::json;
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

struct Harness {
    host: AgentHost,
    store: SqliteStore,
    service: AgentSessionService<SqliteStore>,
}

fn harness() -> Harness {
    let store = SqliteStore::memory(64).expect("store");
    let executor: Arc<dyn TaskExecutor<agentkit_store::SqliteStore>> = Arc::new(ChatExecutor::new(
        Arc::new(Answer),
        Arc::new(ToolRegistry::new()),
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
    Harness {
        host,
        store,
        service,
    }
}

async fn create(h: &Harness, id: &str, metadata: Option<serde_json::Value>) {
    h.host
        .set_session_flush_planner(Arc::new(h.service.clone()))
        .await;
    h.service
        .create_session(&CreateSessionInput {
            session_id: Some(id.into()),
            profile_id: "coding".into(),
            model: Some("model".into()),
            metadata,
            ..Default::default()
        })
        .await
        .unwrap();
}

async fn turn(h: &Harness, session: &str, input_id: &str) -> String {
    let sent = h
        .service
        .send(
            session,
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
        h.host.run_once().await.expect("run");
        let task = h.store.get_task(&turn_id).await.unwrap().unwrap();
        if task.status.is_terminal() {
            assert_eq!(task.status, TaskStatus::Completed);
            return turn_id;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("turn did not settle");
}

async fn prefix_of(h: &Harness, session: &str) -> Option<agentkit_contracts::AiSessionPrefix> {
    let chat = h.store.get_chat(session).await.unwrap().unwrap();
    read_session_prefix(&chat.metadata)
}

async fn refreezes(h: &Harness, turn_id: &str) -> usize {
    h.store
        .events(turn_id, None, 1000)
        .await
        .unwrap()
        .into_iter()
        .filter(|event| event.event["data"]["code"] == "prefix_refrozen")
        .count()
}

#[tokio::test]
async fn the_frozen_profile_is_recorded_and_survives_a_changed_profile() {
    let h = harness();
    let profile = builtin_profile(QWEN_PROFILE_ID).unwrap();
    let metadata = with_model_profile(&json!({}), &profile);
    create(&h, "s1", Some(metadata)).await;
    assert!(prefix_of(&h, "s1").await.is_none());

    let first = turn(&h, "s1", "in1").await;
    let frozen = prefix_of(&h, "s1")
        .await
        .expect("recorded on the first turn");
    assert_eq!(
        frozen.model_profile_id,
        agentkit_contracts::Optional::Present("qwen3.8-27b".into())
    );
    assert_eq!(
        frozen.dialect_version,
        agentkit_contracts::Optional::Present("1".into())
    );
    assert_eq!(
        frozen.model_profile_digest,
        agentkit_contracts::Optional::Present(model_profile_digest(&profile))
    );

    // A later resolution yields a different profile; the session keeps its own.
    let mut newer = profile.clone();
    newer.limits.context_window = 65_536.0;
    assert_ne!(model_profile_digest(&newer), model_profile_digest(&profile));
    let second = turn(&h, "s1", "in2").await;
    let chat = h.store.get_chat("s1").await.unwrap().unwrap();
    assert_eq!(read_model_profile(&chat.metadata), Some(profile));
    assert_eq!(prefix_of(&h, "s1").await.expect("kept"), frozen);
    assert_eq!(
        refreezes(&h, &first).await + refreezes(&h, &second).await,
        0
    );
}

#[tokio::test]
async fn a_session_without_a_profile_records_none() {
    let h = harness();
    create(&h, "s2", None).await;
    let _ = turn(&h, "s2", "in1").await;
    let prefix = prefix_of(&h, "s2").await.expect("recorded");
    assert!(prefix.model_profile_id.is_absent());
    assert!(prefix.dialect_version.is_absent());
    assert!(prefix.model_profile_digest.is_absent());
}
