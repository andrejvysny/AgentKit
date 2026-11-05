#![cfg(feature = "http")]
use agentkit_host::{AgentHost, HostOptions, SystemRuntime, http::DesktopBackend};
use agentkit_store::SqliteStore;
use agentkit_transport_http::{Actor, Config};
use serde_json::{Value, json};
use std::{collections::BTreeMap, sync::Arc};

const TOKEN: &str = "12345678901234567890123456789012";

async fn backend() -> (SqliteStore, DesktopBackend) {
    let store = SqliteStore::memory(16).unwrap();
    let host = AgentHost::new(
        store.clone(),
        Default::default(),
        Arc::new(SystemRuntime),
        HostOptions::default(),
    )
    .unwrap();
    let backend = DesktopBackend::open(
        host,
        Actor {
            id: "desktop".into(),
            metadata: BTreeMap::new(),
        },
        TOKEN,
        "model",
    )
    .await
    .unwrap();
    (store, backend)
}

async fn serve(backend: DesktopBackend) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v1/chats", listener.local_addr().unwrap());
    let backend = Arc::new(backend.with_session_profile("desktop-profile").unwrap());
    let server = tokio::spawn(async move {
        axum::serve(listener, backend.into_router(Config::default()))
            .await
            .unwrap();
    });
    (url, server)
}

#[tokio::test]
async fn invalid_profile_never_creates_a_chat() {
    let (store, backend) = backend().await;
    assert!(backend.with_session_profile(" ").is_err());
    assert!(store.list_chats(true).await.unwrap().is_empty());
}

#[tokio::test]
async fn validated_rest_session_creation_has_no_intermediate_rows() {
    let (store, backend) = backend().await;
    let (url, server) = serve(backend).await;
    let client = reqwest::Client::new();
    for body in [
        json!({"title":42}),
        json!({"metadata":{"agentkit.session":{}}}),
        json!({"metadata":{"agentkit.session.approvals":{}}}),
    ] {
        let response = client
            .post(&url)
            .bearer_auth(TOKEN)
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 400, "{}", response.text().await.unwrap());
        assert!(store.list_chats(true).await.unwrap().is_empty());
    }
    let response = client
        .post(&url)
        .bearer_auth(TOKEN)
        .json(&json!({"title":"native session", "metadata":{"custom":"kept"}}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 201);
    let chat: Value = response.json().await.unwrap();
    assert_eq!(
        chat["metadata"]["agentkit.session"]["profile"]["id"],
        "desktop-profile"
    );
    assert_eq!(chat["metadata"]["custom"], "kept");
    assert_eq!(store.list_chats(true).await.unwrap().len(), 1);
    server.abort();
}
