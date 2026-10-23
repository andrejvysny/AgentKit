#![cfg(feature = "http")]
use agentkit_core::{providers::*, *};
use agentkit_host::{AgentHost, ChatExecutor, HostOptions, SubmitChat, TaskExecutor};
use agentkit_store::{SqliteStore, TaskStatus};
use async_trait::async_trait;
use axum::{Json, Router, extract::State, routing::post};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

struct Transport {
    url: String,
    identity: ConnectionIdentity,
}
#[async_trait]
impl TrustedTransport for Transport {
    fn identity(&self) -> ConnectionIdentity {
        self.identity.clone()
    }
    fn auth_kind(&self) -> AuthKind {
        AuthKind::ApiKey
    }
    async fn request(
        &self,
        identity: &ConnectionIdentity,
        _: &str,
        body: Option<Value>,
        _: CancellationToken,
    ) -> Result<reqwest::Response, CoreError> {
        assert_eq!(identity, &self.identity);
        reqwest::Client::new()
            .post(&self.url)
            .json(&body.unwrap())
            .send()
            .await
            .map_err(|_| CoreError::responses("transport_error"))
    }
}
async fn respond(
    State(requests): State<Arc<Mutex<Vec<Value>>>>,
    Json(body): Json<Value>,
) -> String {
    let mut requests = requests.lock().unwrap();
    requests.push(body);
    let index = requests.len();
    let terminal = json!({"type":"response.completed","response":{"status":"completed","output":[
        {"id":format!("reasoning{index}"),"type":"reasoning","encrypted_content":format!("private{index}")},
        {"id":format!("message{index}"),"type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":format!("answer{index}")}]}]}});
    format!("data: {terminal}\n\n")
}
fn host(store: SqliteStore, url: &str, generation: u64) -> AgentHost {
    let identity = ConnectionIdentity {
        provider_id: "provider".into(),
        protocol: "responses".into(),
        connection_id: "account".into(),
        generation,
    };
    let provider = ResponsesProvider::new(Arc::new(Transport {
        url: url.into(),
        identity,
    }))
    .unwrap();
    let executor: Arc<dyn TaskExecutor> = Arc::new(ChatExecutor::new(
        Arc::new(provider),
        Arc::new(ToolRegistry::new()),
    ));
    AgentHost::new(
        store,
        HashMap::from([("chat.turn".into(), executor)]),
        Arc::new(agentkit_host::SystemRuntime),
        HostOptions::default(),
    )
    .unwrap()
}
async fn turn(host: &AgentHost, id: &str, text: &str) {
    host.submit_chat(SubmitChat {
        task_id: id.into(),
        chat_id: "chat".into(),
        model: "model".into(),
        content: text.into(),
        parent_message_id: None,
    })
    .await
    .unwrap();
    host.run_once().await.unwrap();
}
#[tokio::test]
async fn private_continuation_survives_store_reopen_next_turn_and_rejects_account_generation_change()
 {
    let requests = Arc::new(Mutex::new(vec![]));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/responses", listener.local_addr().unwrap());
    let app = Router::new()
        .route("/responses", post(respond))
        .with_state(requests.clone());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let path =
        std::env::temp_dir().join(format!("agentkit-private-{}.sqlite", uuid::Uuid::new_v4()));
    {
        let store = SqliteStore::open(&path, 64).unwrap();
        store.create_chat("chat", None, json!({}), 0).await.unwrap();
        let host = host(store.clone(), &url, 1);
        turn(&host, "run1", "question").await;
        assert_eq!(
            store.get_task("run1").await.unwrap().unwrap().status,
            TaskStatus::Completed
        );
        assert!(store.get_continuation("run1").await.unwrap().is_some());
        let public = serde_json::to_string(&store.messages("chat", true).await.unwrap()).unwrap();
        assert!(!public.contains("private1"));
        assert!(!public.contains("encrypted_content"));
    }
    {
        let store = SqliteStore::open(&path, 64).unwrap();
        let second = host(store.clone(), &url, 1);
        turn(&second, "run2", "next").await;
        assert_eq!(
            store.get_task("run2").await.unwrap().unwrap().status,
            TaskStatus::Completed
        );
        assert_eq!(requests.lock().unwrap().len(), 2);
        assert!(
            requests.lock().unwrap()[1]["input"]
                .as_array()
                .unwrap()
                .iter()
                .any(|item| item["encrypted_content"] == "private1")
        );
        drop(second);
        let changed = host(store.clone(), &url, 2);
        turn(&changed, "run3", "another").await;
        let task = store.get_task("run3").await.unwrap().unwrap();
        assert_eq!(task.status, TaskStatus::Failed);
        assert!(
            task.error
                .unwrap_or_default()
                .contains("provider_continuation_scope_mismatch")
        );
        assert_eq!(requests.lock().unwrap().len(), 2);
    }
    server.abort();
    let _ = std::fs::remove_file(path);
}

