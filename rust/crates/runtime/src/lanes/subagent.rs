//! The subagent lane: late-bound adapters for delegation, mirroring the
//! coding lane's construction order problem. The executor holds these
//! adapters from construction; `configureSubagents` fills the service.
//! Unconfigured means "no delegation": the contributor offers no tools,
//! the guard passes everything through, the guidance adds nothing, and
//! the settled observer does nothing.

use std::sync::{Arc, Mutex, PoisonError};

use agentkit_governance::{
    ContributedTool, GovernanceResult, ToolContributionContext, ToolContributor, ToolGuard,
    ToolGuardContext, ToolGuardVerdict,
};
use agentkit_host::{
    DelegationGuidance, HostResult, SubagentConfig, SubagentContributor, SubagentRoleGuard,
    SubagentService, SubagentView, TaskSettledObserver,
};
use agentkit_store::TaskRecord;

use crate::error::RuntimeError;

/// The one mutable piece of the subagent lane. Shared by the contributor,
/// the guard, the guidance source, the settled observer, and the native
/// configuration/list/status surface.
pub struct SubagentLane {
    service: Mutex<Option<Arc<SubagentService>>>,
    config: Mutex<SubagentConfig>,
    /// Start-time `maxConcurrency`. The worker pool was sized from it, so a
    /// hot config may lower the limit but never raise it above this.
    concurrency_ceiling: i64,
}

impl SubagentLane {
    pub(crate) fn new(concurrency_ceiling: i64) -> Self {
        let mut config = SubagentConfig::default();
        config.limits.max_concurrency = config.limits.max_concurrency.min(concurrency_ceiling);
        Self {
            service: Mutex::new(None),
            config: Mutex::new(config),
            concurrency_ceiling,
        }
    }

    /// Bind the constructed service (after the host + session service
    /// exist). One-shot: re-binding under running turns would swap
    /// coordination state mid-flight.
    pub(crate) fn bind(&self, service: SubagentService) -> Result<(), RuntimeError> {
        let mut slot = self.service.lock().unwrap_or_else(PoisonError::into_inner);
        if slot.is_some() {
            return Err(RuntimeError::invalid(
                "subagents are already configured on this service",
            ));
        }
        let config = self
            .config
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        service.configure(config);
        *slot = Some(Arc::new(service));
        Ok(())
    }

    /// Hot configuration; applies whether or not binding already happened
    /// (a pre-bind configuration is remembered and applied at bind).
    pub fn configure(&self, mut config: SubagentConfig) {
        if config.limits.max_concurrency > self.concurrency_ceiling {
            eprintln!(
                "agentkit-napi: subagent maxConcurrency {} exceeds the service's start-time ceiling {}; clamped (set subagentMaxConcurrency in the service options)",
                config.limits.max_concurrency, self.concurrency_ceiling
            );
            config.limits.max_concurrency = self.concurrency_ceiling;
        }
        {
            let mut slot = self.config.lock().unwrap_or_else(PoisonError::into_inner);
            *slot = config.clone();
        }
        if let Some(service) = self.service() {
            service.configure(config);
        }
    }

    pub(crate) fn service(&self) -> Option<Arc<SubagentService>> {
        self.service
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    pub fn view(&self) -> Option<serde_json::Value> {
        let service = self.service()?;
        let config = service.config();
        serde_json::to_value(serde_json::json!({
            "enabled": config.enabled,
            "limits": config.limits,
            "roles": config.roles.values().map(|role| serde_json::json!({
                "id": role.id,
                "description": role.description,
                "readOnly": role.read_only,
                "canSpawn": role.can_spawn,
                "model": role.model,
                "capability": role.capability,
            })).collect::<Vec<_>>(),
        }))
        .ok()
    }

    pub(crate) async fn reap_orphans(&self) -> HostResult<usize> {
        match self.service() {
            Some(service) => service.reap_orphans().await,
            None => Ok(0),
        }
    }

    pub async fn list(&self, parent_chat_id: &str) -> HostResult<Vec<SubagentView>> {
        let service = self.service().ok_or_else(|| {
            agentkit_host::HostError::Execution(crate::error::SUBAGENTS_NOT_CONFIGURED.into())
        })?;
        service.list(parent_chat_id).await
    }

    pub async fn status(&self, child_chat_id: &str) -> HostResult<SubagentView> {
        let service = self.service().ok_or_else(|| {
            agentkit_host::HostError::Execution(crate::error::SUBAGENTS_NOT_CONFIGURED.into())
        })?;
        service.status(child_chat_id).await
    }
}

/// Contributor adapter: forwards to the pack while a service exists and
/// the config enables delegation.
pub(crate) struct SubagentLaneContributor {
    lane: Arc<SubagentLane>,
}

impl SubagentLaneContributor {
    pub(crate) fn new(lane: Arc<SubagentLane>) -> Self {
        Self { lane }
    }
}

#[async_trait::async_trait]
impl ToolContributor for SubagentLaneContributor {
    fn namespace(&self) -> &str {
        "subagent"
    }
    async fn contribute(
        &self,
        context: &ToolContributionContext,
    ) -> GovernanceResult<Vec<ContributedTool>> {
        let Some(service) = self.lane.service() else {
            return Ok(vec![]);
        };
        if !service.config().enabled {
            return Ok(vec![]);
        }
        SubagentContributor::new((*service).clone())
            .contribute(context)
            .await
    }
}

/// Guard adapter: role ceilings + workspace lease enforcement, judged by
/// the crate's own guard once a service exists.
pub(crate) struct SubagentLaneGuard {
    lane: Arc<SubagentLane>,
}

impl SubagentLaneGuard {
    pub(crate) fn new(lane: Arc<SubagentLane>) -> Self {
        Self { lane }
    }
}

#[async_trait::async_trait]
impl ToolGuard for SubagentLaneGuard {
    async fn is_visible(&self, context: &ToolGuardContext) -> GovernanceResult<bool> {
        match self.lane.service() {
            Some(service) => {
                SubagentRoleGuard::new((*service).clone())
                    .is_visible(context)
                    .await
            }
            None => Ok(true),
        }
    }
    async fn can_execute(&self, context: &ToolGuardContext) -> GovernanceResult<ToolGuardVerdict> {
        match self.lane.service() {
            Some(service) => {
                SubagentRoleGuard::new((*service).clone())
                    .can_execute(context)
                    .await
            }
            None => Ok(ToolGuardVerdict::Allowed),
        }
    }
}

/// Guidance adapter: delegation instructions while enabled.
#[async_trait::async_trait]
impl DelegationGuidance for SubagentLane {
    async fn guidance(&self, chat_id: &str) -> Option<String> {
        let service = self.service()?;
        service.guidance(chat_id).await
    }
}

/// Settled-observer adapter: orphan reaping on parent settlement.
#[async_trait::async_trait]
impl TaskSettledObserver for SubagentLane {
    async fn observe_settled(&self, task: &TaskRecord) -> HostResult<()> {
        match self.service() {
            Some(service) => service.observe_settled(task).await,
            None => Ok(()),
        }
    }
}
