#![cfg(feature = "http")]
use agentkit_connections::*;
use agentkit_core::{ChatRequest, ProviderEvent};
use agentkit_host::{
    AgentHost, ChatProviderFactory, HostOptions, SystemRuntime,
    http::{DesktopBackend, NativeProviderFactory, ProviderConsentPort},
};
use agentkit_store::SqliteStore;
use agentkit_transport_http::{Actor, Problem};
use async_trait::async_trait;
use futures::StreamExt;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    sync::{Arc, Mutex},
};
use tokio_util::sync::CancellationToken;

#[derive(Default)]
struct Consent {
    requests: Mutex<Vec<Value>>,
    denied: bool,
}
#[async_trait]
impl ProviderConsentPort for Consent {
    async fn approve(
        &self,
        _: &Actor,
        plan: &RequestPlan,
        _: tokio_util::sync::CancellationToken,
    ) -> Result<bool, Problem> {
        self.requests.lock().unwrap().push(json!({"destination":plan.destination().as_str(),"payload":String::from_utf8_lossy(plan.payload()),"generation":plan.connection().generation}));
        Ok(!self.denied)
    }
}
struct Fixture {
    _directory: tempfile::TempDir,
    _backend: DesktopBackend,
    factory: Arc<NativeProviderFactory>,
    connections: AppConnections,
    selected: Connection,
    consent: Arc<Consent>,
}
async fn fixture(endpoint: &str, denied: bool) -> Fixture {
    let directory = tempfile::tempdir().unwrap();
    let host = AgentHost::new(
        SqliteStore::open(directory.path().join("store.sqlite"), 64).unwrap(),
        HashMap::new(),
        Arc::new(SystemRuntime),
        HostOptions::default(),
    )
    .unwrap();
    let connections = ConnectionRepository::default().for_app(AppId::new("app").unwrap());
    let selected = connections
        .create(
            ConnectionId::new("provider").unwrap(),
            ConnectionConfig {
                label: "Test".into(),
                provider_kind: "openai-compatible".into(),
                endpoint: endpoint.parse().unwrap(),
                account: Some("account".into()),
                credential_required: true,
                extra_header_names: BTreeSet::new(),
                allow_insecure_http: false,
                enabled: true,
                capabilities: BTreeSet::from([
                    Capability::Chat,
                    Capability::ModelDiscovery,
                    Capability::Probe,
                ]),
                models: vec![Model {
                    id: "model".into(),
                    display_name: None,
                    capabilities: BTreeSet::from([Capability::Chat]),
                    context_window_tokens: None,
                }],
            },
        )
        .unwrap();
    let secrets = Arc::new(MemorySecretStore::default());
    let native = NativeConnections::new(
        connections.clone(),
        secrets.clone(),
        Arc::new(UnavailableNetwork),
    );
    secrets
        .put(
            &native.credential_key(&selected.id).unwrap(),
            Secret::new("synthetic-key"),
        )
        .await
        .unwrap();
    let factory = Arc::new(NativeProviderFactory::default());
    assert!(!factory.available());
    let consent = Arc::new(Consent {
        denied,
        ..Default::default()
    });
    let backend = DesktopBackend::new(
        host,
        Actor {
            id: "app".into(),
            metadata: BTreeMap::new(),
        },
        "synthetic-provider-test-token-32-bytes",
        "model",
    )
    .unwrap()
    .with_connections(connections.clone(), None, Some(secrets))
    .with_native_provider_operations(consent.clone(), factory.clone())
    .unwrap();
    assert!(factory.available());
    Fixture {
        _directory: directory,
        _backend: backend,
        factory,
        connections,
        selected,
        consent,
    }
}
fn request() -> ChatRequest {
    ChatRequest {
        model: "model".into(),
        run_id: "run".into(),
        ..Default::default()
    }
}
async fn resolve(fixture: &Fixture) -> Arc<dyn agentkit_core::ChatProvider> {
    fixture
        .factory
        .resolve(
            &json!({"connection":fixture.selected,"model":"model"}),
            CancellationToken::new(),
        )
        .await
        .unwrap()
}
async fn server(
    body: &'static str,
    path: &str,
) -> (
    String,
    Arc<Mutex<Vec<(axum::http::HeaderMap, Value)>>>,
    tokio::task::JoinHandle<()>,
) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let capture = seen.clone();
    let app = axum::Router::new().route(
        path,
        axum::routing::post(
            move |headers: axum::http::HeaderMap, axum::Json(value): axum::Json<Value>| {
                let capture = capture.clone();
                async move {
                    capture.lock().unwrap().push((headers, value));
                    ([("content-type", "text/event-stream")], body)
                }
            },
        ),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/v1", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (endpoint, seen, task)
}
const STREAM: &str = "data: {\"id\":\"native-reply\",\"choices\":[{\"delta\":{\"content\":\"Native answer\"}}]}\n\ndata: {\"choices\":[],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":2,\"total_tokens\":5}}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";

