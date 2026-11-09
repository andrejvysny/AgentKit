use agentkit_core::{CoreError, DispatchFence, DispatchGuard};
use agentkit_governance::{ApprovalAsk, ToolGuard, ToolGuardContext, ToolGuardVerdict};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// Carries the admitted decision through a tool's asynchronous lease wait.
/// Rechecking never consumes another one-use answer or opens a new approval.
pub(super) struct ExecutionRecheck {
    pub original: Option<Arc<dyn DispatchGuard>>,
    pub guards: Vec<Arc<dyn ToolGuard>>,
    pub snapshot: ToolGuardContext,
    pub approvals: Vec<ApprovalAsk>,
    pub standing_digest: Option<String>,
    pub cancellation: CancellationToken,
}

impl std::fmt::Debug for ExecutionRecheck {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExecutionRecheck")
            .field("run_id", &self.snapshot.contribution.run_id)
            .finish_non_exhaustive()
    }
}

fn refused(reason: &str) -> CoreError {
    CoreError::new("authorization_changed", reason)
}

#[async_trait::async_trait]
impl DispatchGuard for ExecutionRecheck {
    async fn check(&self) -> Result<(), CoreError> {
        if let Some(original) = &self.original {
            original.check().await?;
        }
        self.check_permissions().await?;
        self.check_standing_grant().await?;
        if self.cancellation.is_cancelled()
            || self.snapshot.contribution.cancellation.is_cancelled()
        {
            return Err(CoreError::new("cancelled", "command authority cancelled"));
        }
        // Store-backed grant and permission reads may outlast the task lease.
        if let Some(original) = &self.original {
            original.check().await?;
        }
        Ok(())
    }

    fn fence(&self) -> DispatchFence {
        self.original
            .as_ref()
            .map(|guard| guard.fence())
            .unwrap_or_else(|| DispatchFence {
                task_id: self.snapshot.contribution.run_id.clone(),
                attempt_id: String::new(),
                fencing_token: 0,
            })
    }
}

impl ExecutionRecheck {
    async fn check_permissions(&self) -> Result<(), CoreError> {
        for guard in &self.guards {
            let verdict = tokio::select! { biased;
                _ = self.cancellation.cancelled() => return Err(CoreError::new("cancelled", "command authority cancelled")),
                result = tokio::time::timeout(super::deadline(&self.snapshot.contribution), guard.can_execute(&self.snapshot)) => result,
            };
            match verdict {
                Ok(Ok(ToolGuardVerdict::Allowed)) => {}
                Ok(Ok(ToolGuardVerdict::AwaitingHuman(ask)))
                    if self.approvals.iter().any(|old| {
                        old.reason == ask.reason
                            && old.preview == ask.preview
                            && old.requested_mode == ask.requested_mode
                            && old.uncertainty_generation == ask.uncertainty_generation
                    }) => {}
                Ok(Ok(ToolGuardVerdict::Refused(reason))) => return Err(refused(&reason)),
                _ => return Err(refused("permissions changed; fresh approval required")),
            }
        }
        Ok(())
    }

    async fn check_standing_grant(&self) -> Result<(), CoreError> {
        let Some(expected) = &self.standing_digest else {
            return Ok(());
        };
        let durable = self
            .snapshot
            .contribution
            .durable
            .as_ref()
            .ok_or_else(|| refused("standing approval authority unavailable"))?;
        let policy = durable
            .standing_policy()
            .ok_or_else(|| refused("standing approval policy unavailable"))?;
        let reasons = self
            .approvals
            .iter()
            .map(|ask| ask.reason.clone())
            .collect::<Vec<_>>();
        let current = policy
            .key_for(&self.snapshot, &reasons)
            .await
            .map_err(|_| refused("standing approval identity unavailable"))?;
        if current.as_ref().map(|(digest, _)| digest) != Some(expected)
            || !durable.has_active_standing_grant(expected).await?
        {
            return Err(refused(
                "standing approval changed or revoked; fresh approval required",
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "execution_recheck_tests.rs"]
mod tests;
