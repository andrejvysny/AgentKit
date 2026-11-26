//! The pack as a host `ToolContributor` (namespace `sandbox`). It offers
//! the six tools only to a run the host attached a sandbox to; whether a
//! call may proceed is the guard's decision, and every body checks the
//! attachment again.

use std::sync::Arc;

use agentkit_core::Toolset;
use agentkit_governance::{
    CodingPermissionAuthority, ContributedTool, GovernanceError, GovernanceResult,
    ToolContributionContext, ToolContributor,
};
use agentkit_workspaces::{SandboxError, WorkspaceService};
use async_trait::async_trait;
use serde_json::{Map, Value, json};

use crate::asks::Asks;
use crate::defs::ToolKind;
use crate::guard::SandboxPermissionGuard;
use crate::ports::{SandboxBinding, SandboxBindingResolver, SandboxExportSink};
use crate::tool::{SandboxTool, Staged};
use crate::{SANDBOX_NAMESPACE, SANDBOX_TOOL_NAMES, SANDBOX_TOOLSET};

const GUIDE: &str = include_str!("../guides/sandbox.md");

/// What the guide and `sandbox_get_status` say about the machine. Fixed for
/// the pack's lifetime, so the guide is session-static and the provider's
/// cached prompt prefix stays intact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxEnvironment {
    /// The image release the host runs, for example `2026.11 (sha256:...)`.
    pub image_version: String,
    /// The operating system of the image.
    pub os: String,
}

impl SandboxEnvironment {
    /// The Debian slim image of the spec (D-18), at `image_version`.
    pub fn new(image_version: impl Into<String>) -> Self {
        Self {
            image_version: image_version.into(),
            os: "Debian GNU/Linux (slim)".to_owned(),
        }
    }
}

/// Everything the pack is built from.
pub struct SandboxPackOptions {
    /// The workspace service every tool goes through.
    pub service: Arc<WorkspaceService>,
    /// The host's pinned attachment of a run.
    pub resolver: Arc<dyn SandboxBindingResolver>,
    /// Where approved exports go.
    pub export_sink: Arc<dyn SandboxExportSink>,
    /// The session's live permission mode, when the host has one; it wins
    /// over the mode pinned with the binding.
    pub authority: Option<Arc<dyn CodingPermissionAuthority>>,
    /// The machine the guide describes.
    pub environment: SandboxEnvironment,
}

/// What the contributor, its tools and its guard share.
pub(crate) struct Shared {
    pub(crate) service: Arc<WorkspaceService>,
    pub(crate) resolver: Arc<dyn SandboxBindingResolver>,
    pub(crate) sink: Arc<dyn SandboxExportSink>,
    pub(crate) authority: Option<Arc<dyn CodingPermissionAuthority>>,
    pub(crate) environment: SandboxEnvironment,
    pub(crate) asks: Asks,
}

/// The sandbox tool pack. Build once per host; register it as a contributor
/// and its [`SandboxToolPack::permission_guard`] among the guards (without
/// the guard nothing asks, so an export outside Full access is refused).
pub struct SandboxToolPack {
    shared: Arc<Shared>,
}

impl SandboxToolPack {
    /// A pack over `options`.
    pub fn new(options: SandboxPackOptions) -> Self {
        Self {
            shared: Arc::new(Shared {
                service: options.service,
                resolver: options.resolver,
                sink: options.export_sink,
                authority: options.authority,
                environment: options.environment,
                asks: Asks::default(),
            }),
        }
    }

    /// The guard of this pack: the mode table and the export and network
    /// approvals. It shares the pack's record of what the owner was shown.
    pub fn permission_guard(&self) -> SandboxPermissionGuard {
        SandboxPermissionGuard::new(self.shared.clone())
    }

    /// The `sandbox` toolset: card, guide (with the session-static
    /// environment) and the six tools.
    pub fn toolset(&self) -> Toolset {
        Toolset::new(
            SANDBOX_TOOLSET,
            "Sandbox",
            "Run commands and keep files in your own Linux sandbox; export results to the user.",
        )
        .with_guide(&guide_source(&self.shared.environment))
        .with_tools(&SANDBOX_TOOL_NAMES)
    }
}

fn guide_source(environment: &SandboxEnvironment) -> String {
    format!(
        "{}\n\nEnvironment\n- Image: {}\n- System: {}; user agent, no root.\n- Working folder: /workspace for an agent, /workspace/chats/<chat id> for a chat. Relative paths resolve there.\n",
        GUIDE.trim_end(),
        environment.image_version,
        environment.os
    )
}

/// The environment a write runs in, bound into the durable execution
/// fingerprint: the run's pinned attachment (workspace key, kind, owner
/// scope, folder) and the agent it acts as (decisions 6 and 14 of ADR
/// 0038). No machine state: the workspace id, generation and image digest
/// change under a parked call (its turn's first command creates the
/// workspace, an idle stop or a recover advances the generation), and an
/// approval answered meanwhile must still hold. What an approval binds
/// beyond this is checked where it is acted on: the export's bytes by
/// their SHA-256, a host by its pending request in the current workspace,
/// an uncertain re-run by the ledger's own generation. Not the mode
/// either: the guard and the body read the live mode at every call.
fn effect_context(binding: &SandboxBinding) -> Value {
    let mut sandbox = Map::new();
    sandbox.insert("workspaceKey".into(), json!(binding.binding.key.as_str()));
    sandbox.insert(
        "kind".into(),
        serde_json::to_value(binding.binding.kind).unwrap_or(Value::Null),
    );
    sandbox.insert("ownerScope".into(), json!(binding.binding.owner_scope));
    let folder = binding
        .binding
        .folder
        .as_ref()
        .map(|f| f.as_str().to_owned());
    sandbox.insert("folder".into(), json!(folder));
    sandbox.insert("agentId".into(), json!(binding.agent_id.as_str()));
    json!({ "sandbox": Value::Object(sandbox) })
}

#[async_trait]
impl ToolContributor for SandboxToolPack {
    fn namespace(&self) -> &str {
        SANDBOX_NAMESPACE
    }

    fn toolsets(&self) -> Vec<Toolset> {
        vec![self.toolset()]
    }

    async fn contribute(
        &self,
        context: &ToolContributionContext,
    ) -> GovernanceResult<Vec<ContributedTool>> {
        let Some(binding) = self.shared.resolver.binding(context).await? else {
            return Ok(vec![]);
        };
        // Read without creating anything (D-36): no record yet is fine.
        match self.shared.service.lookup(&binding.binding).await {
            Ok(_) => {}
            // A stored record of another kind or scope is not this run's.
            Err(SandboxError::NotAttached) => return Ok(vec![]),
            Err(error) => {
                return Err(GovernanceError::Port(format!(
                    "sandbox workspace lookup failed: {error}"
                )));
            }
        }
        let effect = effect_context(&binding);
        let staged = Arc::new(Staged {
            staged: self.shared.asks.staging(),
            shared: self.shared.clone(),
            contribution: context.clone(),
            binding,
        });
        Ok(ToolKind::ALL
            .into_iter()
            .map(|kind| {
                let effect = kind.changes().then(|| effect.clone());
                let tool = Arc::new(SandboxTool::new(kind, staged.clone(), effect));
                if kind.changes() {
                    ContributedTool::DirectWrite(tool)
                } else {
                    ContributedTool::Read(tool)
                }
            })
            .collect())
    }
}
