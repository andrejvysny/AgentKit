use std::collections::BTreeMap;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
};

use agentkit_contracts::{CreateMcpServerRequest, McpServerDto, UpdateMcpServerRequest};
use agentkit_host::{
    McpConfigError, McpConfigRepository, McpConfigRuntime, McpConfigSecrets, SystemRuntime,
};
use agentkit_mcp::{
    ClientOptions, McpClientError, McpClientManager, McpConnection, McpConnector,
    McpSecretResolver, SystemClock, TransportConfig, model,
};
use agentkit_store::SqliteStore;
use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

#[derive(Default)]
struct Vault(Mutex<BTreeMap<String, String>>);
#[async_trait]
impl McpConfigSecrets for Vault {
    async fn get(&self, reference: &str) -> Result<Option<String>, McpConfigError> {
        Ok(self.0.lock().unwrap().get(reference).cloned())
    }
    async fn set(&self, reference: &str, value: &str) -> Result<(), McpConfigError> {
        self.0
            .lock()
            .unwrap()
            .insert(reference.into(), value.into());
        Ok(())
    }
    async fn delete(&self, reference: &str) -> Result<(), McpConfigError> {
        self.0.lock().unwrap().remove(reference);
        Ok(())
    }
}
#[async_trait]
impl McpSecretResolver for Vault {
    async fn get(&self, reference: &str) -> Result<Option<String>, McpClientError> {
        Ok(self.0.lock().unwrap().get(reference).cloned())
    }
}

