#![cfg(feature = "http")]
use agentkit_core::{ChatProvider, ChatRequest, ProviderEvent, ProviderStream, ToolRegistry};
use agentkit_host::{
    AgentHost, ChatExecutor, HostOptions, SystemRuntime, TaskExecutor, http::DesktopBackend,
};
use agentkit_store::SqliteStore;
use agentkit_transport_http::{Actor, Config};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashMap},
    sync::{Arc, Mutex},
};
use tokio_util::sync::CancellationToken;

const TOKEN: &str = "12345678901234567890123456789012";
struct Provider {
    prompts: Arc<Mutex<Vec<ChatRequest>>>,
}
impl ChatProvider for Provider {
    fn id(&self) -> &str {
        "local-test"
    }
    fn stream_chat(&self, request: ChatRequest, _: CancellationToken) -> ProviderStream {
        self.prompts.lock().unwrap().push(request);
        Box::pin(futures::stream::iter([
            Ok(ProviderEvent::Delta("answer".into())),
            Ok(ProviderEvent::Completed {
                content: "answer".into(),
                tool_calls: vec![],
                finish_reason: "stop".into(),
            }),
        ]))
    }
}
fn host() -> (AgentHost, Arc<Mutex<Vec<ChatRequest>>>) {
    let prompts = Arc::new(Mutex::new(Vec::new()));
    let executor: Arc<dyn TaskExecutor> = Arc::new(ChatExecutor::new(
        Arc::new(Provider {
            prompts: prompts.clone(),
        }),
        Arc::new(ToolRegistry::new()),
    ));
    let host = AgentHost::new(
        SqliteStore::memory(64).unwrap(),
        HashMap::from([("chat.turn".into(), executor)]),
        Arc::new(SystemRuntime),
        HostOptions::default(),
    )
    .unwrap();
    (host, prompts)
}
fn actor() -> Actor {
    Actor {
        id: "desktop-user".into(),
        metadata: BTreeMap::new(),
    }
}
async fn serve(host: AgentHost) -> (String, tokio::task::JoinHandle<()>) {
    let backend = Arc::new(
        DesktopBackend::open(host, actor(), TOKEN, "model")
            .await
            .unwrap(),
    );
    let socket = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", socket.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(socket, backend.into_router(Config::default()))
            .await
            .unwrap();
    });
    (base, task)
}
async fn json(response: reqwest::Response, schema: &str) -> Value {
    let status = response.status();
    let body = response.text().await.unwrap();
    assert!(status.is_success(), "{status} {body}");
    let value: Value = serde_json::from_str(&body).unwrap();
    agentkit_contracts::validate_schema(schema, &value).unwrap();
    value
}

#[tokio::test]
async fn real_sqlite_rest_run_placeholder_replay_and_regeneration() {
    let (host, prompts) = host();
    let (base, server) = serve(host.clone()).await;
    let client = reqwest::Client::new();
    let chat = json(
        client
            .post(format!("{base}/v1/chats"))
            .bearer_auth(TOKEN)
            .json(&json!({"title":"Native"}))
            .send()
            .await
            .unwrap(),
        "ChatDtoSchema",
    )
    .await;
    let id = chat["id"].as_str().unwrap();
    let submit = || {
        client
            .post(format!("{base}/v1/chats/{id}/messages"))
            .bearer_auth(TOKEN)
            .header("idempotency-key", "same")
    };
    let turn = json(
        submit()
            .json(&json!({"content":"question"}))
            .send()
            .await
            .unwrap(),
        "SubmitMessageResponseSchema",
    )
    .await;
    let replay = submit()
        .json(&json!({"content":"question"}))
        .send()
        .await
        .unwrap();
    assert_eq!(replay.status(), 200);
    assert_eq!(replay.json::<Value>().await.unwrap(), turn);
    assert_eq!(
        submit()
            .json(&json!({"content":"changed"}))
            .send()
            .await
            .unwrap()
            .status(),
        422
    );
    let messages = host.store().messages(id, true).await.unwrap();
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[1].id, turn["assistantMessageId"].as_str().unwrap());
    assert_eq!(messages[1].metadata["placeholder"], true);
    assert!(host.run_once().await.unwrap());
    let messages = host.store().messages(id, true).await.unwrap();
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[1].content, "answer");
    assert_ne!(messages[1].metadata["placeholder"], true);
    assert_eq!(prompts.lock().unwrap()[0].messages.len(), 1);
    let run = turn["runId"].as_str().unwrap();
    let text = client
        .get(format!("{base}/v1/runs/{run}/stream"))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(text.contains("run.message.delta"));
    assert!(text.ends_with("event: agentkit.stream.settled\ndata: {}\n\n"));
    let assistant = turn["assistantMessageId"].as_str().unwrap();
    let regenerate = json(
        client
            .post(format!(
                "{base}/v1/chats/{id}/messages/{assistant}/regenerate"
            ))
            .bearer_auth(TOKEN)
            .header("idempotency-key", "again")
            .json(&json!({}))
            .send()
            .await
            .unwrap(),
        "SubmitMessageResponseSchema",
    )
    .await;
    assert_eq!(regenerate["userMessageId"], turn["userMessageId"]);
    assert_ne!(regenerate["assistantMessageId"], turn["assistantMessageId"]);
    assert!(host.run_once().await.unwrap());
    let siblings = client
        .get(format!("{base}/v1/messages/{assistant}/siblings"))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(siblings.as_array().unwrap().len(), 2);
    assert_eq!(
        client
            .get(format!("{base}/v1/chats/{id}"))
            .bearer_auth("different-actor-token")
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    server.abort();
}

