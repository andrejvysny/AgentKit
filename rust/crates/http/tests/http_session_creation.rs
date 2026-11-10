use agentkit_engine::SystemRuntime;
use agentkit_http::{Actor, Config, DesktopBackend};
use agentkit_sessions::{AgentHost, HostOptions};
use agentkit_store::SqliteStore;
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
    let backend = Arc::new(backend.with_session_profile("chat").unwrap());
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
        "chat"
    );
    assert_eq!(chat["metadata"]["custom"], "kept");
    assert_eq!(store.list_chats(true).await.unwrap().len(), 1);
    server.abort();
}

struct NoopTurn;
#[async_trait::async_trait]
impl agentkit_engine::TaskExecutor<agentkit_store::SqliteStore> for NoopTurn {
    async fn execute(
        &self,
        _: agentkit_sessions::ExecutionContext,
    ) -> agentkit_engine::EngineResult<(), agentkit_store::StoreError> {
        Ok(())
    }
}

#[tokio::test]
async fn generic_metadata_updates_cannot_remove_or_forge_the_session_profile() {
    let store = SqliteStore::memory(16).unwrap();
    let host = AgentHost::new(
        store.clone(),
        std::collections::HashMap::from([(
            "chat.turn".into(),
            Arc::new(NoopTurn)
                as Arc<dyn agentkit_engine::TaskExecutor<agentkit_store::SqliteStore>>,
        )]),
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
    let (url, server) = serve(backend).await;
    let client = reqwest::Client::new();
    let chat: Value = client
        .post(&url)
        .bearer_auth(TOKEN)
        .json(&json!({"metadata":{"custom":"original"}}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = chat["id"].as_str().unwrap();
    let target = format!("{url}/{id}");
    let original = chat["metadata"]["agentkit.session"].clone();
    let response = client
        .patch(&target)
        .bearer_auth(TOKEN)
        .json(&json!({"metadata":{"custom":"updated"}}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(
        store.get_chat(id).await.unwrap().unwrap().metadata["agentkit.session"],
        original
    );
    let mut forged = original.clone();
    let profile = agentkit_sessions::ProfileRegistry::default()
        .resolve("coding")
        .unwrap();
    forged["profile"] = json!({"id":"coding","digest":profile.digest()});
    forged["profileSnapshot"] = serde_json::to_value(profile).unwrap();
    assert!(
        agentkit_sessions::read_session_descriptor(&json!({"agentkit.session":forged})).is_some()
    );
    let response = client
        .patch(&target)
        .bearer_auth(TOKEN)
        .json(&json!({"metadata":{"agentkit.session":forged}}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 400);
    let sent = client
        .post(format!("{target}/messages"))
        .bearer_auth(TOKEN)
        .header("idempotency-key", "safe-profile")
        .json(&json!({"content":"question","model":"model"}))
        .send()
        .await
        .unwrap();
    assert_eq!(sent.status(), 201);
    let sent: Value = sent.json().await.unwrap();
    let task = store
        .get_task(sent["runId"].as_str().unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(task.payload["profileSnapshot"]["id"], "chat");
    assert_eq!(task.payload["profileSnapshot"]["tools"], json!([]));
    server.abort();
}

#[tokio::test]
async fn ordinary_http_chat_creation_refuses_host_metadata_injection() {
    let (store, backend) = backend().await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v1/chats", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(listener, Arc::new(backend).into_router(Config::default()))
            .await
            .unwrap();
    });
    let response = reqwest::Client::new()
        .post(url)
        .bearer_auth(TOKEN)
        .json(&json!({"metadata":{"agentkit.session":{"version":1}}}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 400);
    assert!(store.list_chats(true).await.unwrap().is_empty());
    server.abort();
}
