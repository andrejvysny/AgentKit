#[path = "support/interop.rs"]
mod interop;
use agentkit_mcp::{
    Backend, BearerAuth, CatalogTool, ClientOptions, Features, McpClient, McpClientError,
    McpServer, ServerOptions, SessionScope, model::*,
};
use async_trait::async_trait;
use rmcp::ErrorData;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};
use tokio_util::sync::CancellationToken;

#[derive(Default)]
struct Host {
    scopes: Mutex<Vec<SessionScope>>,
    revoked: Mutex<Vec<String>>,
    started: tokio::sync::Notify,
}
#[async_trait]
impl Backend for Host {
    fn features(&self) -> Features {
        Features {
            resources: true,
            prompts: true,
        }
    }
    async fn list_tools(&self, scope: &SessionScope) -> Result<Vec<CatalogTool>, ErrorData> {
        self.scopes.lock().unwrap().push(scope.clone());
        Ok([("echo", false), ("write", true)].into_iter().map(|(name, writes)| CatalogTool {
            definition: serde_json::from_value(json!({"name":name,"description":"Native test",
                "inputSchema":{"type":"object","properties":{"text":{"type":"string"}},"required":["text"]}})).unwrap(), writes,
        }).collect())
    }
    async fn call_tool(
        &self,
        name: &str,
        arguments: JsonObject,
        scope: &SessionScope,
    ) -> Result<CallToolResult, ErrorData> {
        if arguments["text"] == "hold" {
            self.scopes.lock().unwrap().push(scope.clone());
            self.started.notify_one();
            std::future::pending::<()>().await;
        }
        Ok(CallToolResult::success(vec![ContentBlock::text(format!(
            "{name}:{}",
            arguments["text"]
        ))]))
    }
    fn revoke_actor(&self, actor: &str) {
        self.revoked.lock().unwrap().push(actor.into());
    }
    async fn list_resources(&self, _scope: &SessionScope) -> Result<Vec<Resource>, ErrorData> {
        Ok(vec![
            serde_json::from_value(json!({"uri":"host://visible","name":"visible"})).unwrap(),
        ])
    }
    async fn read_resource(
        &self,
        uri: &str,
        _scope: &SessionScope,
    ) -> Result<ReadResourceResult, ErrorData> {
        Ok(serde_json::from_value(json!({"contents":[{"uri":uri,"text":"host state"}]})).unwrap())
    }
    async fn list_prompts(&self, _scope: &SessionScope) -> Result<Vec<Prompt>, ErrorData> {
        Ok(vec![
            serde_json::from_value(
                json!({"name":"hello","arguments":[{"name":"name","required":true}]}),
            )
            .unwrap(),
        ])
    }
    async fn get_prompt(
        &self,
        _name: &str,
        arguments: JsonObject,
        _scope: &SessionScope,
    ) -> Result<GetPromptResult, ErrorData> {
        Ok(serde_json::from_value(json!({"messages":[{"role":"user","content":{"type":"text","text":arguments["name"]}}]})).unwrap())
    }
}

#[tokio::test]
async fn deadline_cancels_backend_scope_and_session_remains_usable() {
    let options = ServerOptions {
        max_call_duration: std::time::Duration::from_millis(20),
        ..ServerOptions::default()
    };
    let (endpoint, server, host, handle) = serve(options).await;
    let client = McpClient::http(&endpoint, Some("secret"), ClientOptions::default())
        .await
        .unwrap();
    let failed = client
        .call_tool(
            "echo",
            serde_json::from_value(json!({"text":"hold"})).unwrap(),
            CancellationToken::new(),
        )
        .await;
    assert!(matches!(failed, Err(McpClientError::Request)));
    assert!(
        host.scopes
            .lock()
            .unwrap()
            .last()
            .unwrap()
            .cancellation
            .is_cancelled()
    );
    let tools = client.list_tools(CancellationToken::new()).await.unwrap();
    assert_eq!(tools.len(), 1);
    client.close().await.unwrap();
    server.close().await;
    handle.abort();
}

async fn serve(
    options: ServerOptions,
) -> (String, McpServer, Arc<Host>, tokio::task::JoinHandle<()>) {
    let host = Arc::new(Host::default());
    let server = McpServer::new(
        host.clone(),
        Arc::new(BearerAuth::new("secret", json!({"id":"owner"})).unwrap()),
        options,
    )
    .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/", listener.local_addr().unwrap());
    let router = server.router();
    let handle = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    (endpoint, server, host, handle)
}

async fn rpc(
    endpoint: &str,
    session: Option<&str>,
    method: &str,
    params: Value,
) -> (reqwest::StatusCode, Option<String>, Value) {
    let mut request = reqwest::Client::new()
        .post(endpoint)
        .bearer_auth("secret")
        .header("accept", "application/json, text/event-stream")
        .header(
            "mcp-protocol-version",
            params
                .get("protocolVersion")
                .and_then(Value::as_str)
                .unwrap_or("2025-11-25"),
        )
        .json(&json!({"jsonrpc":"2.0","id":1,"method":method,"params":params}));
    if let Some(session) = session {
        request = request.header("mcp-session-id", session);
    }
    let response = request.send().await.unwrap();
    let status = response.status();
    let session = response
        .headers()
        .get("mcp-session-id")
        .map(|value| value.to_str().unwrap().to_owned());
    let text = response.text().await.unwrap();
    let body = serde_json::from_str(&text)
        .ok()
        .or_else(|| {
            text.lines()
                .filter_map(|line| {
                    line.strip_prefix("data:")
                        .and_then(|data| serde_json::from_str(data.trim()).ok())
                })
                .next_back()
        })
        .unwrap_or(Value::Null);
    (status, session, body)
}

