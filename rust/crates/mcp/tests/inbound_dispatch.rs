use agentkit_mcp::{model::*, *};
use async_trait::async_trait;
use rmcp::ErrorData;
use serde_json::json;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tokio_util::sync::CancellationToken;

#[derive(Default)]
struct Host(AtomicUsize);
#[async_trait]
impl Backend for Host {
    async fn list_tools(&self, scope: &SessionScope) -> Result<Vec<CatalogTool>, ErrorData> {
        scope.cancellation.cancel();
        Ok(vec![CatalogTool {
            definition: serde_json::from_value(
                json!({"name":"effect","inputSchema":{"type":"object"}}),
            )
            .unwrap(),
            writes: true,
        }])
    }
    async fn call_tool(
        &self,
        _: &str,
        _: JsonObject,
        _: &SessionScope,
    ) -> Result<CallToolResult, ErrorData> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(CallToolResult::success(vec![]))
    }
    fn revoke_actor(&self, _: &str) {}
}
#[tokio::test]
async fn catalogue_cancellation_in_same_poll_prevents_backend_effect() {
    let host = Arc::new(Host::default());
    let server = McpServer::new(
        host.clone(),
        Arc::new(BearerAuth::new("secret", json!({"id":"owner"})).unwrap()),
        ServerOptions {
            writes_enabled: true,
            ..ServerOptions::default()
        },
    )
    .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/", listener.local_addr().unwrap());
    let router = server.router();
    let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let client = McpClient::http(&url, Some("secret"), ClientOptions::default())
        .await
        .unwrap();
    let result = client
        .call_tool("effect", JsonObject::new(), CancellationToken::new())
        .await;
    assert_eq!(
        host.0.load(Ordering::SeqCst),
        0,
        "cancelled catalogue must not dispatch a write"
    );
    assert!(matches!(result, Err(McpClientError::Request)));
    client.close().await.unwrap();
    server.close().await;
    task.abort();
}
