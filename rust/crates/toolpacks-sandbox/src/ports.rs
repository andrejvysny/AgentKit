//! The two host ports of the pack: where a run's sandbox comes from, and
//! where an approved export goes. OneMind implements both; tests use fakes.

use agentkit_governance::{GovernanceResult, ToolContributionContext};
use agentkit_workspaces::{AgentId, Binding, ExportFile, WorkspaceKey};
use async_trait::async_trait;

use crate::mode::SandboxMode;

/// A run's sandbox attachment, pinned by the host at admission (D-24). The
/// model never chooses it: no tool takes a workspace argument (SBX-11).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxBinding {
    /// Which workspace and which folder in it.
    pub binding: Binding,
    /// The agent the run acts as (attribution, membership, lease holder).
    pub agent_id: AgentId,
    /// The run's permission mode in the sandbox (a chat turn is
    /// [`SandboxMode::AskBeforeChanges`]). A `CodingPermissionAuthority`
    /// given to the pack overrides it with the session's live mode.
    pub mode: SandboxMode,
}

/// The host's answer to "which sandbox is this run attached to". Asked when
/// the tools are staged (no answer, no tools) and again by every tool body,
/// which refuses `sandbox_not_attached` when the answer is gone or changed.
/// An `Err` fails closed: the tools are not staged, a body refuses.
#[async_trait]
pub trait SandboxBindingResolver: Send + Sync {
    /// The pinned attachment of the run `context` stages, if any.
    async fn binding(
        &self,
        context: &ToolContributionContext,
    ) -> GovernanceResult<Option<SandboxBinding>>;
}

/// Where an approved export comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportContext {
    /// The durable `execution_id` of the export call; the same id means the
    /// same export, so a host can deduplicate.
    pub execution_id: String,
    /// The run that exported.
    pub run_id: String,
    /// The chat of the run, when it has one.
    pub chat_id: Option<String>,
    /// The model's tool call.
    pub tool_call_id: Option<String>,
    /// The agent that exported.
    pub agent_id: AgentId,
    /// The workspace it came from.
    pub workspace_key: WorkspaceKey,
    /// The file inside the guest, for example `/workspace/chats/c1/out.csv`.
    pub path: String,
}

/// What the host made of an export.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportKind {
    /// A file the user can download.
    File,
    /// An artifact shown in the product.
    Artifact,
}

impl ExportKind {
    /// Wire name.
    pub fn as_str(self) -> &'static str {
        match self {
            ExportKind::File => "file",
            ExportKind::Artifact => "artifact",
        }
    }
}

/// The host's receipt of an export.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportReceipt {
    /// The host's id of the stored file or artifact.
    pub file_id: String,
    /// What it became.
    pub kind: ExportKind,
}

/// Why the host did not take an export.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ExportSinkError {
    /// The host refuses this export (type, size, policy).
    #[error("export refused: {reason}")]
    Refused {
        /// Why, for the model.
        reason: String,
    },
    /// The host could not store it.
    #[error("export failed: {reason}")]
    Failed {
        /// Why, for the model.
        reason: String,
    },
}

impl ExportSinkError {
    /// The tool result code.
    pub fn code(&self) -> &'static str {
        match self {
            ExportSinkError::Refused { .. } => "export_refused",
            ExportSinkError::Failed { .. } => "export_failed",
        }
    }
}

/// The host side of `sandbox_export_file`: receives the exact bytes the
/// owner approved (or, in Full access, the bytes read), once per admitted
/// call.
#[async_trait]
pub trait SandboxExportSink: Send + Sync {
    /// Store `file` for the user under `title`.
    async fn export(
        &self,
        context: &ExportContext,
        file: ExportFile,
        title: &str,
    ) -> Result<ExportReceipt, ExportSinkError>;
}
