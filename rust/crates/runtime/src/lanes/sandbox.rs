//! The sandbox lane (ADR 0038, WP-1.7): the ports
//! `AgentRuntimeBuilder::sandbox_lane` accepts so the model loop offers the
//! `sandbox` toolset. The consumer names the concrete backend and
//! repository, as with the native lane; the builder builds the
//! `WorkspaceService` on the runtime's own clock and registers the pack as
//! a contributor and its `SandboxPermissionGuard` among the guards.

use std::sync::Arc;

use agentkit_engine::SharedRuntime;
use agentkit_governance::{CodingPermissionAuthority, ToolContributor, ToolGuard};
use agentkit_toolpacks_sandbox::{
    SandboxBindingResolver, SandboxEnvironment, SandboxExportSink, SandboxPackOptions,
    SandboxToolPack,
};
use agentkit_workspaces::{SandboxBackend, ServiceConfig, WorkspaceRepository, WorkspaceService};

use crate::error::RuntimeError;

/// Everything the sandbox tools need, provided by the consumer at
/// composition time.
pub struct SandboxLane {
    /// The isolated machines (a real backend adapter, or the testkit fake).
    pub backend: Arc<dyn SandboxBackend>,
    /// Workspace records, leases, grants and requests.
    pub repository: Arc<dyn WorkspaceRepository>,
    /// Image, limits, lease and polling settings of the workspace service;
    /// refused at `build()` when [`ServiceConfig::validate`] fails.
    pub config: ServiceConfig,
    /// The host's pinned attachment of a run: no answer, no tools.
    pub resolver: Arc<dyn SandboxBindingResolver>,
    /// Where approved exports go.
    pub export_sink: Arc<dyn SandboxExportSink>,
    /// The session's live permission mode, when the host has one; it wins
    /// over the mode pinned with the binding. `None` judges by the binding.
    pub authority: Option<Arc<dyn CodingPermissionAuthority>>,
    /// The machine the toolset guide describes.
    pub environment: SandboxEnvironment,
}

/// The lane, built: the service the host drives (recovery, quiesce, owner
/// actions) and the pack's contributor and guard.
pub(crate) struct BuiltSandboxLane {
    pub(crate) service: Arc<WorkspaceService>,
    pub(crate) contributor: Arc<dyn ToolContributor>,
    pub(crate) guard: Arc<dyn ToolGuard>,
}

impl SandboxLane {
    /// Build the workspace service on `runtime` (clock and ids) and the
    /// pack over it. Both the pack and its guard are needed: without the
    /// guard nothing asks, so every export outside Full access is refused.
    pub(crate) fn build(self, runtime: SharedRuntime) -> Result<BuiltSandboxLane, RuntimeError> {
        let service = WorkspaceService::new(self.repository, self.backend, runtime, self.config)
            .map_err(|error| RuntimeError::invalid(format!("sandbox lane: {error}")))?;
        let service = Arc::new(service);
        let pack = SandboxToolPack::new(SandboxPackOptions {
            service: Arc::clone(&service),
            resolver: self.resolver,
            export_sink: self.export_sink,
            authority: self.authority,
            environment: self.environment,
        });
        let guard = Arc::new(pack.permission_guard()) as Arc<dyn ToolGuard>;
        Ok(BuiltSandboxLane {
            service,
            contributor: Arc::new(pack),
            guard,
        })
    }
}

/// Startup recovery of the workspace service, before the worker claims a
/// turn: open operations of the previous life become uncertain (or are
/// fenced) and leftover machines are stopped. A backend that cannot prove
/// a stop is not an error: the workspace is fenced (each stop waits up to
/// the call timeout). An error is a repository fault, and it fails the
/// build, as the host's own recovery does: no sandbox call may run before
/// recovery finished.
pub(crate) async fn recover(service: &WorkspaceService) -> Result<(), RuntimeError> {
    service
        .recover_all()
        .await
        .map(|_| ())
        .map_err(|error| RuntimeError::Coded {
            code: error.code().to_owned(),
            message: format!("sandbox recovery failed: {error}"),
        })
}