async fn respond_tools(
    State(requests): State<Arc<Mutex<Vec<Value>>>>,
    Json(body): Json<Value>,
) -> String {
    let mut requests = requests.lock().unwrap();
    requests.push(body);
    let index = requests.len();
    let item = if index % 2 == 1 {
        json!({"id":format!("function{index}"),"type":"function_call","call_id":format!("call{index}"),"namespace":"agentkit","name":"echo","arguments":"{}","status":"completed"})
    } else {
        json!({"id":format!("message{index}"),"type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":format!("answer{index}")}]})
    };
    let terminal = json!({"type":"response.completed","response":{"status":"completed","output":[{"id":format!("reasoning{index}"),"type":"reasoning","encrypted_content":format!("private{index}")},item]}});
    format!("data: {terminal}\n\n")
}
struct Echo {
    definition: ToolDefinition,
}
#[async_trait]
impl Tool for Echo {
    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }
    async fn execute(&self, context: ToolContext, _: Value) -> Result<ToolResult, CoreError> {
        Ok(ToolResult {
            ok: true,
            data: Some(json!({"ok":true})),
            sources: vec![],
            warnings: vec![],
            truncated: false,
            limits: context.limits,
            model_data: None,
            summary: None,
            status: None,
        })
    }
}
struct VerifyOnce(std::sync::atomic::AtomicUsize);
#[async_trait]
impl agentkit_host::VerificationHook for VerifyOnce {
    async fn verify(
        &self,
        _: agentkit_host::ExecutionContext,
    ) -> agentkit_host::HostResult<agentkit_host::VerificationReport> {
        let first = self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0;
        Ok(agentkit_host::VerificationReport {
            status: if first {
                "partial".into()
            } else {
                "pass".into()
            },
            deficiencies: if first {
                vec!["fix domain".into()]
            } else {
                vec![]
            },
        })
    }
}
#[tokio::test]
async fn responses_correction_preserves_canonical_private_history_for_next_run() {
    let requests = Arc::new(Mutex::new(vec![]));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/responses", listener.local_addr().unwrap());
    let app = Router::new()
        .route("/responses", post(respond_tools))
        .with_state(requests.clone());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let store = SqliteStore::memory(64).unwrap();
    store.create_chat("chat", None, json!({}), 0).await.unwrap();
    let identity = ConnectionIdentity {
        provider_id: "provider".into(),
        protocol: "responses".into(),
        connection_id: "account".into(),
        generation: 1,
    };
    let provider = ResponsesProvider::new(Arc::new(Transport { url, identity })).unwrap();
    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(Echo {definition:serde_json::from_value(json!({"name":"echo","version":"1","effect":"read","capability":"test","description":"Echo","inputSchema":{"type":"object"}})).unwrap()})).unwrap();
    let options = agentkit_host::ChatExecutorOptions {
        verification: Some(Arc::new(VerifyOnce(std::sync::atomic::AtomicUsize::new(0)))),
        correction: Some(agentkit_host::CorrectionConfig::default()),
        ..Default::default()
    };
    let executor: Arc<dyn TaskExecutor> =
        Arc::new(ChatExecutor::new(Arc::new(provider), Arc::new(registry)).with_options(options));
    let host = AgentHost::new(
        store.clone(),
        HashMap::from([("chat.turn".into(), executor)]),
        Arc::new(agentkit_host::SystemRuntime),
        HostOptions::default(),
    )
    .unwrap();
    turn(&host, "corrected", "question").await;
    let status = store.get_task("corrected").await.unwrap().unwrap();
    let errors = store
        .events("corrected", None, 1000)
        .await
        .unwrap()
        .into_iter()
        .filter(|event| event.event["type"] == "run.failed")
        .map(|event| event.event)
        .collect::<Vec<_>>();
    assert_eq!(
        status.status,
        TaskStatus::Completed,
        "{:?} {errors:?}",
        status.error
    );
    assert_eq!(requests.lock().unwrap().len(), 4);
    assert!(
        requests.lock().unwrap()[2]["input"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["encrypted_content"] == "private2")
    );
    assert!(
        requests.lock().unwrap()[2]["input"]
            .to_string()
            .contains("fix domain")
    );
    assert_eq!(
        store
            .get_message("corrected_assistant")
            .await
            .unwrap()
            .unwrap()
            .content,
        "answer4"
    );
    turn(&host, "next-run", "next").await;
    assert_eq!(
        store.get_task("next-run").await.unwrap().unwrap().status,
        TaskStatus::Completed
    );
    assert_eq!(requests.lock().unwrap().len(), 6);
    assert!(
        requests.lock().unwrap()[4]["input"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["encrypted_content"] == "private4")
    );
    server.abort();
}