async fn initialize(endpoint: &str) -> String {
    let (status, session, body) = rpc(endpoint, None, "initialize", json!({"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"test","version":"1"}})).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["result"]["protocolVersion"], "2025-11-25");
    let session = session.unwrap();
    let response = reqwest::Client::new()
        .post(endpoint)
        .bearer_auth("secret")
        .header("accept", "application/json, text/event-stream")
        .header("mcp-session-id", &session)
        .header("mcp-protocol-version", "2025-11-25")
        .json(&json!({"jsonrpc":"2.0","method":"notifications/initialized"}))
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success());
    session
}

#[tokio::test]
async fn native_http_client_lists_calls_and_closes_with_actor_revocation() {
    let (endpoint, server, host, handle) = serve(ServerOptions::default()).await;
    let client = McpClient::http(&endpoint, Some("secret"), ClientOptions::default())
        .await
        .unwrap();
    let tools = client.list_tools(CancellationToken::new()).await.unwrap();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, "echo");
    let result = client
        .call_tool(
            "echo",
            serde_json::from_value(json!({"text":"hi"})).unwrap(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_ne!(result.is_error, Some(true));
    let invalid = client
        .call_tool("echo", JsonObject::new(), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(invalid.is_error, Some(true));
    assert_eq!(
        client
            .call_tool("write", JsonObject::new(), CancellationToken::new())
            .await
            .unwrap_err(),
        McpClientError::Request
    );
    client.close().await.unwrap();
    assert!(!host.revoked.lock().unwrap().is_empty());
    server.close().await;
    assert!(!host.revoked.lock().unwrap().is_empty());
    let scopes = host.scopes.lock().unwrap();
    assert!(
        scopes
            .iter()
            .all(|scope| scope.principal["id"] == "owner" && scope.actor_id == scopes[0].actor_id)
    );
    handle.abort();
}

#[tokio::test]
async fn native_http_extensions_reject_guessed_names_and_bad_prompt_arguments() {
    let (endpoint, server, _host, handle) = serve(ServerOptions::default()).await;
    let session = initialize(&endpoint).await;
    let (_, _, listed) = rpc(&endpoint, Some(&session), "resources/list", json!({})).await;
    assert_eq!(listed["result"]["resources"][0]["uri"], "host://visible");
    let (_, _, read) = rpc(
        &endpoint,
        Some(&session),
        "resources/read",
        json!({"uri":"host://visible"}),
    )
    .await;
    assert_eq!(read["result"]["contents"][0]["text"], "host state");
    let (_, _, guessed) = rpc(
        &endpoint,
        Some(&session),
        "resources/read",
        json!({"uri":"host://hidden"}),
    )
    .await;
    assert_eq!(guessed["error"]["code"], -32602);
    let (_, _, bad) = rpc(
        &endpoint,
        Some(&session),
        "prompts/get",
        json!({"name":"hello","arguments":{"extra":"x"}}),
    )
    .await;
    assert_eq!(bad["error"]["code"], -32602);
    let (_, _, prompt) = rpc(
        &endpoint,
        Some(&session),
        "prompts/get",
        json!({"name":"hello","arguments":{"name":"Ada"}}),
    )
    .await;
    assert_eq!(prompt["result"]["messages"][0]["content"]["text"], "Ada");
    server.close().await;
    handle.abort();
}

#[tokio::test]
async fn sdk_stdio_pagination_and_ambiguous_delivery_never_retry_writes() {
    if interop::ts_interop_skipped("mcp native stdio fixture") {
        return;
    }
    let fixture = format!("{}/tests/fixtures/stdio.mjs", env!("CARGO_MANIFEST_DIR"));
    let directory = tempfile::tempdir().unwrap();
    let marker = directory.path().join("writes");
    let environment = BTreeMap::from([
        ("MARKER".into(), marker.to_str().unwrap().into()),
        ("PATH".into(), std::env::var("PATH").unwrap()),
    ]);
    let client = McpClient::stdio("bun", &[fixture], &environment, ClientOptions::default())
        .await
        .unwrap();
    let tools = client.list_tools(CancellationToken::new()).await.unwrap();
    assert_eq!(
        tools
            .iter()
            .map(|tool| tool.name.as_ref())
            .collect::<Vec<_>>(),
        ["first", "write"]
    );
    let failure = client
        .call_tool("write", JsonObject::new(), CancellationToken::new())
        .await
        .unwrap_err();
    assert!(matches!(
        failure,
        McpClientError::Disconnected | McpClientError::Timeout
    ));
    assert_eq!(std::fs::read_to_string(&marker).unwrap(), "once\n");
    client.close().await.unwrap();
}

#[tokio::test]
async fn exact_five_typescript_revisions_initialize_and_newer_revision_is_refused() {
    let (endpoint, server, _host, handle) = serve(ServerOptions::default()).await;
    for revision in agentkit_mcp::TS_PROTOCOL_VERSIONS {
        let (status, session, body) = rpc(&endpoint, None, "initialize", json!({
            "protocolVersion":revision,"capabilities":{},"clientInfo":{"name":"test","version":"1"}
        })).await;
        assert_eq!(status, 200, "{revision}: {body}");
        assert!(session.is_some());
        assert_eq!(body["result"]["protocolVersion"], *revision);
    }
    let (status, session, body) = rpc(&endpoint, None, "initialize", json!({
        "protocolVersion":"2026-07-28","capabilities":{},"clientInfo":{"name":"test","version":"1"}
    })).await;
    assert_eq!(status, 400);
    assert!(session.is_none());
    assert_eq!(body["error"]["message"], "Unsupported MCP protocol version");
    server.close().await;
    handle.abort();
}
