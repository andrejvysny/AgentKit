//! Review R7: the handler must hand the backend a TYPED JSON-RPC request
//! id — numeric `1` and string `"1"` are distinct ids and must never be
//! collapsed by a stringifying conversion — and every initialized session
//! carries its own ingress identity (`actor_id`), so a governed backend
//! can scope redelivery proofs to the session that earned them.

use agentkit_mcp::{
    Backend, BearerAuth, CatalogTool, McpServer, RpcId, ServerOptions, SessionScope, model::*,
};
use async_trait::async_trait;
use rmcp::ErrorData;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct Host {
    /// (session ingress identity, typed request id) per observed call.
    seen: Mutex<Vec<(String, Option<RpcId>)>>,
}

#[async_trait]
impl Backend for Host {
    async fn list_tools(&self, _scope: &SessionScope) -> Result<Vec<CatalogTool>, ErrorData> {
        Ok(vec![CatalogTool {
            definition: serde_json::from_value(json!({
                "name": "echo",
                "description": "Echoes.",
                "inputSchema": {"type": "object", "properties": {}}
            }))
            .unwrap(),
            writes: false,
        }])
    }
    async fn call_tool(
        &self,
        _name: &str,
        _arguments: JsonObject,
        scope: &SessionScope,
    ) -> Result<CallToolResult, ErrorData> {
        self.seen
            .lock()
            .unwrap()
            .push((scope.actor_id.clone(), scope.request_id.clone()));
        Ok(CallToolResult::success(vec![ContentBlock::text("ok")]))
    }
    fn revoke_actor(&self, _actor: &str) {}
}

async fn serve(host: Arc<Host>) -> (String, McpServer) {
    let server = McpServer::new(
        host,
        Arc::new(BearerAuth::new("secret", json!({"id": "owner"})).unwrap()),
        ServerOptions::default(),
    )
    .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/", listener.local_addr().unwrap());
    let router = server.router();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    (endpoint, server)
}

async fn request(
    client: &reqwest::Client,
    endpoint: &str,
    session: Option<&str>,
    payload: Value,
) -> (reqwest::StatusCode, Vec<(String, String)>, String) {
    let mut request = client
        .post(endpoint)
        .bearer_auth("secret")
        .header("accept", "application/json, text/event-stream")
        .header("content-type", "application/json")
        .header("mcp-protocol-version", "2025-11-25");
    if let Some(session) = session {
        request = request.header("mcp-session-id", session);
    }
    let response = request.json(&payload).send().await.unwrap();
    let status = response.status();
    let headers = response
        .headers()
        .iter()
        .map(|(name, value)| {
            (
                name.as_str().to_owned(),
                value.to_str().unwrap_or_default().to_owned(),
            )
        })
        .collect();
    (status, headers, response.text().await.unwrap())
}

fn frame(text: &str) -> Value {
    serde_json::from_str(text)
        .ok()
        .or_else(|| {
            text.lines()
                .filter_map(|line| {
                    line.strip_prefix("data:")
                        .and_then(|data| serde_json::from_str(data.trim()).ok())
                })
                .next_back()
        })
        .unwrap_or(Value::Null)
}

async fn init(client: &reqwest::Client, endpoint: &str) -> String {
    let (status, headers, text) = request(
        client,
        endpoint,
        None,
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
            "protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"rpc-id","version":"1"}}}),
    )
    .await;
    assert_eq!(status, 200, "{text}");
    headers
        .iter()
        .find(|(name, _)| name == "mcp-session-id")
        .expect("initialize mints a session")
        .1
        .clone()
}

/// One `tools/call` whose JSON-RPC id is exactly `id` (raw JSON value:
/// numeric or string, exactly as the client wrote it).
async fn call(client: &reqwest::Client, endpoint: &str, session: &str, id: Value) {
    let (status, _, text) = request(
        client,
        endpoint,
        Some(session),
        json!({"jsonrpc":"2.0","id":id,"method":"tools/call",
               "params":{"name":"echo","arguments":{}}}),
    )
    .await;
    assert_eq!(status, 200, "{text}");
    let response = frame(&text);
    assert!(
        response["result"].is_object(),
        "the call succeeded: {response}"
    );
    assert_ne!(response["result"]["isError"].as_bool(), Some(true));
    // Wire behavior is unchanged: the response echoes the id exactly as
    // the transport received it.
    assert_eq!(response["id"], id);
}

/// Review R7: numeric `1` and string `"1"` are different JSON-RPC ids.
/// The handler's conversion must deliver both types faithfully; a lossy
/// stringify made them one key and fused unrelated calls.
#[tokio::test]
async fn numeric_and_string_ids_never_collapse() {
    let host = Arc::new(Host::default());
    let (endpoint, _server) = serve(host.clone()).await;
    let client = reqwest::Client::new();
    let session = init(&client, &endpoint).await;
    call(&client, &endpoint, &session, json!(1)).await;
    call(&client, &endpoint, &session, json!("1")).await;
    let seen = host.seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[0].1, Some(RpcId::Number(1)));
    assert_eq!(seen[1].1, Some(RpcId::String("1".to_owned())));
    assert_ne!(
        seen[0].1, seen[1].1,
        "numeric 1 and string \"1\" are distinct ids and distinct map keys"
    );
}

/// The per-session ingress identity distinguishes connected sessions, so
/// a backend memo keyed on (actor_id, typed id) can never share a result
/// across two sessions that restate the same id.
#[tokio::test]
async fn each_connected_session_carries_its_own_ingress_identity() {
    let host = Arc::new(Host::default());
    let (endpoint, _server) = serve(host.clone()).await;
    let client = reqwest::Client::new();
    let first = init(&client, &endpoint).await;
    let second = init(&client, &endpoint).await;
    assert_ne!(first, second, "two live sessions");
    call(&client, &endpoint, &first, json!(1)).await;
    call(&client, &endpoint, &second, json!(1)).await;
    let seen = host.seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[0].1, Some(RpcId::Number(1)));
    assert_eq!(seen[1].1, Some(RpcId::Number(1)));
    assert_ne!(
        seen[0].0, seen[1].0,
        "both sessions restate id 1; only the session identity separates them"
    );
}

/// The typed id itself: constructors keep the JSON type, `Other` cannot
/// collide with a real number or string, and equality/Hash treat the
/// types as distinct map keys.
#[test]
fn rpc_id_preserves_the_json_type() {
    assert_eq!(RpcId::from_json(&json!(1)), RpcId::Number(1));
    assert_eq!(RpcId::from_json(&json!("1")), RpcId::String("1".into()));
    assert_ne!(RpcId::from_json(&json!(1)), RpcId::from_json(&json!("1")));
    assert_eq!(RpcId::from_json(&json!(null)), RpcId::Other("null".into()));
    assert_eq!(RpcId::from_json(&json!(1.5)), RpcId::Other("1.5".into()));
    let mut memo = std::collections::HashMap::new();
    memo.insert(RpcId::Number(1), "number");
    memo.insert(RpcId::String("1".into()), "string");
    memo.insert(RpcId::Other("1".into()), "other");
    assert_eq!(memo.len(), 3, "three distinct keys, never fused");
}
