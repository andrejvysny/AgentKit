#![cfg(feature = "http")]
use agentkit_contracts::{
    CreateMcpServerRequest, GrantAllowanceRequest, McpServerDto, UpdateMcpServerRequest,
    WriteAllowanceDto,
};
use agentkit_host::{
    AgentHost, HostOptions, SystemRuntime,
    http::{DesktopBackend, McpConfigurationPort, WritePolicyPort},
};
use agentkit_store::SqliteStore;
use agentkit_transport_http::{Actor, Config, Problem};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashMap},
    sync::{Arc, Mutex},
};
const TOKEN: &str = "12345678901234567890123456789012";

struct Ports {
    scopes: Mutex<Vec<String>>,
}
#[async_trait]
impl WritePolicyPort for Ports {
    async fn list(&self, _: &Actor, chat: &str) -> Result<Vec<WriteAllowanceDto>, Problem> {
        self.scopes.lock().unwrap().push(chat.into());
        Ok(vec![])
    }
    async fn grant(
        &self,
        _: &Actor,
        chat: &str,
        input: GrantAllowanceRequest,
    ) -> Result<WriteAllowanceDto, Problem> {
        self.scopes.lock().unwrap().push(chat.into());
        let input = serde_json::to_value(input).unwrap();
        assert!(input.get("chatId").is_none());
        Ok(serde_json::from_value(json!({"key":"k","chatId":chat,"toolName":"edit","proposalKind":"edit","maxRisk":"low","createdAt":"2026-01-01T00:00:00Z"})).unwrap())
    }
    async fn revoke(&self, _: &Actor, chat: &str, _: &str) -> Result<(), Problem> {
        self.scopes.lock().unwrap().push(chat.into());
        Ok(())
    }
}
fn server_record() -> McpServerDto {
    serde_json::from_value(json!({"id":"s","alias":"server","transport":{"kind":"stdio","command":"local-server","env":{"TOKEN":"sensitive"}},"createdAt":"2026-01-01T00:00:00Z","updatedAt":"2026-01-01T00:00:00Z"})).unwrap()
}
#[async_trait]
impl McpConfigurationPort for Ports {
    async fn list(&self, _: &Actor) -> Result<Vec<McpServerDto>, Problem> {
        Ok(vec![server_record()])
    }
    async fn create(&self, _: &Actor, _: CreateMcpServerRequest) -> Result<McpServerDto, Problem> {
        Ok(server_record())
    }
    async fn update(
        &self,
        _: &Actor,
        _: &str,
        _: UpdateMcpServerRequest,
    ) -> Result<McpServerDto, Problem> {
        Ok(server_record())
    }
    async fn delete(&self, _: &Actor, _: &str) -> Result<(), Problem> {
        Ok(())
    }
}
fn host() -> AgentHost {
    AgentHost::new(
        SqliteStore::memory(32).unwrap(),
        HashMap::new(),
        Arc::new(SystemRuntime),
        HostOptions::default(),
    )
    .unwrap()
}
fn actor(id: &str) -> Actor {
    Actor {
        id: id.into(),
        metadata: BTreeMap::new(),
    }
}

#[tokio::test]
async fn configured_optional_ports_use_path_scope_and_redact_secrets() {
    let host = host();
    host.store()
        .create_chat("path-chat", None, json!({}), 0)
        .await
        .unwrap();
    let ports = Arc::new(Ports {
        scopes: Mutex::new(vec![]),
    });
    let backend = Arc::new(
        DesktopBackend::open(host, actor("one"), TOKEN, "model")
            .await
            .unwrap()
            .with_write_policy(ports.clone())
            .with_mcp_configuration(ports.clone()),
    );
    let socket = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", socket.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(socket, backend.into_router(Config::default()))
            .await
            .unwrap();
    });
    let client = reqwest::Client::new();
    let response=client.post(format!("{base}/v1/chats/path-chat/write-policy/allowances")).bearer_auth(TOKEN)
        .json(&json!({"toolName":"edit","proposalKind":"edit","maxRisk":"low","chatId":"body-injection"})).send().await.unwrap();
    assert_eq!(response.status(), 201);
    assert_eq!(
        response.json::<Value>().await.unwrap()["chatId"],
        "path-chat"
    );
    assert_eq!(*ports.scopes.lock().unwrap(), vec!["path-chat"]);
    let response = client
        .get(format!("{base}/v1/mcp/servers"))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let value = response.json::<Value>().await.unwrap();
    assert_eq!(value[0]["transport"]["env"]["TOKEN"], "***");
    assert!(!value.to_string().contains("sensitive"));
    server.abort();
}

#[tokio::test]
async fn actor_store_binding_is_atomic_and_survives_new_backend() {
    let host = host();
    let first = DesktopBackend::open(host.clone(), actor("one"), TOKEN, "model").await;
    assert!(first.is_ok());
    let second = DesktopBackend::open(host.clone(), actor("two"), TOKEN, "model").await;
    assert!(matches!(second, Err(Problem { status: 403, .. })));
    assert!(
        DesktopBackend::open(host, actor("one"), TOKEN, "model")
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn restored_connection_app_is_independent_of_actor_and_ambiguous_legacy_fails() {
    use agentkit_connections::{AppId, ConnectionRepository};
    use agentkit_transport_http::{Backend, OperationRequest};
    let host = host();
    let connections = ConnectionRepository::default().for_app(AppId::new("different-app").unwrap());
    let backend = DesktopBackend::open(host.clone(), actor("one"), TOKEN, "model")
        .await
        .unwrap()
        .with_connections(connections, None, None);
    let request = |operation: &str, body: Value| OperationRequest {
        actor: actor("one"),
        operation: operation.into(),
        params: BTreeMap::new(),
        query: BTreeMap::new(),
        body,
        idempotency: None,
    };
    backend.dispatch(request("createProvider",json!({"id":"provider","label":"Local","kind":"openai-compatible","baseUrl":"http://127.0.0.1:9999","defaultModel":"m1"}))).await.unwrap();
    assert_eq!(
        host.store()
            .get_setting("http", "connections")
            .await
            .unwrap()
            .unwrap()["appId"],
        "different-app"
    );
    let reopened = DesktopBackend::open(host.clone(), actor("one"), TOKEN, "model")
        .await
        .unwrap();
    assert_eq!(
        reopened
            .dispatch(request("listProviders", Value::Null))
            .await
            .unwrap()
            .body
            .as_array()
            .unwrap()
            .len(),
        1
    );
    host.store()
        .set_setting(
            "http",
            "connections",
            json!({"version":1,"last_generation":0,"connections":[]}),
        )
        .await
        .unwrap();
    assert!(
        DesktopBackend::open(host, actor("one"), TOKEN, "model")
            .await
            .is_err()
    );
}
