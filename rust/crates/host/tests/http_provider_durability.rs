#![cfg(feature = "http")]
use agentkit_connections::*;
use agentkit_host::{
    AgentHost, ExecutionContext, HostOptions, HostResult, SystemRuntime, TaskExecutor,
    http::DesktopBackend,
};
use agentkit_store::{SqliteStore, TaskRecord};
use agentkit_transport_http::{Actor, Backend, Idempotency, OperationRequest};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

const TOKEN: &str = "synthetic-durability-test-token-32-bytes";

#[derive(Default)]
struct Secrets {
    inner: MemorySecretStore,
    fail_put: AtomicBool,
    block_put: AtomicBool,
    puts: AtomicUsize,
    started: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

#[async_trait]
impl SecretStore for Secrets {
    async fn get(&self, key: &SecretKey) -> Result<Secret, ConnectionError> {
        self.inner.get(key).await
    }
    async fn put(&self, key: &SecretKey, value: Secret) -> Result<(), ConnectionError> {
        self.puts.fetch_add(1, Ordering::SeqCst);
        if self.block_put.load(Ordering::SeqCst) {
            self.started.notify_one();
            self.release.notified().await;
        }
        if self.fail_put.load(Ordering::SeqCst) {
            Err(ConnectionError::SecretUnavailable)
        } else {
            self.inner.put(key, value).await
        }
    }
    async fn delete(&self, key: &SecretKey) -> Result<(), ConnectionError> {
        self.inner.delete(key).await
    }
}

struct Executor;
#[async_trait]
impl TaskExecutor for Executor {
    fn resolves_provider_snapshots(&self) -> bool {
        true
    }
    async fn execute(&self, _: ExecutionContext) -> HostResult<()> {
        unreachable!("review only queues tasks")
    }
}

struct Database(PathBuf);
impl Database {
    fn new() -> Self {
        let directory =
            std::env::temp_dir().join(format!("agentkit-provider-review-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&directory).unwrap();
        Self(directory)
    }
    fn path(&self) -> PathBuf {
        self.0.join("review.sqlite")
    }
    fn reject_snapshot(&self, reject: bool) {
        let database = rusqlite::Connection::open(self.path()).unwrap();
        database
            .execute_batch(if reject {
                "CREATE TRIGGER review_reject_connections BEFORE INSERT ON settings
             WHEN NEW.namespace='http' AND NEW.key='connections'
             BEGIN SELECT RAISE(ABORT,'injected connection snapshot failure'); END;"
            } else {
                "DROP TRIGGER review_reject_connections;"
            })
            .unwrap();
    }
}
impl Drop for Database {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Fixture {
    host: AgentHost,
    backend: Arc<DesktopBackend>,
    connections: AppConnections,
    native: Arc<NativeConnections>,
    secrets: Arc<Secrets>,
}

async fn fixture(path: &Path, secrets: Arc<Secrets>) -> Fixture {
    let host = AgentHost::new(
        SqliteStore::open(path, 64).unwrap(),
        HashMap::from([(
            "chat.turn".into(),
            Arc::new(Executor) as Arc<dyn TaskExecutor>,
        )]),
        Arc::new(SystemRuntime),
        HostOptions::default(),
    )
    .unwrap();
    fixture_with_host(host, secrets).await
}

async fn fixture_with_host(host: AgentHost, secrets: Arc<Secrets>) -> Fixture {
    let repository = match host
        .store()
        .get_setting("http", "connections")
        .await
        .unwrap()
    {
        Some(snapshot) => ConnectionRepository::restore(
            serde_json::from_value(snapshot["repository"].clone()).unwrap(),
        )
        .unwrap(),
        None => ConnectionRepository::default(),
    };
    let connections = repository.for_app(AppId::new("durability-app").unwrap());
    let native = Arc::new(NativeConnections::new(
        connections.clone(),
        secrets.clone(),
        Arc::new(UnavailableNetwork),
    ));
    let backend = DesktopBackend::open(host.clone(), actor(), TOKEN, "model")
        .await
        .unwrap()
        .with_connections(
            connections.clone(),
            Some(native.clone()),
            Some(secrets.clone()),
        );
    Fixture {
        host,
        backend: Arc::new(backend),
        connections,
        native,
        secrets,
    }
}

fn actor() -> Actor {
    Actor {
        id: "review-actor".into(),
        metadata: BTreeMap::new(),
    }
}
fn request(operation: &str, body: Value) -> OperationRequest {
    OperationRequest {
        actor: actor(),
        operation: operation.into(),
        params: BTreeMap::new(),
        query: BTreeMap::new(),
        body,
        idempotency: None,
    }
}
fn provider_request(operation: &str, id: &str, body: Value) -> OperationRequest {
    let mut request = request(operation, body);
    request.params.insert("providerId".into(), id.into());
    request
}
fn configuration(id: &str) -> Value {
    json!({"id":id,"label":"Committed","kind":"openai-compatible",
        "baseUrl":"https://committed.example.invalid/v1","defaultModel":"model","apiKey":"synthetic-original"})
}
fn changed() -> Value {
    json!({"label":"Uncommitted","baseUrl":"https://uncommitted.example.invalid/v1","apiKey":"synthetic-replacement"})
}
async fn create(fixture: &Fixture, id: &str) -> Connection {
    fixture
        .backend
        .dispatch(request("createProvider", configuration(id)))
        .await
        .unwrap();
    fixture
        .connections
        .get(&ConnectionId::new(id).unwrap())
        .unwrap()
}
async fn snapshot(fixture: &Fixture) -> Value {
    fixture
        .host
        .store()
        .get_setting("http", "connections")
        .await
        .unwrap()
        .unwrap()
}
async fn queue(fixture: &Fixture, provider: &str, suffix: &str) -> TaskRecord {
    let chat = format!("chat-{suffix}");
    fixture
        .host
        .store()
        .create_chat(&chat, None, json!({}), 0)
        .await
        .unwrap();
    let mut request = request(
        "submitMessage",
        json!({"content":"synthetic","providerId":provider,"model":"model"}),
    );
    request.params.insert("chatId".into(), chat);
    request.idempotency = Some(Idempotency {
        key: suffix.into(),
        task_id: format!("run-{suffix}"),
        fingerprint: suffix.into(),
    });
    fixture.backend.dispatch(request).await.unwrap();
    fixture
        .host
        .store()
        .get_task(&format!("run-{suffix}"))
        .await
        .unwrap()
        .unwrap()
}

mod durability {
    use super::*;
    include!("provider_durability/cases.rs");
}
mod catalog {
    use super::*;
    include!("provider_durability/catalog.rs");
}
