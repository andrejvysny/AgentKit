//! Native parity witnesses use the real store, admission service and executor.
use agentkit_chat::*;
use agentkit_core::{
    ChatProvider, ChatRequest, CoreError, ProviderEvent, ProviderStream, ReasoningEffort, Tool,
    ToolContext, ToolDefinition, ToolRegistry, ToolResult,
};
use agentkit_engine::{SystemRuntime, TaskExecutor};

use agentkit_sessions::*;
use agentkit_store::{MessageInput, SqliteStore, TaskStatus};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};
use tokio_util::sync::CancellationToken;

struct Provider(Arc<Mutex<Vec<ChatRequest>>>);
#[async_trait]
impl ChatProvider for Provider {
    fn id(&self) -> &str {
        "provider"
    }
    fn stream_chat(&self, request: ChatRequest, _: CancellationToken) -> ProviderStream {
        self.0.lock().unwrap().push(request);
        Box::pin(futures::stream::iter([Ok(ProviderEvent::Completed {
            content: "answer".into(),
            tool_calls: vec![],
            finish_reason: "stop".into(),
        })]))
    }
}
struct Authority(AtomicBool);
#[async_trait]
impl ContentAuthority for Authority {
    async fn authorized_connectors(
        &self,
        _: &str,
        _: CancellationToken,
    ) -> agentkit_sessions::SessionsResult<Vec<String>> {
        Ok(if self.0.load(Ordering::SeqCst) {
            vec!["github".into()]
        } else {
            vec![]
        })
    }
}
struct StubTool(ToolDefinition);
#[async_trait]
impl Tool for StubTool {
    fn definition(&self) -> &ToolDefinition {
        &self.0
    }
    async fn execute(&self, _: ToolContext, _: Value) -> Result<ToolResult, CoreError> {
        unreachable!()
    }
}
fn tools() -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    for name in ["allowed", "denied"] {
        registry.register(Arc::new(StubTool(serde_json::from_value(json!({"name":name,"version":"1","effect":"read","capability":"read","description":"test","inputSchema":{"type":"object"}})).unwrap()))).unwrap();
    }
    registry
}
fn host(
    store: &SqliteStore,
    requests: Arc<Mutex<Vec<ChatRequest>>>,
    authority: Arc<dyn ContentAuthority>,
) -> AgentHost {
    let executor: Arc<dyn TaskExecutor<agentkit_store::SqliteStore>> = Arc::new(
        ChatExecutor::new(Arc::new(Provider(requests)), Arc::new(tools())).with_options(
            ChatExecutorOptions {
                content_authority: Some(authority),
                max_output_tokens: Some(5),
                ..Default::default()
            },
        ),
    );
    AgentHost::new(
        store.clone(),
        HashMap::from([("chat.turn".into(), executor)]),
        Arc::new(SystemRuntime),
        HostOptions::default(),
    )
    .unwrap()
}
fn service(
    store: &SqliteStore,
    authority: Arc<dyn ContentAuthority>,
    overrides: &[Value],
) -> AgentSessionService {
    AgentSessionService::new(AgentSessionServiceDeps {
        store: store.clone(),
        runtime: Arc::new(SystemRuntime),
        plan_approval: None,
        wake: None,
    })
    .with_profiles(Arc::new(ProfileRegistry::new(overrides).unwrap()))
    .with_content_authority(authority)
}
async fn send(service: &AgentSessionService, id: &str) -> String {
    match service
        .send(
            "chat",
            &SendInput {
                content: json!("question"),
                input_id: Some(id.into()),
                when_busy: WhenBusy::Reject,
                turn_id: None,
                on_settled: None,
            },
        )
        .await
        .unwrap()
    {
        AgentSendResult::Turn { turn_id, .. } => turn_id,
        other => panic!("expected turn: {other:?}"),
    }
}
async fn create(service: &AgentSessionService, skills: Option<Vec<Value>>) {
    service
        .create_session(&CreateSessionInput {
            session_id: Some("chat".into()),
            profile_id: "coding".into(),
            context: Some(json!({"toolResultElision":false})),
            skills,
            ..Default::default()
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn content_revision_profile_defaults_and_reasoning_survive_later_edits() {
    let store = SqliteStore::memory(32).unwrap();
    let authority = Arc::new(Authority(AtomicBool::new(true)));
    let requests = Arc::new(Mutex::new(vec![]));
    let profile = json!({"id":"coding","model":"profile-model","reasoningEffort":"high","tools":["allowed"],"limits":{"maxOutputTokens":17}});
    let service = service(&store, authority.clone(), &[profile]);
    let original = "---\nname: guide\ndescription: guide\nmetadata:\n  requires-connectors: github\n---\noriginal instruction\n";
    let skill = store
        .publish_skill("docs".into(), "guide".into(), original.into())
        .await
        .unwrap();
    create(
        &service,
        Some(vec![
            json!({"id":"docs:guide","revision":skill["revision"]}),
        ]),
    )
    .await;
    let run = send(&service, "first").await;
    let payload = store.get_task(&run).await.unwrap().unwrap().payload;
    assert_eq!(payload["model"], "profile-model");
    assert_eq!(payload["reasoningEffort"], "high");
    assert!(payload["contentSnapshot"]["digest"].is_string());
    store
        .publish_skill(
            "docs".into(),
            "guide".into(),
            original.replace("original", "changed"),
        )
        .await
        .unwrap();
    let host = host(&store, requests.clone(), authority);
    host.run_once().await.unwrap();
    assert_eq!(
        store.get_task(&run).await.unwrap().unwrap().status,
        TaskStatus::Completed
    );
    let requests = requests.lock().unwrap();
    let request = &requests[0];
    assert_eq!(request.model, "profile-model");
    assert_eq!(request.reasoning_effort, Some(ReasoningEffort::High));
    assert_eq!(request.max_output_tokens, Some(5));
    assert_eq!(
        request
            .tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>(),
        ["allowed"]
    );
    assert!(request.selected_content.iter().any(|message| {
        message
            .content
            .as_str()
            .is_some_and(|text| text.contains("original instruction"))
    }));
    assert!(!request.messages.iter().any(|message| {
        message
            .content
            .as_str()
            .is_some_and(|text| text.contains("original instruction"))
    }));
}

#[tokio::test]
async fn revoked_connector_grant_blocks_frozen_content_before_provider_dispatch() {
    let store = SqliteStore::memory(32).unwrap();
    let authority = Arc::new(Authority(AtomicBool::new(true)));
    let requests = Arc::new(Mutex::new(vec![]));
    let service = service(
        &store,
        authority.clone(),
        &[json!({"id":"coding","model":"model"})],
    );
    let skill = store.publish_skill("docs".into(), "guide".into(), "---\nname: guide\ndescription: guide\nmetadata:\n  requires-connectors: github\n---\nsecret context\n".into()).await.unwrap();
    create(
        &service,
        Some(vec![
            json!({"id":"docs:guide","revision":skill["revision"]}),
        ]),
    )
    .await;
    let run = send(&service, "revoked").await;
    authority.0.store(false, Ordering::SeqCst);
    host(&store, requests.clone(), authority)
        .run_once()
        .await
        .unwrap();
    assert!(requests.lock().unwrap().is_empty());
    let task = store.get_task(&run).await.unwrap().unwrap();
    assert_eq!(task.status, TaskStatus::Failed);
    assert!(
        task.error
            .unwrap()
            .to_string()
            .contains("content_authority_revoked")
    );
}

#[tokio::test]
async fn full_history_survives_two_hundred_records_and_explicit_caps_refuse() {
    for cap in [None, Some(100)] {
        let store = SqliteStore::memory(32).unwrap();
        let authority = Arc::new(Authority(AtomicBool::new(false)));
        let requests = Arc::new(Mutex::new(vec![]));
        let profile = cap.map_or(
            json!({"id":"coding","model":"model"}),
            |limit| json!({"id":"coding","model":"model","limits":{"historyLimit":limit}}),
        );
        let service = service(&store, authority.clone(), &[profile]);
        create(&service, None).await;
        let mut parent = None;
        for index in 0..240 {
            let id = format!("old-{index}");
            store
                .append_message(
                    MessageInput {
                        id: Some(id.clone()),
                        chat_id: "chat".into(),
                        role: if index % 2 == 0 {
                            "user".into()
                        } else {
                            "assistant".into()
                        },
                        content: json!(format!("old {index}")),
                        parent_message_id: parent,
                        ..Default::default()
                    },
                    None,
                    index,
                )
                .await
                .unwrap();
            parent = Some(id);
        }
        let run = send(&service, "history").await;
        host(&store, requests.clone(), authority)
            .run_once()
            .await
            .unwrap();
        let task = store.get_task(&run).await.unwrap().unwrap();
        if cap.is_some() {
            assert_eq!(task.status, TaskStatus::Failed);
            assert!(task.error.unwrap().to_string().contains("context_overflow"));
            assert!(requests.lock().unwrap().is_empty());
        } else {
            assert_eq!(task.status, TaskStatus::Completed);
            let requests = requests.lock().unwrap();
            assert!(
                requests[0]
                    .messages
                    .iter()
                    .any(|message| message.content == json!("old 0"))
            );
            assert!(requests[0].messages.len() > 240);
        }
    }
}

#[tokio::test]
async fn unknown_profiles_and_digest_changes_fail_before_creating_rows() {
    let store = SqliteStore::memory(8).unwrap();
    let service = service(&store, Arc::new(Authority(AtomicBool::new(false))), &[]);
    for (profile_id, profile_digest) in [("missing", None), ("coding", Some("0".repeat(64)))] {
        assert!(
            service
                .create_session(&CreateSessionInput {
                    profile_id: profile_id.into(),
                    profile_digest,
                    ..Default::default()
                })
                .await
                .is_err()
        );
        assert!(store.list_chats(true).await.unwrap().is_empty());
    }
}

struct ScopedAuthority {
    grants: Mutex<Vec<String>>,
    generation: std::sync::atomic::AtomicU64,
}
#[async_trait]
impl ContentAuthority for ScopedAuthority {
    async fn authorized_connectors(
        &self,
        _: &str,
        _: CancellationToken,
    ) -> agentkit_sessions::SessionsResult<Vec<String>> {
        Ok(self.grants.lock().unwrap().clone())
    }
    async fn authority_identity(
        &self,
        _: &str,
        required: &[String],
        _: CancellationToken,
    ) -> agentkit_sessions::SessionsResult<Value> {
        Ok(json!({"required":required,"generation":self.generation.load(Ordering::SeqCst)}))
    }
}
#[tokio::test]
async fn only_consumed_connector_grants_are_frozen_and_replacement_identity_is_refused() {
    let store = SqliteStore::memory(32).unwrap();
    let authority = Arc::new(ScopedAuthority {
        grants: Mutex::new(vec!["github".into(), "unrelated".into()]),
        generation: std::sync::atomic::AtomicU64::new(1),
    });
    let requests = Arc::new(Mutex::new(vec![]));
    let service = service(
        &store,
        authority.clone(),
        &[json!({"id":"coding","model":"model"})],
    );
    let skill = store.publish_skill("docs".into(), "guide".into(), "---\nname: guide\ndescription: guide\nmetadata:\n  requires-connectors: github\n---\ninstruction\n".into()).await.unwrap();
    create(
        &service,
        Some(vec![
            json!({"id":"docs:guide","revision":skill["revision"]}),
        ]),
    )
    .await;
    let first = send(&service, "one").await;
    let snapshot =
        store.get_task(&first).await.unwrap().unwrap().payload["contentSnapshot"].clone();
    assert_eq!(snapshot["authorizedConnectors"], json!(["github"]));
    assert!(validate_frozen_run_content(&snapshot).is_ok());
    let mut altered = snapshot.clone();
    altered["messages"][0]["content"] = "tampered".into();
    assert!(validate_frozen_run_content(&altered).is_err());
    *authority.grants.lock().unwrap() = vec!["github".into()];
    let host = host(&store, requests.clone(), authority.clone());
    host.run_once().await.unwrap();
    assert_eq!(
        store.get_task(&first).await.unwrap().unwrap().status,
        TaskStatus::Completed
    );
    let second = send(&service, "two").await;
    authority.generation.store(2, Ordering::SeqCst);
    host.run_once().await.unwrap();
    assert_eq!(requests.lock().unwrap().len(), 1);
    assert_eq!(
        store.get_task(&second).await.unwrap().unwrap().status,
        TaskStatus::Failed
    );
}

struct AwaitUsage {
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}
#[async_trait]
impl UsageAuthorizer for AwaitUsage {
    async fn authorize(&self, _: &str, _: &Value) -> agentkit_chat::ChatResult<()> {
        self.entered.notify_one();
        self.release.notified().await;
        Ok(())
    }
}
#[tokio::test]
async fn connector_revocation_during_usage_authorization_prevents_final_dispatch() {
    let store = SqliteStore::memory(32).unwrap();
    let authority = Arc::new(ScopedAuthority {
        grants: Mutex::new(vec!["github".into()]),
        generation: std::sync::atomic::AtomicU64::new(1),
    });
    let requests = Arc::new(Mutex::new(vec![]));
    let service = service(
        &store,
        authority.clone(),
        &[json!({"id":"coding","model":"model"})],
    );
    let skill=store.publish_skill("docs".into(),"guide".into(),"---\nname: guide\ndescription: guide\nmetadata:\n  requires-connectors: github\n---\ninstruction\n".into()).await.unwrap();
    create(
        &service,
        Some(vec![
            json!({"id":"docs:guide","revision":skill["revision"]}),
        ]),
    )
    .await;
    let run = send(&service, "during-usage").await;
    let usage = Arc::new(AwaitUsage {
        entered: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    });
    let executor: Arc<dyn TaskExecutor<agentkit_store::SqliteStore>> = Arc::new(
        ChatExecutor::new(Arc::new(Provider(requests.clone())), Arc::new(tools())).with_options(
            ChatExecutorOptions {
                content_authority: Some(authority.clone()),
                usage: Some(usage.clone()),
                ..Default::default()
            },
        ),
    );
    let host = AgentHost::new(
        store.clone(),
        HashMap::from([("chat.turn".into(), executor)]),
        Arc::new(SystemRuntime),
        HostOptions::default(),
    )
    .unwrap();
    let attempt = tokio::spawn(async move { host.run_once().await });
    tokio::time::timeout(std::time::Duration::from_secs(2), usage.entered.notified())
        .await
        .unwrap();
    authority.grants.lock().unwrap().clear();
    usage.release.notify_one();
    attempt.await.unwrap().unwrap();
    assert!(requests.lock().unwrap().is_empty());
    assert_eq!(
        store.get_task(&run).await.unwrap().unwrap().status,
        TaskStatus::Failed
    );
}

#[tokio::test]
async fn raw_chat_submission_cannot_bypass_a_session_profile() {
    let store = SqliteStore::memory(16).unwrap();
    let authority = Arc::new(Authority(AtomicBool::new(false)));
    let service = service(&store, authority.clone(), &[]);
    create(&service, None).await;
    let host = host(&store, Arc::new(Mutex::new(vec![])), authority);
    let error = host
        .submit_chat(SubmitChat {
            task_id: "raw".into(),
            chat_id: "chat".into(),
            model: "model".into(),
            content: json!("question"),
            parent_message_id: None,
        })
        .await
        .err()
        .expect("session admission required");
    assert!(error.to_string().contains("session_submission_required"));
    assert!(
        store
            .list_tasks(Some("chat".into()))
            .await
            .unwrap()
            .is_empty()
    );
    assert!(store.messages("chat", true).await.unwrap().is_empty());
}