#[tokio::test]
async fn settings_provider_projection_and_snapshot_restore() {
    let (host, _) = host();
    let (base, server) = serve(host.clone()).await;
    let client = reqwest::Client::new();
    let settings = json(
        client
            .patch(format!("{base}/v1/settings"))
            .bearer_auth(TOKEN)
            .json(&json!({"maxToolIterations":4,"defaultModel":"chosen"}))
            .send()
            .await
            .unwrap(),
        "SettingsDtoSchema",
    )
    .await;
    assert_eq!(settings["maxToolIterations"], 4);
    let provider=json(client.post(format!("{base}/v1/providers")).bearer_auth(TOKEN).json(&json!({"id":"local","label":"Local","kind":"openai-compatible","baseUrl":"http://127.0.0.1:9999","defaultModel":"model","metadata":{"private":"not public"}})).send().await.unwrap(),"ProviderDtoSchema").await;
    assert!(provider.get("metadata").is_none());
    assert!(provider.get("apiKey").is_none());
    server.abort();
    let _ = server.await;
    let (base, server) = serve(host).await;
    let providers = client
        .get(format!("{base}/v1/providers"))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(providers.as_array().unwrap().len(), 1);
    assert_eq!(providers[0]["id"], "local");
    let settings = client
        .get(format!("{base}/v1/settings"))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(settings["maxToolIterations"], 4);
    let unsupported = client
        .get(format!("{base}/v1/mcp/servers"))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(unsupported.status(), 501);
    assert_eq!(
        unsupported.json::<Value>().await.unwrap()["code"],
        "not_implemented"
    );
    server.abort();
}

#[tokio::test]
async fn provider_secret_headers_survive_generation_change_without_projection_leak() {
    use agentkit_connections::{
        AppId, ConnectionId, ConnectionRepository, MemorySecretStore, NativeConnections,
        SecretStore, UnavailableNetwork,
    };
    let (host, _) = host();
    let repository = ConnectionRepository::default();
    let connections = repository.for_app(AppId::new("desktop-user").unwrap());
    let secrets = Arc::new(MemorySecretStore::default());
    let native = Arc::new(NativeConnections::new(
        connections.clone(),
        secrets.clone(),
        Arc::new(UnavailableNetwork),
    ));
    let backend = DesktopBackend::new(host.clone(), actor(), TOKEN, "model")
        .unwrap()
        .with_connections(
            connections.clone(),
            Some(native.clone()),
            Some(secrets.clone()),
        );
    let socket = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", socket.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(socket, Arc::new(backend).into_router(Config::default()))
            .await
            .unwrap();
    });
    let client = reqwest::Client::new();
    let response=client.post(format!("{base}/v1/providers")).bearer_auth(TOKEN).json(&json!({"id":"secret-provider","label":"Secret","kind":"openai-compatible","baseUrl":"http://127.0.0.1:9999","defaultModel":"model","apiKey":"private-key","extraHeaders":{"x-api-token":"private-header"}})).send().await.unwrap();
    let provider = json(response, "ProviderDtoSchema").await;
    assert!(!provider.to_string().contains("private"));
    assert_eq!(
        provider["apiKeySecretRef"],
        "provider/secret-provider/api-key"
    );
    assert!(provider.get("generation").is_none());
    let id = ConnectionId::new("secret-provider").unwrap();
    let old = connections.get(&id).unwrap().generation;
    let response = client
        .patch(format!("{base}/v1/providers/secret-provider"))
        .bearer_auth(TOKEN)
        .json(&json!({"label":"Renamed"}))
        .send()
        .await
        .unwrap();
    json(response, "ProviderDtoSchema").await;
    assert!(connections.get(&id).unwrap().generation > old);
    assert_eq!(
        secrets
            .get(&native.credential_key(&id).unwrap())
            .await
            .unwrap()
            .expose(),
        "private-key"
    );
    assert!(
        secrets
            .get(&native.header_key(&id).unwrap())
            .await
            .unwrap()
            .expose()
            .contains("private-header")
    );
    let snapshot = host
        .store()
        .get_setting("http", "connections")
        .await
        .unwrap()
        .unwrap()
        .to_string();
    assert!(!snapshot.contains("private-key"));
    assert!(!snapshot.contains("private-header"));
    server.abort();
}

