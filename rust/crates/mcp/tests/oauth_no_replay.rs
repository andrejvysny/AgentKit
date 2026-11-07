use agentkit_mcp::{McpClientError, McpClientManager, McpSecretResolver};
use async_trait::async_trait;
use axum::{Json, Router, extract::State, http::StatusCode, routing::post};
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tokio_util::sync::CancellationToken;

struct Secrets;
#[async_trait]
impl McpSecretResolver for Secrets {
    async fn get(&self, _: &str) -> Result<Option<String>, McpClientError> {
        Ok(None)
    }
}

async fn endpoint(
    State(effects): State<Arc<AtomicUsize>>,
    Json(request): Json<Value>,
) -> (StatusCode, Json<Value>) {
    let id = &request["id"];
    let (status, result) = match request["method"].as_str() {
        Some("initialize") => (
            StatusCode::OK,
            json!({"result":{"protocolVersion":"2025-11-25","capabilities":{"tools":{}},"serverInfo":{"name":"write-before-401","version":"1"}}}),
        ),
        Some("tools/call") => {
            effects.fetch_add(1, Ordering::SeqCst);
            (
                StatusCode::UNAUTHORIZED,
                json!({"error":{"code":-32001,"message":"authorization required"}}),
            )
        }
        Some("tools/list") => (
            StatusCode::OK,
            json!({"result":{"tools":[{"name":"write","inputSchema":{"type":"object"}}]}}),
        ),
        _ => return (StatusCode::ACCEPTED, Json(Value::Null)),
    };
    let mut response = json!({"jsonrpc":"2.0","id":id});
    response
        .as_object_mut()
        .unwrap()
        .extend(result.as_object().unwrap().clone());
    (status, Json(response))
}

#[tokio::test]
async fn unauthorized_after_remote_mutation_is_never_replayed_even_with_retries_enabled() {
    let effects = Arc::new(AtomicUsize::new(0));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint_url = format!("http://{}/mcp", listener.local_addr().unwrap());
    let router = Router::new()
        .route("/mcp", post(endpoint))
        .with_state(effects.clone());
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let config = serde_json::from_value(
        json!({"alias":"writes","transport":{"kind":"http","url":endpoint_url},
        "resilience":{"maxConnectAttempts":1,"retryTimeouts":true,"reconnectMaxAttempts":3}}),
    )
    .unwrap();
    let manager = McpClientManager::new(vec![config], Arc::new(Secrets)).unwrap();
    let result = manager
        .call_tool(
            "mcp.writes.write",
            Default::default(),
            CancellationToken::new(),
        )
        .await;
    assert!(result.is_err());
    assert_eq!(
        effects.load(Ordering::SeqCst),
        1,
        "a dispatched mutation with an uncertain answer must remain single-send"
    );
    manager.dispose().await.unwrap();
    server.abort();
}
