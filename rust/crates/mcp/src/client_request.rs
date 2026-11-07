use rmcp::{RoleClient, service::RequestHandle};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
pub(crate) struct WireDispatch {
    guard: Option<Arc<dyn agentkit_core::DispatchGuard>>,
    cancellation: CancellationToken,
    denied: Arc<AtomicBool>,
}
impl WireDispatch {
    pub(crate) fn new(
        guard: Option<Arc<dyn agentkit_core::DispatchGuard>>,
        cancellation: CancellationToken,
    ) -> Self {
        Self {
            guard,
            cancellation,
            denied: Arc::new(AtomicBool::new(false)),
        }
    }
    pub(crate) fn denied(&self) -> bool {
        self.denied.load(Ordering::Acquire)
    }
    pub(crate) async fn check(&self) -> Result<(), crate::McpClientError> {
        if self.cancellation.is_cancelled() {
            return Err(crate::McpClientError::Cancelled);
        }
        if let Some(guard) = &self.guard
            && guard.check().await.is_err()
        {
            self.denied.store(true, Ordering::Release);
            return Err(crate::McpClientError::Authority);
        }
        if self.cancellation.is_cancelled() {
            return Err(crate::McpClientError::Cancelled);
        }
        Ok(())
    }
}

/// Cancellation also runs when an outer manager deadline drops the call future.
pub(crate) struct Pending(Option<RequestHandle<RoleClient>>);

impl Pending {
    pub(crate) fn new(handle: RequestHandle<RoleClient>) -> Self {
        Self(Some(handle))
    }
    pub(crate) fn handle(&mut self) -> &mut RequestHandle<RoleClient> {
        self.0.as_mut().expect("live pending MCP request")
    }
    pub(crate) fn disarm(&mut self) {
        self.0.take();
    }
    pub(crate) async fn cancel(&mut self) {
        if let Some(handle) = self.0.take() {
            cancel(handle).await;
        }
    }
}

async fn cancel(handle: RequestHandle<RoleClient>) {
    let _ = tokio::time::timeout(
        Duration::from_secs(1),
        handle.cancel(Some("AgentKit request abandoned".into())),
    )
    .await;
}

impl Drop for Pending {
    fn drop(&mut self) {
        if let Some(handle) = self.0.take()
            && let Ok(runtime) = tokio::runtime::Handle::try_current()
        {
            runtime.spawn(cancel(handle));
        }
    }
}
