use crate::{ExecutionContext, HostResult};
use agentkit_core::{CoreError, DispatchFence, DispatchGuard};
use async_trait::async_trait;
use std::sync::Arc;

impl ExecutionContext {
    pub async fn check_dispatch(&self) -> HostResult<()> {
        self.check_cancelled()?;
        if let Err(error) = self
            .store
            .check_dispatch(self.fence.clone(), self.runtime.now_ms())
            .await
        {
            self.cancellation.cancel();
            return Err(error.into());
        }
        self.check_cancelled()
    }

    /// Domain appliers must also enforce this guard's fencing token atomically.
    pub fn dispatch_guard(&self) -> Arc<dyn DispatchGuard> {
        Arc::new(TaskDispatchGuard(self.clone()))
    }
}

struct TaskDispatchGuard(ExecutionContext);
impl std::fmt::Debug for TaskDispatchGuard {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TaskDispatchGuard")
            .field("task_id", &self.0.task.task_id)
            .finish_non_exhaustive()
    }
}
#[async_trait]
impl DispatchGuard for TaskDispatchGuard {
    async fn check(&self) -> Result<(), CoreError> {
        self.0.check_dispatch().await.map_err(|_| {
            CoreError::new(
                "lease_lost",
                "Task execution authority expired or was revoked",
            )
        })
    }
    fn fence(&self) -> DispatchFence {
        DispatchFence {
            task_id: self.0.task.task_id.clone(),
            attempt_id: self.0.attempt_id.clone(),
            fencing_token: self.0.fence.fencing_token,
        }
    }
}
