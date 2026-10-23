#[path = "support/managed.rs"]
mod support;
use agentkit_core::{CoreError, DispatchFence, DispatchGuard};
use agentkit_mcp::*;
use async_trait::async_trait;
use std::sync::{
    Arc,
    atomic::{AtomicU64, AtomicUsize, Ordering},
};
use support::{Connection, token};
use tokio::sync::Notify;

#[derive(Debug, Default)]
struct LeaseGuard {
    now: AtomicU64,
    checks: AtomicUsize,
}
#[async_trait]
impl DispatchGuard for LeaseGuard {
    async fn check(&self) -> Result<(), CoreError> {
        self.checks.fetch_add(1, Ordering::SeqCst);
        if self.now.load(Ordering::SeqCst) >= 100 {
            Err(CoreError::new("lease_lost", "lease expired"))
        } else {
            Ok(())
        }
    }
    fn fence(&self) -> DispatchFence {
        DispatchFence {
            task_id: "task".into(),
            attempt_id: "attempt".into(),
            fencing_token: 1,
        }
    }
}
struct Connector {
    attempts: AtomicUsize,
    first: Arc<Connection>,
    second: Arc<Connection>,
    started: Notify,
    released: Notify,
}
#[async_trait]
impl McpConnector for Connector {
    async fn connect(
        &self,
        _: TransportConfig,
        _: ClientOptions,
        _: tokio_util::sync::CancellationToken,
    ) -> Result<Arc<dyn McpConnection>, McpClientError> {
        if self.attempts.fetch_add(1, Ordering::SeqCst) == 0 {
            return Ok(self.first.clone());
        }
        self.started.notify_one();
        self.released.notified().await;
        Ok(self.second.clone())
    }
}

#[tokio::test]
async fn lease_expiry_during_reconnect_prevents_remote_dispatch() {
    let first = Arc::new(Connection {
        reject_before_dispatch: true,
        ..Connection::default()
    });
    let second = Arc::new(Connection::default());
    let connector = Arc::new(Connector {
        attempts: AtomicUsize::new(0),
        first: first.clone(),
        second: second.clone(),
        started: Notify::new(),
        released: Notify::new(),
    });
    let manager = Arc::new(
        McpClientManager::with_ports(
            vec![support::config("docs")],
            Arc::new(support::Secrets::default()),
            connector.clone(),
            Arc::new(support::Clock::default()),
        )
        .unwrap(),
    );
    let tool = manager.contribute(token()).await.unwrap().tools.remove(0);
    let guard = Arc::new(LeaseGuard::default());
    let mut context = support::context();
    context.dispatch_guard = Some(guard.clone());
    let executing = tokio::spawn(async move { tool.execute(context, serde_json::json!({})).await });
    connector.started.notified().await;
    guard.now.store(100, Ordering::SeqCst);
    connector.released.notify_one();
    let result = executing.await.unwrap().unwrap();
    assert!(!result.ok);
    assert_eq!(
        result.model_data.unwrap()["errorCode"],
        "mcp_authority_revoked"
    );
    assert_eq!(first.effects.load(Ordering::SeqCst), 0);
    assert_eq!(second.effects.load(Ordering::SeqCst), 0);
    assert_eq!(connector.attempts.load(Ordering::SeqCst), 2);
    assert_eq!(guard.checks.load(Ordering::SeqCst), 2);
    manager.dispose().await.unwrap();
}
