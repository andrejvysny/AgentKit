use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

use agentkit_contracts::{CreateMcpServerRequest, McpServerDto};
use agentkit_engine::SystemRuntime;
use agentkit_mcp::{McpConfigError, McpConfigLease, McpConfigRuntime, McpConfigurationService};
use agentkit_store::SqliteStore;
use async_trait::async_trait;
use serde_json::json;
use tokio::sync::Notify;

#[derive(Default)]
struct Runtime {
    reject: AtomicBool,
    reserved: Arc<AtomicBool>,
    invalidations: AtomicUsize,
    reconciles: AtomicUsize,
    entered: Notify,
    release: Notify,
}

struct Lease(Arc<AtomicBool>);
impl McpConfigLease for Lease {}
impl Drop for Lease {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

#[async_trait]
impl McpConfigRuntime for Runtime {
    async fn prepare(
        &self,
        _: u64,
        _: Vec<McpServerDto>,
    ) -> Result<Box<dyn McpConfigLease>, McpConfigError> {
        if self.reject.load(Ordering::SeqCst) {
            return Err(McpConfigError::Conflict);
        }
        assert!(!self.reserved.swap(true, Ordering::SeqCst));
        Ok(Box::new(Lease(self.reserved.clone())))
    }

    fn invalidate(&self, _: u64, _: &[String]) {
        assert!(self.reserved.load(Ordering::SeqCst));
        self.invalidations.fetch_add(1, Ordering::SeqCst);
    }

    async fn reconcile(&self, _: u64, _: Vec<McpServerDto>) -> Result<(), McpConfigError> {
        assert!(self.reserved.load(Ordering::SeqCst));
        self.reconciles.fetch_add(1, Ordering::SeqCst);
        self.entered.notify_one();
        self.release.notified().await;
        assert!(self.reserved.load(Ordering::SeqCst));
        Ok(())
    }
}

fn input() -> CreateMcpServerRequest {
    serde_json::from_value(
        json!({"alias":"notes","transport":{"kind":"stdio","command":"new-command"}}),
    )
    .unwrap()
}

fn fixture() -> (
    SqliteStore,
    McpConfigurationService<SqliteStore>,
    Arc<Runtime>,
) {
    let store = SqliteStore::memory(16).unwrap();
    let runtime = Arc::new(Runtime::default());
    let repository = McpConfigurationService::new(store.clone(), "app", Arc::new(SystemRuntime))
        .unwrap()
        .with_ports(None, Some(runtime.clone()))
        .unwrap();
    (store, repository, runtime)
}

#[tokio::test]
async fn rejected_preflight_preserves_durable_config_and_live_authority() {
    let (store, repository, runtime) = fixture();
    runtime.reject.store(true, Ordering::SeqCst);
    assert!(matches!(
        repository.create(input()).await,
        Err(McpConfigError::Conflict)
    ));
    assert!(
        store
            .get_setting("mcp-config", "app")
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .get_setting("mcp-config-generation", "app")
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(runtime.invalidations.load(Ordering::SeqCst), 0);
    assert_eq!(runtime.reconciles.load(Ordering::SeqCst), 0);
    assert!(!runtime.reserved.load(Ordering::SeqCst));
}

#[tokio::test]
async fn reservation_lasts_through_reconcile_and_releases_on_success() {
    let (_, repository, runtime) = fixture();
    let creating = tokio::spawn(async move { repository.create(input()).await });
    runtime.entered.notified().await;
    assert!(runtime.reserved.load(Ordering::SeqCst));
    runtime.release.notify_one();
    creating.await.unwrap().unwrap();
    assert!(!runtime.reserved.load(Ordering::SeqCst));
}

#[tokio::test]
async fn cancellation_releases_reservation_without_reinstating_invalidated_authority() {
    let (_, repository, runtime) = fixture();
    let creating = tokio::spawn(async move { repository.create(input()).await });
    runtime.entered.notified().await;
    creating.abort();
    assert!(creating.await.unwrap_err().is_cancelled());
    assert!(!runtime.reserved.load(Ordering::SeqCst));
    assert_eq!(runtime.invalidations.load(Ordering::SeqCst), 1);
    assert_eq!(runtime.reconciles.load(Ordering::SeqCst), 1);
}