#[tokio::test]
async fn native_factory_streams_with_same_vault_and_preserves_usage() {
    let (endpoint, seen, task) = server(STREAM, "/v1/chat/completions").await;
    let fixture = fixture(&endpoint, false).await;
    let provider = resolve(&fixture).await;
    let events = provider
        .stream_chat(request(), CancellationToken::new())
        .collect::<Vec<_>>()
        .await;
    assert!(events.iter().all(Result::is_ok), "{events:?}");
    assert!(events.iter().any(
        |event| matches!(event,Ok(ProviderEvent::Event{kind,data}) if kind=="run.message.completed" && data["content"]=="Native answer")
    ));
    assert!(
        events.iter().any(
            |event| matches!(event, Ok(ProviderEvent::Usage(data)) if data["totalTokens"] == 5)
        )
    );
    let requests = seen.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].0["authorization"], "Bearer synthetic-key");
    assert_eq!(requests[0].1["model"], "model");
    assert_eq!(fixture.consent.requests.lock().unwrap().len(), 1);
    task.abort();
}

#[tokio::test]
async fn native_factory_denies_exact_request_before_network_and_rechecks_rotated_generation() {
    let fixture = fixture("http://127.0.0.1:1/v1", true).await;
    let provider = resolve(&fixture).await;
    let error = provider
        .stream_chat(request(), CancellationToken::new())
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .find_map(Result::err)
        .unwrap();
    assert_eq!(error.code, "consent_required");
    assert_eq!(fixture.consent.requests.lock().unwrap().len(), 1);
    fixture
        .connections
        .update(
            &fixture.selected.id,
            fixture.selected.generation,
            fixture.selected.config.clone(),
        )
        .unwrap();
    let error = fixture
        .factory
        .resolve(
            &json!({"connection":fixture.selected,"model":"model"}),
            CancellationToken::new(),
        )
        .await
        .err()
        .unwrap();
    assert!(error.to_string().contains("connection_changed"));
    let error = provider
        .stream_chat(request(), CancellationToken::new())
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .find_map(Result::err)
        .unwrap();
    assert_eq!(error.code, "connection_changed");
}