#[derive(Default)]
struct Connector {
    pause: AtomicBool,
    started: Notify,
    release: Notify,
    values: Mutex<Vec<String>>,
    calls: AtomicUsize,
    closes: AtomicUsize,
}
struct Connection(Arc<Connector>);
#[async_trait]
impl McpConnection for Connection {
    async fn list_tools(&self, _: CancellationToken) -> Result<Vec<model::Tool>, McpClientError> {
        Ok(vec![
            serde_json::from_value(json!({"name":"echo","inputSchema":{"type":"object"}})).unwrap(),
        ])
    }
    async fn call_tool(
        &self,
        _: &str,
        _: model::JsonObject,
        _: CancellationToken,
    ) -> Result<model::CallToolResult, McpClientError> {
        self.0.calls.fetch_add(1, Ordering::SeqCst);
        Ok(serde_json::from_value(json!({"content":[]})).unwrap())
    }
    async fn close(&self) -> Result<(), McpClientError> {
        self.0.closes.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}
struct ConnectorPort(Arc<Connector>);
#[async_trait]
impl McpConnector for ConnectorPort {
    async fn connect(
        &self,
        transport: TransportConfig,
        _: ClientOptions,
        _: CancellationToken,
    ) -> Result<Arc<dyn McpConnection>, McpClientError> {
        if let TransportConfig::Stdio { env, .. } = transport {
            self.0.values.lock().unwrap().extend(env.into_values());
        }
        self.0.started.notify_one();
        if self.0.pause.load(Ordering::SeqCst) {
            self.0.release.notified().await;
        }
        Ok(Arc::new(Connection(self.0.clone())))
    }
}
struct Runtime {
    manager: Arc<McpClientManager>,
    generation: AtomicU64,
    stale_metadata: AtomicBool,
    fail_reconcile: AtomicBool,
}
#[async_trait]
impl McpConfigRuntime for Runtime {
    fn invalidate(&self, generation: u64, aliases: &[String]) {
        if generation < self.generation.load(Ordering::SeqCst) {
            return;
        }
        self.generation.store(generation, Ordering::SeqCst);
        for alias in aliases {
            self.manager.revoke_alias(alias);
        }
    }
    async fn reconcile(
        &self,
        generation: u64,
        records: Vec<McpServerDto>,
    ) -> Result<(), McpConfigError> {
        if generation < self.generation.load(Ordering::SeqCst) {
            return Err(McpConfigError::Conflict);
        }
        let configs = records
            .into_iter()
            .map(|record| {
                serde_json::from_value(serde_json::to_value(record).unwrap())
                    .map_err(|_| McpConfigError::Invalid)
            })
            .collect::<Result<Vec<_>, _>>()?;
        if self.fail_reconcile.load(Ordering::SeqCst) {
            return Err(McpConfigError::Runtime);
        }
        self.manager
            .replace_configs(configs)
            .await
            .map_err(|_| McpConfigError::Runtime)?;
        self.generation.store(generation, Ordering::SeqCst);
        Ok(())
    }
    fn metadata(&self, generation: u64, alias: &str) -> Option<Value> {
        if generation != self.generation.load(Ordering::SeqCst)
            || !self.manager.aliases().contains(&alias.to_owned())
        {
            return None;
        }
        let reported = if self.stale_metadata.load(Ordering::SeqCst) {
            generation - 1
        } else {
            generation
        };
        Some(
            json!({"generation":reported,"status":if self.manager.connected_aliases().contains(&alias.to_owned()) {"connected"} else {"disconnected"},"toolCount":1,"secret":"must not escape"}),
        )
    }
}

fn input(alias: &str, token: Option<&str>) -> CreateMcpServerRequest {
    let mut transport = json!({"kind":"stdio","command":"host-owned-command"});
    if let Some(token) = token {
        transport["env"] = json!({"TOKEN":token});
    }
    serde_json::from_value(json!({"alias":alias,"transport":transport})).unwrap()
}
fn patch(value: Value) -> UpdateMcpServerRequest {
    serde_json::from_value(value).unwrap()
}
fn ports(
    store: SqliteStore,
) -> (
    McpConfigRepository,
    Arc<Vault>,
    Arc<Runtime>,
    Arc<Connector>,
) {
    let vault = Arc::new(Vault::default());
    let connector = Arc::new(Connector::default());
    let manager = Arc::new(
        McpClientManager::with_ports(
            Vec::new(),
            vault.clone(),
            Arc::new(ConnectorPort(connector.clone())),
            Arc::new(SystemClock),
        )
        .unwrap(),
    );
    let runtime = Arc::new(Runtime {
        manager,
        generation: AtomicU64::new(0),
        stale_metadata: AtomicBool::new(false),
        fail_reconcile: AtomicBool::new(false),
    });
    let repository = McpConfigRepository::new(store, "app", Arc::new(SystemRuntime))
        .unwrap()
        .with_ports(Some(vault.clone()), Some(runtime.clone()))
        .unwrap();
    (repository, vault, runtime, connector)
}

#[tokio::test]
async fn literals_require_an_explicit_vault_and_never_reach_sqlite_or_projection() {
    let store = SqliteStore::memory(16).unwrap();
    let repository =
        McpConfigRepository::new(store.clone(), "no-vault", Arc::new(SystemRuntime)).unwrap();
    assert!(matches!(
        repository
            .create(input("tools", Some("dummy-secret-one")))
            .await,
        Err(McpConfigError::SecretStore)
    ));
    assert!(
        store
            .get_setting("mcp-config", "no-vault")
            .await
            .unwrap()
            .is_none()
    );
    let (repository, vault, runtime, connector) = ports(store.clone());
    repository.initialize().await.unwrap();
    let record = repository
        .create(input("tools", Some("dummy-secret-one")))
        .await
        .unwrap();
    let projected = serde_json::to_value(&record).unwrap();
    assert_eq!(projected["transport"]["env"]["TOKEN"], "***");
    let saved = store
        .get_setting("mcp-config", "app")
        .await
        .unwrap()
        .unwrap();
    assert!(!saved.to_string().contains("dummy-secret-one"));
    assert!(
        saved[0]["transport"]["env"]["TOKEN"]
            .as_str()
            .unwrap()
            .starts_with("${secret_")
    );
    assert_eq!(vault.0.lock().unwrap().len(), 1);
    runtime
        .manager
        .connect("tools", CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(*connector.values.lock().unwrap(), vec!["dummy-secret-one"]);
    assert_metadata_fence(&repository, &runtime).await;
    runtime.manager.dispose().await.unwrap();
}

async fn assert_metadata_fence(repository: &McpConfigRepository, runtime: &Runtime) {
    let listed = serde_json::to_value(repository.list().await.unwrap()).unwrap();
    assert_eq!(listed[0]["connection"]["status"], "connected");
    assert!(listed[0]["connection"].get("secret").is_none());
    runtime.stale_metadata.store(true, Ordering::SeqCst);
    assert!(
        serde_json::to_value(repository.list().await.unwrap()).unwrap()[0]
            .get("connection")
            .is_none()
    );
}

#[tokio::test]
async fn foreign_refs_and_redacted_target_changes_are_refused_before_vault_access() {
    let store = SqliteStore::memory(16).unwrap();
    let (repository, vault, runtime, _) = ports(store);
    let foreign: CreateMcpServerRequest = serde_json::from_value(json!({"alias":"steal","transport":{"kind":"stdio","command":"host-owned-command","env":{"TOKEN":"${token}"}},"secretRefs":{"token":"oauth/another-account/credentials"}})).unwrap();
    assert!(matches!(
        repository.create(foreign).await,
        Err(McpConfigError::Invalid)
    ));
    assert!(vault.0.lock().unwrap().is_empty());
    let record = repository
        .create(input("tools", Some("dummy-secret-one")))
        .await
        .unwrap();
    let original_refs = vault.0.lock().unwrap().keys().cloned().collect::<Vec<_>>();
    let scope = original_refs[0].rsplit_once('/').unwrap().0;
    let traversal = format!("{scope}/../../provider-credential");
    assert!(matches!(repository.update(&record.id, patch(json!({"transport":{"kind":"stdio","command":"host-owned-command","env":{"TOKEN":"${token}"}},"secretRefs":{"token":traversal}}))).await, Err(McpConfigError::Invalid)));

    repository.update(&record.id, patch(json!({"transport":{"kind":"stdio","command":"host-owned-command","env":{"TOKEN":"***"}}}))).await.unwrap();
    assert_eq!(
        vault.0.lock().unwrap().keys().cloned().collect::<Vec<_>>(),
        original_refs
    );
    assert!(matches!(repository.update(&record.id, patch(json!({"transport":{"kind":"stdio","command":"other-target","env":{"TOKEN":"***"}}}))).await, Err(McpConfigError::Invalid)));
    assert_eq!(vault.0.lock().unwrap().len(), 1);
    runtime.manager.dispose().await.unwrap();
}

#[tokio::test]
async fn rotation_revokes_old_handles_and_delete_cancels_pending_connect() {
    let store = SqliteStore::memory(16).unwrap();
    let (repository, vault, runtime, connector) = ports(store);
    let record = repository
        .create(input("tools", Some("dummy-secret-one")))
        .await
        .unwrap();
    runtime
        .manager
        .connect("tools", CancellationToken::new())
        .await
        .unwrap();
    let old = runtime.manager.binding("tools").unwrap();
    let updated = repository.update(&record.id, patch(json!({"transport":{"kind":"stdio","command":"host-owned-command","env":{"TOKEN":"dummy-secret-two"}}}))).await.unwrap();
    assert!(
        updated.extra["generation"].as_u64().unwrap()
            > record.extra["generation"].as_u64().unwrap()
    );
    assert!(
        runtime
            .manager
            .call_tool_bound(
                "mcp.tools.echo",
                old,
                Default::default(),
                CancellationToken::new()
            )
            .await
            .is_err()
    );
    assert_eq!(connector.calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        vault.0.lock().unwrap().len(),
        1,
        "rotation removes the old owned secret"
    );
    delete_pending(&repository, &record, &runtime, &connector).await;
    assert!(vault.0.lock().unwrap().is_empty());
    assert!(repository.list().await.unwrap().is_empty());
}

async fn delete_pending(
    repository: &McpConfigRepository,
    record: &McpServerDto,
    runtime: &Runtime,
    connector: &Connector,
) {
    connector.started.notified().await;
    connector.pause.store(true, Ordering::SeqCst);
    let connecting = runtime.manager.connect("tools", CancellationToken::new());
    let deleting = async {
        connector.started.notified().await;
        repository.delete(&record.id).await.unwrap();
    };
    let (connected, ()) = tokio::time::timeout(std::time::Duration::from_secs(1), async {
        tokio::join!(connecting, deleting)
    })
    .await
    .expect("delete must not await the peer");
    assert!(connected.is_err());
    assert!(runtime.manager.aliases().is_empty());
}

#[tokio::test]
async fn aborted_storage_commit_fences_live_state_and_retains_only_prior_refs() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("store.sqlite");
    let store = SqliteStore::open(&path, 16).unwrap();
    let (repository, vault, runtime, _) = ports(store.clone());
    let record = repository
        .create(input("tools", Some("dummy-secret-one")))
        .await
        .unwrap();
    runtime
        .manager
        .connect("tools", CancellationToken::new())
        .await
        .unwrap();
    let database = rusqlite::Connection::open(&path).unwrap();
    database.execute_batch("CREATE TRIGGER fail_mcp_write BEFORE UPDATE ON settings WHEN NEW.namespace='mcp-config' BEGIN SELECT RAISE(ABORT,'config write failed'); END;").unwrap();
    assert!(repository.update(&record.id, patch(json!({"transport":{"kind":"stdio","command":"host-owned-command","env":{"TOKEN":"dummy-secret-two"}}}))).await.is_err());
    assert!(
        runtime.manager.aliases().is_empty(),
        "unknown authority cannot remain callable after failed commit"
    );
    assert_eq!(
        vault
            .0
            .lock()
            .unwrap()
            .values()
            .cloned()
            .collect::<Vec<_>>(),
        vec!["dummy-secret-one"]
    );
    database
        .execute_batch("DROP TRIGGER fail_mcp_write")
        .unwrap();
    repository.initialize().await.unwrap();
    runtime
        .manager
        .connect("tools", CancellationToken::new())
        .await
        .unwrap();
    assert!(
        repository.list().await.unwrap()[0].extra["generation"]
            .as_u64()
            .unwrap()
            > record.extra["generation"].as_u64().unwrap()
    );
    runtime.manager.dispose().await.unwrap();
}

#[tokio::test]
async fn durable_generation_never_reuses_deleted_or_prior_process_authority() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("store.sqlite");
    let store = SqliteStore::open(&path, 16).unwrap();
    let (repository, _, runtime, _) = ports(store.clone());
    let first = repository.create(input("tools", None)).await.unwrap();
    let generation = first.extra["generation"].as_u64().unwrap();
    repository.delete(&first.id).await.unwrap();
    runtime.manager.dispose().await.unwrap();
    drop(repository);
    drop(store);
    let store = SqliteStore::open(&path, 16).unwrap();
    let (repository, _, runtime, _) = ports(store.clone());
    let second = repository.create(input("tools", None)).await.unwrap();
    assert!(second.extra["generation"].as_u64().unwrap() > generation);
    let other = McpConfigRepository::new(store, "other-app", Arc::new(SystemRuntime)).unwrap();
    assert_eq!(
        other.create(input("tools", None)).await.unwrap().extra["generation"],
        1
    );
    runtime.manager.dispose().await.unwrap();
}

