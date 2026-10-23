use agentkit_mcp::{
    Authenticator, Backend, CatalogTool, ClientOptions, McpClient, McpClientError, McpServer,
    ServerOptions, SessionScope, TS_PROTOCOL_VERSIONS, model::*,
};
use async_trait::async_trait;
use rmcp::ErrorData;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio_util::sync::CancellationToken;

#[derive(Default)]
struct Host {
    calls: AtomicUsize,
    revoked: Mutex<Vec<String>>,
}
#[async_trait]
impl Backend for Host {
    async fn list_tools(&self, _scope: &SessionScope) -> Result<Vec<CatalogTool>, ErrorData> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(vec![])
    }
    async fn call_tool(
        &self,
        _name: &str,
        _arguments: JsonObject,
        _scope: &SessionScope,
    ) -> Result<CallToolResult, ErrorData> {
        unreachable!("empty catalogue forbids execution")
    }
    fn revoke_actor(&self, actor: &str) {
        self.revoked.lock().unwrap().push(actor.into());
    }
}
struct Authentication;
#[async_trait]
impl Authenticator for Authentication {
    async fn authenticate(&self, headers: &http::HeaderMap) -> Option<(Value, Option<String>)> {
        match headers.get("authorization")?.to_str().ok()? {
            "Bearer one" => Some((json!({"id":"one"}), Some("chat-one".into()))),
            "Bearer two" => Some((json!({"id":"two"}), Some("chat-two".into()))),
            _ => None,
        }
    }
}

async fn serve(
    options: ServerOptions,
) -> (String, McpServer, Arc<Host>, tokio::task::JoinHandle<()>) {
    let host = Arc::new(Host::default());
    let server = McpServer::new(host.clone(), Arc::new(Authentication), options).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/", listener.local_addr().unwrap());
    let router = server.router();
    let handle = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    (endpoint, server, host, handle)
}

async fn initialize(endpoint: &str) -> String {
    let response = reqwest::Client::new().post(endpoint).bearer_auth("one")
        .header("accept", "application/json, text/event-stream")
        .json(&json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
            "protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"security","version":"1"}}}))
        .send().await.unwrap();
    assert_eq!(response.status(), 200);
    response
        .headers()
        .get("mcp-session-id")
        .unwrap()
        .to_str()
        .unwrap()
        .into()
}

#[tokio::test]
async fn auth_precedes_catalogue_and_other_principals_cannot_borrow_sessions() {
    let (endpoint, server, host, handle) = serve(ServerOptions::default()).await;
    let client = reqwest::Client::new();
    assert_eq!(
        client
            .post(&endpoint)
            .body("malformed")
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    assert_eq!(host.calls.load(Ordering::SeqCst), 0);
    let session = initialize(&endpoint).await;
    for method in [
        reqwest::Method::GET,
        reqwest::Method::POST,
        reqwest::Method::DELETE,
    ] {
        let response = client
            .request(method, &endpoint)
            .bearer_auth("two")
            .header("mcp-session-id", &session)
            .body("malformed")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 404);
    }
    assert_eq!(host.calls.load(Ordering::SeqCst), 0);
    server.close().await;
    assert_eq!(host.revoked.lock().unwrap().len(), 1);
    handle.abort();
}

#[tokio::test]
async fn rebinding_body_bounds_and_session_eviction_fail_closed() {
    let options = ServerOptions {
        max_sessions_per_principal: 1,
        max_request_bytes: 1024,
        allowed_origins: Some(vec!["https://trusted.example:443".into()]),
        ..ServerOptions::default()
    };
    let (endpoint, server, host, handle) = serve(options).await;
    let client = reqwest::Client::new();
    let evil = client
        .post(&endpoint)
        .bearer_auth("one")
        .header("host", "evil.example")
        .header("content-type", "application/json")
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(evil.status(), 403);
    let origin = client
        .post(&endpoint)
        .bearer_auth("one")
        .header("origin", "https://evil.example")
        .header("content-type", "application/json")
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(origin.status(), 403);
    assert_eq!(
        client
            .post(&endpoint)
            .bearer_auth("one")
            .body(" ".repeat(1025))
            .send()
            .await
            .unwrap()
            .status(),
        413
    );
    let old = initialize(&endpoint).await;
    let new = initialize(&endpoint).await;
    assert_ne!(old, new);
    assert_eq!(
        client
            .delete(&endpoint)
            .bearer_auth("one")
            .header("mcp-session-id", old)
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
    assert!(!host.revoked.lock().unwrap().is_empty());
    assert_eq!(host.calls.load(Ordering::SeqCst), 0);
    server.close().await;
    handle.abort();
}

#[tokio::test]
async fn stdio_repeated_cursor_page_bounds_and_precancel_are_bounded() {
    let fixture = format!("{}/tests/fixtures/stdio.mjs", env!("CARGO_MANIFEST_DIR"));
    let environment = BTreeMap::from([
        ("PATH".into(), std::env::var("PATH").unwrap()),
        ("LOOP".into(), "1".into()),
    ]);
    let client = McpClient::stdio("bun", &[fixture], &environment, ClientOptions::default())
        .await
        .unwrap();
    assert_eq!(
        client
            .list_tools(CancellationToken::new())
            .await
            .unwrap_err(),
        McpClientError::Pagination
    );
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    assert_eq!(
        client
            .call_tool("write", JsonObject::new(), cancelled)
            .await
            .unwrap_err(),
        McpClientError::Cancelled
    );
    client.close().await.unwrap();
    assert_eq!(
        client
            .list_tools(CancellationToken::new())
            .await
            .unwrap_err(),
        McpClientError::Closed
    );
}

#[test]
fn protocol_freeze_matches_installed_typescript_sdk_and_excludes_newer_rmcp_revision() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..");
    let source = std::fs::read_to_string(
        root.join("node_modules/@modelcontextprotocol/sdk/dist/esm/types.js"),
    )
    .unwrap();
    let latest = source
        .split("LATEST_PROTOCOL_VERSION = '")
        .nth(1)
        .unwrap()
        .split('\'')
        .next()
        .unwrap();
    let list = source
        .split("SUPPORTED_PROTOCOL_VERSIONS = [")
        .nth(1)
        .unwrap()
        .split(']')
        .next()
        .unwrap();
    let versions: Vec<_> = list
        .split(',')
        .map(|entry| {
            let entry = entry.trim().trim_matches('\'');
            if entry == "LATEST_PROTOCOL_VERSION" {
                latest
            } else {
                entry
            }
        })
        .collect();
    assert_eq!(versions, TS_PROTOCOL_VERSIONS);
    assert!(!TS_PROTOCOL_VERSIONS.contains(&"2026-07-28"));
}

#[tokio::test]
async fn missing_stdio_program_is_reported_without_leaking_environment() {
    let options = ClientOptions {
        connect_timeout: Duration::from_millis(100),
        ..ClientOptions::default()
    };
    let result = McpClient::stdio(
        "/nonexistent/agentkit-fixture",
        &[],
        &BTreeMap::new(),
        options,
    )
    .await;
    assert!(matches!(result, Err(McpClientError::Connect)));
    assert_eq!(McpClientError::Connect.to_string(), "MCP connection failed");
}
