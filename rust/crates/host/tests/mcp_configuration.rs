use agentkit_contracts::{CreateMcpServerRequest, UpdateMcpServerRequest};
use agentkit_host::{McpConfigError, McpConfigRepository, SystemRuntime};
use agentkit_store::SqliteStore;
use serde_json::json;
use std::sync::Arc;

fn input(alias: &str) -> CreateMcpServerRequest {
    serde_json::from_value(json!({"alias":alias,"transport":{"kind":"stdio","command":"explicit-host-tool","env":{"TOKEN":"${token}"}},"secretRefs":{"token":"keychain-ref"}})).unwrap()
}

struct FixedRuntime(i64);
impl agentkit_host::Runtime for FixedRuntime {
    fn now_ms(&self) -> i64 {
        self.0
    }
    fn id(&self) -> String {
        "clock-boundary".into()
    }
}

#[tokio::test]
async fn configuration_timestamps_cover_the_entire_javascript_date_range() {
    for (milliseconds, expected) in [
        (-8_640_000_000_000_000, "-271821-04-20T00:00:00.000Z"),
        (8_640_000_000_000_000, "+275760-09-13T00:00:00.000Z"),
    ] {
        let repository = McpConfigRepository::new(
            SqliteStore::memory(16).unwrap(),
            "app",
            Arc::new(FixedRuntime(milliseconds)),
        )
        .unwrap();
        let record = repository.create(input("tools")).await.unwrap();
        assert_eq!(record.created_at, expected);
        assert_eq!(record.updated_at, expected);
    }
    let repository = McpConfigRepository::new(
        SqliteStore::memory(16).unwrap(),
        "app",
        Arc::new(FixedRuntime(8_640_000_000_000_001)),
    )
    .unwrap();
    assert!(matches!(
        repository.create(input("tools")).await,
        Err(McpConfigError::Invalid)
    ));
    assert!(repository.list().await.unwrap().is_empty());
}

#[tokio::test]
async fn aliases_are_atomic_across_repository_handles_and_scopes_are_isolated() {
    let store = SqliteStore::memory(16).unwrap();
    let first = McpConfigRepository::new(store.clone(), "app", Arc::new(SystemRuntime)).unwrap();
    let second = McpConfigRepository::new(store.clone(), "app", Arc::new(SystemRuntime)).unwrap();
    let (left, right) = tokio::join!(first.create(input("tools")), second.create(input("tools")));
    assert_ne!(left.is_ok(), right.is_ok());
    assert!(matches!(
        left.as_ref().err().or(right.as_ref().err()),
        Some(McpConfigError::DuplicateAlias)
    ));
    let record = first.list().await.unwrap().remove(0);
    let patch: UpdateMcpServerRequest =
        serde_json::from_value(json!({"secretRefs":{},"id":"malicious","createdAt":"changed"}))
            .unwrap();
    let updated = second.update(&record.id, patch).await.unwrap();
    assert_eq!(updated.id, record.id);
    assert_eq!(updated.created_at, record.created_at);
    assert_eq!(
        serde_json::to_value(updated).unwrap()["secretRefs"],
        json!({})
    );
    let other = McpConfigRepository::new(store, "another-app", Arc::new(SystemRuntime)).unwrap();
    assert!(other.list().await.unwrap().is_empty());
    assert!(matches!(
        other.delete(&record.id).await,
        Err(McpConfigError::NotFound)
    ));
    first.delete(&record.id).await.unwrap();
    assert!(second.list().await.unwrap().is_empty());
}

#[tokio::test]
async fn declarations_survive_reopen_without_resolved_credentials() {
    let path = std::env::temp_dir().join(format!(
        "agentkit-mcp-config-{}.sqlite",
        uuid::Uuid::new_v4()
    ));
    {
        let store = SqliteStore::open(&path, 16).unwrap();
        McpConfigRepository::new(store, "app", Arc::new(SystemRuntime))
            .unwrap()
            .create(input("tools"))
            .await
            .unwrap();
    }
    {
        let store = SqliteStore::open(&path, 16).unwrap();
        let records = McpConfigRepository::new(store, "app", Arc::new(SystemRuntime))
            .unwrap()
            .list()
            .await
            .unwrap();
        assert_eq!(records.len(), 1);
        let value = serde_json::to_value(&records[0]).unwrap();
        assert_eq!(value["transport"]["env"]["TOKEN"], "${token}");
        assert_eq!(value["secretRefs"]["token"], "keychain-ref");
    }
    std::fs::remove_file(path).unwrap();
}
