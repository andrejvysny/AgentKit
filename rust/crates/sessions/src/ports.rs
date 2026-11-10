use async_trait::async_trait;
use serde_json::Value;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

use crate::HostResult;

#[async_trait]
pub trait ChatProviderFactory: Send + Sync {
    fn available(&self) -> bool {
        true
    }
    async fn snapshot(
        &self,
        _provider_id: &str,
        _model: &str,
        _cancel: CancellationToken,
    ) -> HostResult<Option<Value>> {
        Ok(None)
    }
    async fn resolve(
        &self,
        snapshot: &Value,
        cancel: CancellationToken,
    ) -> HostResult<Arc<dyn agentkit_core::ChatProvider>>;
}
