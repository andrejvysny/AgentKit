#![cfg(feature = "http")]

use agentkit_contracts::CreateMcpServerRequest;
use agentkit_host::{
    McpConfigError, McpConfigRepository, McpConfigSecrets, SystemRuntime,
    http::McpConfigurationPort,
};
use agentkit_store::SqliteStore;
use agentkit_transport_http::Actor;
use async_trait::async_trait;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

#[derive(Default)]
struct Vault(Mutex<BTreeMap<String, String>>);
#[async_trait]
impl McpConfigSecrets for Vault {
    async fn get(&self, key: &str) -> Result<Option<String>, McpConfigError> {
        Ok(self.0.lock().unwrap().get(key).cloned())
    }
    async fn set(&self, key: &str, value: &str) -> Result<(), McpConfigError> {
        self.0.lock().unwrap().insert(key.into(), value.into());
        Ok(())
    }
    async fn delete(&self, key: &str) -> Result<(), McpConfigError> {
        self.0.lock().unwrap().remove(key);
        Ok(())
    }
}
fn input() -> CreateMcpServerRequest {
    serde_json::from_value(json!({"alias":"http","transport":{"kind":"http","url":"https://resource.test/mcp","headers":{"Authorization":"Bearer dummy-secret"}}})).unwrap()
}
fn actor(id: &str) -> Actor {
    Actor {
        id: id.into(),
        metadata: BTreeMap::new(),
    }
}

#[tokio::test]
async fn production_http_port_requires_vault_and_scopes_before_secret_access() {
    let (repository, store, vault) = configured_port().await;
    let record = McpConfigurationPort::create(&repository, &actor("app"), input())
        .await
        .unwrap();
    let projected = serde_json::to_value(record).unwrap();
    assert_eq!(projected["transport"]["headers"]["Authorization"], "***");
    assert!(!projected.to_string().contains("dummy-secret"));
    let stored = store
        .get_setting("mcp-config", "app")
        .await
        .unwrap()
        .unwrap();
    assert!(!stored.to_string().contains("dummy-secret"));
    assert!(
        stored[0]["transport"]["headers"]["Authorization"]
            .as_str()
            .unwrap()
            .starts_with("${secret_")
    );
    assert_eq!(
        vault
            .0
            .lock()
            .unwrap()
            .values()
            .cloned()
            .collect::<Vec<_>>(),
        vec!["Bearer dummy-secret"]
    );
    let listed: Value = serde_json::to_value(
        McpConfigurationPort::list(&repository, &actor("app"))
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(listed[0]["transport"]["headers"]["Authorization"], "***");
}

async fn configured_port() -> (McpConfigRepository, SqliteStore, Arc<Vault>) {
    let store = SqliteStore::memory(16).unwrap();
    let repository =
        McpConfigRepository::new(store.clone(), "app", Arc::new(SystemRuntime)).unwrap();
    let missing = McpConfigurationPort::create(&repository, &actor("app"), input())
        .await
        .unwrap_err();
    assert_eq!(missing.status, 501);
    assert!(
        store
            .get_setting("mcp-config", "app")
            .await
            .unwrap()
            .is_none()
    );
    let vault = Arc::new(Vault::default());
    let repository = repository.with_ports(Some(vault.clone()), None).unwrap();
    let denied = McpConfigurationPort::create(&repository, &actor("other-app"), input())
        .await
        .unwrap_err();
    assert_eq!(denied.status, 403);
    assert!(vault.0.lock().unwrap().is_empty());
    (repository, store, vault)
}

#[tokio::test]
async fn unused_transport_fields_cannot_bypass_credential_storage_rules() {
    let store = SqliteStore::memory(16).unwrap();
    let repository =
        McpConfigRepository::new(store.clone(), "app", Arc::new(SystemRuntime)).unwrap();
    let input: CreateMcpServerRequest = serde_json::from_value(json!({"alias":"stdio","transport":{"kind":"stdio","command":"host-owned-command","headers":{"Authorization":"dummy-unused-secret"}}})).unwrap();
    repository.create(input).await.unwrap();
    let input: CreateMcpServerRequest = serde_json::from_value(json!({"alias":"http","transport":{"kind":"http","url":"https://resource.test/mcp","env":{"TOKEN":"dummy-unused-secret"}}})).unwrap();
    repository.create(input).await.unwrap();
    assert!(
        !store
            .get_setting("mcp-config", "app")
            .await
            .unwrap()
            .unwrap()
            .to_string()
            .contains("dummy-unused-secret")
    );
}