#[tokio::test]
async fn replay_normalizes_json_numbers_and_rejects_delimiter_collision_across_chats() {
    let (host, _) = host();
    host.store()
        .create_chat("a:b", None, json!({}), 0)
        .await
        .unwrap();
    host.store()
        .create_chat("a", None, json!({}), 0)
        .await
        .unwrap();
    let (base, server) = serve(host).await;
    let client = reqwest::Client::new();
    let submit = |chat: &str, key: &str| {
        client
            .post(format!("{base}/v1/chats/{chat}/messages"))
            .bearer_auth(TOKEN)
            .header("idempotency-key", key)
    };
    let first = submit("a:b", "c")
        .body("{\"content\":\"same\",\"metadata\":{\"number\":0.0}}")
        .send()
        .await
        .unwrap();
    assert_eq!(first.status(), 201);
    let first = first.json::<Value>().await.unwrap();
    let replay = submit("a:b", "c")
        .body("{\"metadata\":{\"number\":0},\"content\":\"same\"}")
        .send()
        .await
        .unwrap();
    assert_eq!(replay.status(), 200);
    assert_eq!(replay.json::<Value>().await.unwrap(), first);
    let collision = submit("a", "b:c")
        .json(&json!({"content":"same","metadata":{"number":0}}))
        .send()
        .await
        .unwrap();
    assert_eq!(collision.status(), 422);
    assert_eq!(
        collision.json::<Value>().await.unwrap()["code"],
        "idempotency_key_mismatch"
    );
    server.abort();
}

#[tokio::test]
async fn unsupported_selected_content_never_creates_turns_or_calls_providers() {
    let (host, prompts) = host();
    let (base, server) = serve(host.clone()).await;
    let client = reqwest::Client::new();
    let chat = json(
        client
            .post(format!("{base}/v1/chats"))
            .bearer_auth(TOKEN)
            .json(&json!({}))
            .send()
            .await
            .unwrap(),
        "ChatDtoSchema",
    )
    .await;
    let chat_id = chat["id"].as_str().unwrap();
    for path in [
        format!("chats/{chat_id}/messages"),
        format!("chats/{chat_id}/messages/unused/regenerate"),
    ] {
        for selection in [
            json!({}),
            json!({"skills":[{"id":"skill","revision":"1"}]}),
            json!({"template":{"id":"template","revision":"1","arguments":{}}}),
        ] {
            assert_content_unsupported(&client, &format!("{base}/v1/{path}"), selection).await;
        }
    }
    assert!(
        host.store()
            .list_tasks(Some(chat_id.into()))
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        host.store()
            .messages(chat_id, true)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(!host.run_once().await.unwrap());
    assert!(prompts.lock().unwrap().is_empty());
    server.abort();
}

async fn assert_content_unsupported(client: &reqwest::Client, url: &str, selection: Value) {
    let response = client
        .post(url)
        .bearer_auth(TOKEN)
        .header("idempotency-key", "unsupported")
        .json(&json!({"content":"hello","contentSelection":selection}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 501);
    assert_eq!(
        response.json::<Value>().await.unwrap()["code"],
        "not_implemented"
    );
}
