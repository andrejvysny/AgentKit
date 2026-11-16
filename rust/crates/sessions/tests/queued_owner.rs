//! A queued owner `send` keeps its owner provenance: the durable envelope
//! carries it and every promotion path stamps the promoted turn's user
//! message, so memory capture treats it as the owner's words. Follow-ups
//! and rows written before the marker existed stay unstamped.

use agentkit_engine::{SharedRuntime, SystemRuntime, TaskExecutor};
use agentkit_sessions::{
    AgentSendResult, AgentSessionService, AgentSessionServiceDeps, CreateSessionInput,
    FollowUpInput, INPUT_ORIGIN_METADATA_KEY, INPUT_ORIGIN_OWNER, SendInput, SessionSettle,
    WhenBusy,
};
use agentkit_store::SqliteStore;
use async_trait::async_trait;
use serde_json::{Value, json};
use std::sync::Arc;

type AgentHost = agentkit_engine::AgentHost<SqliteStore>;
type ExecutionContext = agentkit_engine::ExecutionContext<SqliteStore>;
type HostOptions = agentkit_engine::HostOptions<SqliteStore>;
type Service = AgentSessionService<SqliteStore>;

struct Done;

#[async_trait]
impl TaskExecutor<SqliteStore> for Done {
    async fn execute(
        &self,
        _context: ExecutionContext,
    ) -> agentkit_engine::EngineResult<(), agentkit_store::StoreError> {
        Ok(())
    }
}

/// Plans the settlement like the service, but names no candidate, so the
/// store stages whatever it finds pending from the durable rows alone.
struct MissedSnapshot(Service);

#[async_trait]
impl agentkit_engine::SessionFlushPlanner<SqliteStore> for MissedSnapshot {
    async fn settle_plan(
        &self,
        settling: &agentkit_store::TaskRecord,
        completed: bool,
    ) -> agentkit_engine::EngineResult<SessionSettle, agentkit_store::StoreError> {
        let mut plan = self.0.settle_plan(settling, completed).await?;
        plan.convert.clear();
        Ok(plan)
    }
}

fn send(content: &str, input_id: &str, when_busy: WhenBusy) -> SendInput {
    SendInput {
        frozen_payload: None,
        content: json!(content),
        input_id: Some(input_id.into()),
        when_busy,
        turn_id: None,
        on_settled: None,
    }
}

async fn session() -> (SqliteStore, Service, AgentHost) {
    let store = SqliteStore::memory(64).unwrap();
    let runtime = Arc::new(SystemRuntime) as SharedRuntime;
    let service = AgentSessionService::new(AgentSessionServiceDeps {
        store: store.clone(),
        runtime: runtime.clone(),
        plan_approval: None,
        wake: None,
    });
    service
        .create_session(&CreateSessionInput {
            session_id: Some("s1".into()),
            profile_id: "coding".into(),
            model: Some("model".into()),
            ..CreateSessionInput::default()
        })
        .await
        .unwrap();
    let host = AgentHost::new(
        store.clone(),
        std::collections::HashMap::from([(
            "chat.turn".to_owned(),
            Arc::new(Done) as Arc<dyn TaskExecutor<SqliteStore>>,
        )]),
        runtime,
        HostOptions::default(),
    )
    .unwrap();
    (store, service, host)
}

async fn user_metadata(store: &SqliteStore, message_id: &str) -> Value {
    store
        .messages("s1", true)
        .await
        .unwrap()
        .into_iter()
        .find(|message| message.id == message_id)
        .unwrap_or_else(|| panic!("message {message_id} was not staged"))
        .metadata
}

async fn queue_behind_a_live_turn(service: &Service) {
    let first = service
        .send("s1", &send("first", "in-0", WhenBusy::Reject))
        .await
        .unwrap();
    assert!(matches!(first, AgentSendResult::Turn { .. }));
    let queued = service
        .send("s1", &send("owner words", "in-1", WhenBusy::Queue))
        .await
        .unwrap();
    assert!(matches!(queued, AgentSendResult::Queued { .. }));
    let follow_up = service
        .follow_up(
            "s1",
            &FollowUpInput {
                frozen_payload: None,
                content: json!("agent words"),
                input_id: Some("in-2".into()),
            },
        )
        .await
        .unwrap();
    assert!(matches!(follow_up, AgentSendResult::Queued { .. }));
}

fn owner() -> Value {
    json!({ INPUT_ORIGIN_METADATA_KEY: INPUT_ORIGIN_OWNER })
}

#[tokio::test]
async fn a_queued_owner_send_is_stamped_when_the_settlement_promotes_it() {
    let (store, service, host) = session().await;
    host.set_session_flush_planner(Arc::new(service.clone()))
        .await;
    queue_behind_a_live_turn(&service).await;
    let input = store.get_input("in-1".into()).await.unwrap().unwrap();
    assert_eq!(input.content["owner"], json!(true));
    let follow_up = store.get_input("in-2".into()).await.unwrap().unwrap();
    assert!(follow_up.content.get("owner").is_none());

    assert!(host.run_once().await.unwrap());
    assert_eq!(user_metadata(&store, "msg_q:in-1").await, owner());
    // The follow-up behind it is promoted next and is never the owner's.
    assert!(host.run_once().await.unwrap());
    assert_ne!(user_metadata(&store, "msg_q:in-2").await, owner());
}

#[tokio::test]
async fn a_queued_owner_send_is_stamped_when_the_store_stages_it_itself() {
    let (store, service, host) = session().await;
    host.set_session_flush_planner(Arc::new(MissedSnapshot(service.clone())))
        .await;
    queue_behind_a_live_turn(&service).await;
    assert!(host.run_once().await.unwrap());
    assert_eq!(user_metadata(&store, "msg_q:in-1").await, owner());
    // A row without the marker (a follow-up, or one written before the
    // marker existed) is staged unstamped.
    assert!(host.run_once().await.unwrap());
    assert_ne!(user_metadata(&store, "msg_q:in-2").await, owner());
}

#[tokio::test]
async fn a_queued_owner_send_is_stamped_by_an_explicit_flush() {
    let (store, service, host) = session().await;
    queue_behind_a_live_turn(&service).await;
    // No flush planner: the first turn settles and leaves the queue alone.
    assert!(host.run_once().await.unwrap());
    let started = service.flush_queue("s1").await.unwrap();
    assert!(started.is_some());
    assert_eq!(user_metadata(&store, "msg_q:in-1").await, owner());
}
