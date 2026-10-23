#[path = "support/managed.rs"]
mod support;
use agentkit_mcp::*;
use async_trait::async_trait;
use rmcp::ErrorData;
use serde_json::json;
use std::sync::{Arc, Mutex, atomic::Ordering};
use support::{Secrets, token};

#[derive(Default)]
struct Host {
    actors: Mutex<Vec<String>>,
    revoked: Mutex<Vec<String>>,
}
#[async_trait]
impl Backend for Host {
    async fn list_tools(&self, scope: &SessionScope) -> Result<Vec<CatalogTool>, ErrorData> {
        self.actors.lock().unwrap().push(scope.actor_id.clone());
        Ok(vec![CatalogTool {
            definition: support::tools().remove(0),
            writes: false,
        }])
    }
    async fn call_tool(
        &self,
        _: &str,
        _: model::JsonObject,
        scope: &SessionScope,
    ) -> Result<model::CallToolResult, ErrorData> {
        self.actors.lock().unwrap().push(scope.actor_id.clone());
        Ok(model::CallToolResult::success(vec![
            model::ContentBlock::text("ok"),
        ]))
    }
    fn revoke_actor(&self, actor: &str) {
        self.revoked.lock().unwrap().push(actor.into());
    }
}
struct RotationAuth;
#[async_trait]
impl Authenticator for RotationAuth {
    async fn authenticate(
        &self,
        headers: &http::HeaderMap,
    ) -> Option<(serde_json::Value, Option<String>)> {
        matches!(
            headers.get("authorization")?.to_str().ok()?,
            "Bearer first" | "Bearer second"
        )
        .then(|| (json!({"id":"owner"}), None))
    }
}

#[tokio::test]
async fn real_http_rotation_reauthenticates_and_revokes_previous_session_actor() {
    let host = Arc::new(Host::default());
    let server = McpServer::new(
        host.clone(),
        Arc::new(RotationAuth),
        ServerOptions::default(),
    )
    .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/", listener.local_addr().unwrap());
    let router = server.router();
    let task = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let config: ServerConfig = serde_json::from_value(
        json!({"alias":"docs", "transport":{"kind":"http", "url":url,
        "headers":{"authorization":"Bearer ${token}"}}, "secretRefs":{"token":"credential"}}),
    )
    .unwrap();
    let secrets = Arc::new(Secrets::default());
    secrets
        .values
        .lock()
        .unwrap()
        .insert("credential".into(), "first".into());
    let manager = McpClientManager::new(vec![config], secrets.clone()).unwrap();
    assert_eq!(manager.list_tools("docs", token()).await.unwrap().len(), 1);
    let first_actor = host.actors.lock().unwrap()[0].clone();
    secrets
        .values
        .lock()
        .unwrap()
        .insert("credential".into(), "second".into());
    secrets.revision.fetch_add(1, Ordering::SeqCst);
    manager
        .call_tool("mcp.docs.remote", model::JsonObject::new(), token())
        .await
        .unwrap();
    let second_actor = host.actors.lock().unwrap().last().unwrap().clone();
    assert_ne!(first_actor, second_actor);
    assert_eq!(*host.revoked.lock().unwrap(), [first_actor]);
    manager.dispose().await.unwrap();
    assert_eq!(host.revoked.lock().unwrap().len(), 2);
    server.close().await;
    task.abort();
}