#[tokio::test]
async fn pre_cancelled_native_request_never_asks_for_consent() {
    let fixture = fixture("http://127.0.0.1:1/v1", false).await;
    let provider = resolve(&fixture).await;
    let cancel = CancellationToken::new();
    cancel.cancel();
    let error = provider
        .stream_chat(request(), cancel)
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .find_map(Result::err)
        .unwrap();
    assert_eq!(error.code, "cancelled");
    assert!(fixture.consent.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn native_cancellation_stops_a_pending_network_request() {
    let started = Arc::new(tokio::sync::Notify::new());
    let signal = started.clone();
    let app = axum::Router::new().route(
        "/v1/chat/completions",
        axum::routing::post(move || {
            let signal = signal.clone();
            async move {
                signal.notify_one();
                std::future::pending::<&'static str>().await
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/v1", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let fixture = fixture(&endpoint, false).await;
    let provider = resolve(&fixture).await;
    let cancel = CancellationToken::new();
    let mut stream = provider.stream_chat(request(), cancel.clone());
    assert!(matches!(
        stream.next().await.unwrap().unwrap(),
        ProviderEvent::Started
    ));
    let pending = tokio::spawn(async move { stream.next().await.unwrap() });
    tokio::time::timeout(std::time::Duration::from_secs(1), started.notified())
        .await
        .unwrap();
    cancel.cancel();
    let error = tokio::time::timeout(std::time::Duration::from_secs(1), pending)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert_eq!(error.code, "cancelled");
    server.abort();
}

#[tokio::test]
async fn responses_capability_uses_native_protocol_and_usage_with_explicit_selection() {
    const BODY: &str = "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"id\":\"message1\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[]}}\n\ndata: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"item_id\":\"message1\",\"content_index\":0,\"delta\":\"answer\"}\n\ndata: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"output\":[{\"id\":\"message1\",\"type\":\"message\",\"role\":\"assistant\",\"status\":\"completed\",\"content\":[{\"type\":\"output_text\",\"text\":\"answer\"}]}],\"usage\":{\"input_tokens\":2,\"output_tokens\":3,\"total_tokens\":5}}}\n\n";
    let (endpoint, seen, task) = server(BODY, "/v1/responses").await;
    let mut fixture = fixture(&endpoint, false).await;
    let mut config = fixture.selected.config.clone();
    config.credential_required = false;
    config.capabilities = BTreeSet::from([Capability::Responses]);
    config.models[0].capabilities = config.capabilities.clone();
    fixture.selected = fixture
        .connections
        .update(&fixture.selected.id, fixture.selected.generation, config)
        .unwrap();
    let provider = resolve(&fixture).await;
    let events = provider
        .stream_chat(request(), CancellationToken::new())
        .collect::<Vec<_>>()
        .await;
    assert!(events.iter().all(Result::is_ok), "{events:?}");
    assert!(
        events.iter().any(
            |event| matches!(event, Ok(ProviderEvent::Usage(data)) if data["totalTokens"] == 5)
        )
    );
    let requests = seen.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].1["store"], false);
    assert_eq!(fixture.consent.requests.lock().unwrap().len(), 1);
    task.abort();
}

#[tokio::test]
async fn admission_snapshot_uses_current_native_authority_and_refuses_revoked_or_unknown_models() {
    let fixture = fixture("http://127.0.0.1:1/v1", false).await;
    let snapshot = fixture
        .factory
        .snapshot("provider", "model", CancellationToken::new())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        snapshot["connection"]["generation"],
        fixture.selected.generation
    );
    assert!(
        fixture
            .factory
            .resolve(&snapshot, CancellationToken::new())
            .await
            .is_ok()
    );
    assert!(
        fixture
            .factory
            .snapshot("provider", "unknown-model", CancellationToken::new())
            .await
            .is_err()
    );
    assert!(fixture.consent.requests.lock().unwrap().is_empty());
    fixture
        .connections
        .delete(&fixture.selected.id, fixture.selected.generation)
        .unwrap();
    assert!(
        fixture
            .factory
            .snapshot("provider", "model", CancellationToken::new())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn unbound_factory_only_falls_back_to_an_explicit_static_registry_id() {
    let factory = NativeProviderFactory::default()
        .with_static_provider_ids(BTreeSet::from(["custom-callback".into()]));
    assert!(
        factory
            .snapshot("custom-callback", "model", CancellationToken::new())
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        factory
            .snapshot("unknown", "model", CancellationToken::new())
            .await
            .is_err()
    );
}

#[derive(Debug)]
struct SelectedGuard(Arc<std::sync::atomic::AtomicBool>);
#[async_trait]
impl agentkit_core::DispatchGuard for SelectedGuard {
    async fn check(&self) -> Result<(), agentkit_core::CoreError> {
        if self.0.load(std::sync::atomic::Ordering::SeqCst) {
            Ok(())
        } else {
            Err(agentkit_core::CoreError::new(
                "selected_content_revoked",
                "Selected content grant was revoked",
            ))
        }
    }
    fn fence(&self) -> agentkit_core::DispatchFence {
        agentkit_core::DispatchFence {
            task_id: "run".into(),
            attempt_id: "attempt".into(),
            fencing_token: 1,
        }
    }
}
struct RevokingConsent(Arc<std::sync::atomic::AtomicBool>);
#[async_trait]
impl ProviderConsentPort for RevokingConsent {
    async fn approve(
        &self,
        _: &Actor,
        _: &RequestPlan,
        _: CancellationToken,
    ) -> Result<bool, Problem> {
        tokio::task::yield_now().await;
        self.0.store(false, std::sync::atomic::Ordering::SeqCst);
        Ok(true)
    }
}

#[tokio::test]
async fn selected_grant_revoked_during_consent_never_reaches_provider() {
    let (endpoint, seen, server) = server(STREAM, "/v1/chat/completions").await;
    let directory = tempfile::tempdir().unwrap();
    let host = AgentHost::new(
        SqliteStore::open(directory.path().join("guard.sqlite"), 64).unwrap(),
        HashMap::new(),
        Arc::new(SystemRuntime),
        HostOptions::default(),
    )
    .unwrap();
    let connections = ConnectionRepository::default().for_app(AppId::new("guard-app").unwrap());
    let selected = connections
        .create(
            ConnectionId::new("guard-provider").unwrap(),
            ConnectionConfig {
                label: "Guard".into(),
                provider_kind: "openai-compatible".into(),
                endpoint: endpoint.parse().unwrap(),
                account: None,
                credential_required: false,
                extra_header_names: BTreeSet::new(),
                allow_insecure_http: false,
                enabled: true,
                capabilities: BTreeSet::from([Capability::Chat]),
                models: vec![Model {
                    id: "model".into(),
                    display_name: None,
                    capabilities: BTreeSet::from([Capability::Chat]),
                    context_window_tokens: None,
                }],
            },
        )
        .unwrap();
    let alive = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let factory = Arc::new(NativeProviderFactory::default());
    let _backend = DesktopBackend::new(
        host,
        Actor {
            id: "guard-app".into(),
            metadata: BTreeMap::new(),
        },
        "synthetic-guard-token-at-least-32-bytes",
        "model",
    )
    .unwrap()
    .with_connections(
        connections,
        None,
        Some(Arc::new(MemorySecretStore::default())),
    )
    .with_native_provider_operations(Arc::new(RevokingConsent(alive.clone())), factory.clone())
    .unwrap();
    let provider = factory
        .resolve(
            &json!({"connection":selected,"model":"model"}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let mut input = request();
    input.request_guard = Some(Arc::new(SelectedGuard(alive)));
    let events = provider
        .stream_chat(input, CancellationToken::new())
        .collect::<Vec<_>>()
        .await;
    assert!(
        events
            .iter()
            .any(|event| matches!(event,Err(error) if error.code=="selected_content_revoked")),
        "{events:?}"
    );
    assert!(seen.lock().unwrap().is_empty());
    server.abort();
}