#[tokio::test]
async fn one_live_runtime_cannot_cross_application_or_store_scopes() {
    let store = SqliteStore::memory(16).unwrap();
    let (repository, _, runtime, _) = ports(store.clone());
    let other_app = McpConfigRepository::new(store, "other-app", Arc::new(SystemRuntime)).unwrap();
    assert!(matches!(
        other_app.with_ports(None, Some(runtime.clone())),
        Err(McpConfigError::Conflict)
    ));
    let other_store = McpConfigRepository::new(
        SqliteStore::memory(16).unwrap(),
        "app",
        Arc::new(SystemRuntime),
    )
    .unwrap();
    assert!(matches!(
        other_store.with_ports(None, Some(runtime.clone())),
        Err(McpConfigError::Conflict)
    ));
    repository.create(input("tools", None)).await.unwrap();
    runtime.manager.dispose().await.unwrap();
}

#[tokio::test]
async fn runtime_failure_after_commit_retains_the_referenced_secret_for_retry() {
    let store = SqliteStore::memory(16).unwrap();
    let (repository, vault, runtime, _) = ports(store.clone());
    runtime.fail_reconcile.store(true, Ordering::SeqCst);
    assert!(matches!(
        repository
            .create(input("tools", Some("dummy-secret-one")))
            .await,
        Err(McpConfigError::Runtime)
    ));
    let saved = store
        .get_setting("mcp-config", "app")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(saved.as_array().unwrap().len(), 1);
    assert!(!saved.to_string().contains("dummy-secret-one"));
    assert_eq!(vault.0.lock().unwrap().len(), 1);
    runtime.fail_reconcile.store(false, Ordering::SeqCst);
    repository.initialize().await.unwrap();
    runtime
        .manager
        .connect("tools", CancellationToken::new())
        .await
        .unwrap();
    runtime.manager.dispose().await.unwrap();
}
