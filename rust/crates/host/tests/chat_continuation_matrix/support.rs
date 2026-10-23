use agentkit_core::{providers::*, *};
use agentkit_host::{AgentHost, ChatExecutor, HostOptions, SubmitChat, TaskExecutor};
use agentkit_store::{SqliteStore, TaskStatus};
use async_trait::async_trait;
use axum::{Json, Router, extract::State, routing::post};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

pub struct Fixture {
    pub path: PathBuf,
    pub url: String,
    pub requests: Arc<Mutex<Vec<Value>>>,
    pub tools: Arc<AtomicUsize>,
    server: tokio::task::JoinHandle<()>,
}
#[derive(Clone)]
struct ServerState {
    requests: Arc<Mutex<Vec<Value>>>,
    response: String,
}
async fn respond(State(state): State<ServerState>, Json(body): Json<Value>) -> String {
    state.requests.lock().unwrap().push(body);
    state.response
}
impl Fixture {
    pub async fn start(response: String) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/responses", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(vec![]));
        let app = Router::new()
            .route("/responses", post(respond))
            .with_state(ServerState {
                requests: requests.clone(),
                response,
            });
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self {
            path: std::env::temp_dir().join(format!(
                "agentkit-private-matrix-{}.sqlite",
                uuid::Uuid::new_v4()
            )),
            url,
            requests,
            tools: Arc::new(AtomicUsize::new(0)),
            server,
        }
    }
    pub fn open(&self) -> SqliteStore {
        SqliteStore::open(&self.path, 64).unwrap()
    }
    pub fn host(&self, store: SqliteStore, identity: ConnectionIdentity) -> AgentHost {
        let transport = Arc::new(Transport {
            url: self.url.clone(),
            identity: identity.clone(),
        });
        let provider: Arc<dyn ChatProvider> = if identity.protocol == "responses" {
            Arc::new(ResponsesProvider::new(transport).unwrap())
        } else {
            Arc::new(ChatCompletionsProvider::new(transport).unwrap())
        };
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(Write { count: self.tools.clone(), definition: serde_json::from_value(json!({"name":"write","version":"1","effect":"write","capability":"test","description":"Write","inputSchema":{"type":"object"}})).unwrap() })).unwrap();
        let executor: Arc<dyn TaskExecutor> =
            Arc::new(ChatExecutor::new(provider, Arc::new(registry)));
        AgentHost::new(
            store,
            HashMap::from([("chat.turn".into(), executor)]),
            Arc::new(agentkit_host::SystemRuntime),
            HostOptions::default(),
        )
        .unwrap()
    }
    pub fn request_count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }
    pub fn tool_count(&self) -> usize {
        self.tools.load(Ordering::SeqCst)
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
        let _ = std::fs::remove_file(&self.path);
    }
}
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
struct Write {
    definition: ToolDefinition,
    count: Arc<AtomicUsize>,
}
#[async_trait]
impl Tool for Write {
    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }
    async fn execute(&self, context: ToolContext, _: Value) -> Result<ToolResult, CoreError> {
        self.count.fetch_add(1, Ordering::SeqCst);
        Ok(ToolResult {
            ok: true,
            data: Some(json!({"applied":true})),
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
pub fn identity() -> ConnectionIdentity {
    ConnectionIdentity {
        provider_id: "provider".into(),
        protocol: "responses".into(),
        connection_id: "account".into(),
        generation: 1,
    }
}
pub async fn turn(host: &AgentHost, id: &str, model: &str) {
    host.submit_chat(SubmitChat {
        task_id: id.into(),
        chat_id: "chat".into(),
        model: model.into(),
        content: "question".into(),
        parent_message_id: None,
    })
    .await
    .unwrap();
    assert!(host.run_once().await.unwrap());
}
pub async fn assert_failed(store: &SqliteStore, id: &str, error: &str) {
    let task = store.get_task(id).await.unwrap().unwrap();
    assert_eq!(task.status, TaskStatus::Failed, "{task:?}");
    assert!(
        task.error.as_deref().unwrap_or_default().contains(error),
        "{task:?}"
    );
}
pub fn frame(event: Value) -> String {
    format!("data: {event}\n\n")
}
pub fn reasoning() -> Value {
    json!({"id":"reasoning","type":"reasoning","encrypted_content":"private-secret"})
}
pub fn function() -> Value {
    json!({"id":"function","type":"function_call","call_id":"call","namespace":"agentkit","name":"write","arguments":"{}","status":"completed"})
}
pub fn completed(output: Vec<Value>) -> String {
    frame(json!({"type":"response.completed","response":{"status":"completed","output":output}}))
}
pub fn success() -> String {
    completed(vec![
        reasoning(),
        json!({"id":"message","type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":"answer"}]}),
    ])
}
