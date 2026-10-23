#![cfg(feature = "http")]
use agentkit_connections::{
    AppId, Capability, Connection, ConnectionConfig, ConnectionId, ConnectionRepository, Model,
};
use agentkit_core::{ChatProvider, ChatRequest, ProviderEvent, ProviderStream, ToolRegistry};
use agentkit_host::{
    AgentHost, ChatExecutor, ChatProviderFactory, HostError, HostOptions, HostResult,
    SystemRuntime, TaskExecutor, http::DesktopBackend,
};
use agentkit_store::{SqliteStore, TaskStatus};
use agentkit_transport_http::{Actor, Config};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    sync::{Arc, Mutex},
};
use tokio_util::sync::CancellationToken;
const TOKEN: &str = "12345678901234567890123456789012";
struct Provider;
impl ChatProvider for Provider {
    fn id(&self) -> &str {
        "test"
    }
    fn stream_chat(&self, _: ChatRequest, _: CancellationToken) -> ProviderStream {
        Box::pin(futures::stream::iter([Ok(ProviderEvent::Completed {
            content: "answer".into(),
            tool_calls: vec![],
            finish_reason: "stop".into(),
        })]))
    }
}
struct Factory {
    connections: agentkit_connections::AppConnections,
    observed: Arc<Mutex<Vec<Value>>>,
}
#[async_trait]
impl ChatProviderFactory for Factory {
    async fn resolve(
        &self,
        snapshot: &Value,
        _: CancellationToken,
    ) -> HostResult<Arc<dyn ChatProvider>> {
        self.observed.lock().unwrap().push(snapshot.clone());
        let frozen: Connection = serde_json::from_value(snapshot["connection"].clone()).unwrap();
        let current = self.connections.get(&frozen.id).unwrap();
        if current != frozen {
            return Err(HostError::Execution("connection_changed".into()));
        }
        Ok(Arc::new(Provider))
    }
}
fn configuration() -> ConnectionConfig {
    ConnectionConfig {
        label: "Provider".into(),
        provider_kind: "openai-compatible".into(),
        endpoint: "http://127.0.0.1:9999".parse().unwrap(),
        account: Some("account-one".into()),
        credential_required: false,
        enabled: true,
        capabilities: BTreeSet::from([Capability::Chat]),
        models: vec![Model {
            id: "model".into(),
            display_name: None,
            capabilities: BTreeSet::from([Capability::Chat]),
            context_window_tokens: None,
        }],
        extra_header_names: BTreeSet::new(),
        allow_insecure_http: false,
    }
}
async fn serve(backend: DesktopBackend) -> (String, tokio::task::JoinHandle<()>) {
    let socket = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", socket.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(socket, Arc::new(backend).into_router(Config::default()))
            .await
            .unwrap();
    });
    (base, server)
}
fn actor() -> Actor {
    Actor {
        id: "one".into(),
        metadata: BTreeMap::new(),
    }
}

#[tokio::test]
async fn selection_freezes_full_snapshot_and_factory_rejects_mutated_generation() {
    let connections = ConnectionRepository::default().for_app(AppId::new("one").unwrap());
    let id = ConnectionId::new("selected").unwrap();
    let original = connections.create(id.clone(), configuration()).unwrap();
    let observed = Arc::new(Mutex::new(vec![]));
    let executor: Arc<dyn TaskExecutor> = Arc::new(
        ChatExecutor::new(Arc::new(Provider), Arc::new(ToolRegistry::new())).with_provider_factory(
            Arc::new(Factory {
                connections: connections.clone(),
                observed: observed.clone(),
            }),
        ),
    );
    let host = AgentHost::new(
        SqliteStore::memory(64).unwrap(),
        HashMap::from([("chat.turn".into(), executor)]),
        Arc::new(SystemRuntime),
        HostOptions::default(),
    )
    .unwrap();
    let backend = DesktopBackend::open(host.clone(), actor(), TOKEN, "model")
        .await
        .unwrap()
        .with_connections(connections.clone(), None, None);
    let (base, server) = serve(backend).await;
    let client = reqwest::Client::new();
    let chat = client
        .post(format!("{base}/v1/chats"))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    let chat_id = chat["id"].as_str().unwrap();
    let turn = client
        .post(format!("{base}/v1/chats/{chat_id}/messages"))
        .bearer_auth(TOKEN)
        .header("idempotency-key", "key")
        .json(&json!({"content":"q","providerId":"selected","model":"model"}))
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    let run_id = turn["runId"].as_str().unwrap();
    let queued = host.store().get_task(run_id).await.unwrap().unwrap();
    assert_eq!(
        queued.payload["providerSnapshot"]["connection"]["generation"],
        original.generation
    );
    assert_eq!(queued.payload["providerSnapshot"]["model"], "model");
    let mut edited = original.config.clone();
    edited.account = Some("account-two".into());
    connections
        .update(&id, original.generation, edited)
        .unwrap();
    assert!(host.run_once().await.unwrap());
    assert_eq!(
        host.store().get_task(run_id).await.unwrap().unwrap().status,
        TaskStatus::Failed
    );
    assert_eq!(
        observed.lock().unwrap()[0]["connection"]["config"]["account"],
        "account-one"
    );
    server.abort();
}

#[tokio::test]
async fn insecure_external_http_requires_explicit_host_opt_in() {
    for (allow, expected) in [(false, 400), (true, 201)] {
        let host = AgentHost::new(
            SqliteStore::memory(32).unwrap(),
            HashMap::new(),
            Arc::new(SystemRuntime),
            HostOptions::default(),
        )
        .unwrap();
        let backend = DesktopBackend::open(host, actor(), TOKEN, "model")
            .await
            .unwrap()
            .with_insecure_http(allow);
        let (base, server) = serve(backend).await;
        let response=reqwest::Client::new().post(format!("{base}/v1/providers")).bearer_auth(TOKEN)
            .json(&json!({"id":"lan","label":"LAN","kind":"openai-compatible","baseUrl":"http://192.0.2.1:8080","defaultModel":"model"})).send().await.unwrap();
        assert_eq!(response.status().as_u16(), expected);
        server.abort();
    }
}
