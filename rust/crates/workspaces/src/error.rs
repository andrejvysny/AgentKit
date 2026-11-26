//! Errors of the two ports. Both are enums; a `String` field is context for
//! people, never the discriminant.

use crate::ids::{Generation, GrantId, OperationId, RequestId, WorkspaceId};
use crate::lease::LeaseHolder;
use crate::network::RequestState;
use crate::operation::OperationPhase;
use crate::workspace::WorkspaceState;

/// A refusal or fault of [`crate::WorkspaceRepository`]. A refusal changes
/// nothing.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RepositoryError {
    /// No workspace with this id.
    #[error("workspace {0} not found")]
    WorkspaceNotFound(WorkspaceId),
    /// No operation with this id.
    #[error("operation {0} not found")]
    OperationNotFound(OperationId),
    /// No grant with this id.
    #[error("network grant {0} not found")]
    GrantNotFound(GrantId),
    /// No request with this id.
    #[error("network request {0} not found")]
    RequestNotFound(RequestId),
    /// The presented lease token is not current (released, broken, expired,
    /// taken over, or for another workspace).
    #[error("lease on workspace {workspace_id} is lost")]
    LeaseLost {
        /// The workspace the token named.
        workspace_id: WorkspaceId,
    },
    /// A compare-and-swap on the generation, or a write naming a
    /// generation, found another current generation.
    #[error("workspace {workspace_id} is at generation {current}, not {expected}")]
    StaleGeneration {
        /// The workspace.
        workspace_id: WorkspaceId,
        /// What the caller expected.
        expected: Generation,
        /// What is stored.
        current: Generation,
    },
    /// The workspace's state does not allow this write (for example an
    /// operation insert while fenced).
    #[error("workspace {workspace_id} is {state:?}; the write is not allowed")]
    WorkspaceStateConflict {
        /// The workspace.
        workspace_id: WorkspaceId,
        /// Its stored state.
        state: WorkspaceState,
    },
    /// An open (`Dispatching` or `Running`) operation prevents the write.
    #[error("operation {operation_id} is still open in workspace {workspace_id}")]
    OperationOpen {
        /// The workspace.
        workspace_id: WorkspaceId,
        /// The open operation.
        operation_id: OperationId,
    },
    /// An `Uncertain` operation of the workspace's current generation
    /// prevents the write: its machine must be proven stopped and the
    /// generation advanced first.
    #[error(
        "operation {operation_id} of workspace {workspace_id} is uncertain in the current generation"
    )]
    OperationUncertain {
        /// The workspace.
        workspace_id: WorkspaceId,
        /// The uncertain operation.
        operation_id: OperationId,
    },
    /// An operation with this id exists with other identity fields.
    #[error("operation {0} exists with different identity")]
    OperationConflict(OperationId),
    /// The phase change breaks [`OperationPhase::can_transition_to`].
    #[error("operation {operation_id} cannot go from {from:?} to {to:?}")]
    OperationTransition {
        /// The operation.
        operation_id: OperationId,
        /// Stored phase.
        from: OperationPhase,
        /// Requested phase.
        to: OperationPhase,
    },
    /// The request was already decided.
    #[error("network request {request_id} is {state:?}, not pending")]
    RequestNotPending {
        /// The request.
        request_id: RequestId,
        /// Its stored state.
        state: RequestState,
    },
    /// The input breaks a documented precondition (for example a grant for
    /// another workspace).
    #[error("invalid repository input: {reason}")]
    InvalidInput {
        /// What is wrong.
        reason: String,
    },
    /// Storage fault.
    #[error("workspace repository failure: {reason}")]
    Backend {
        /// Adapter message.
        reason: String,
    },
}

/// A refusal or fault of the sandbox: from a backend, the repository or the
/// workspace service. [`SandboxError::code`] is the tool result code.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SandboxError {
    /// The run has no pinned workspace.
    #[error("no sandbox is attached to this run")]
    NotAttached,
    /// The capability exists but a machine cannot start.
    #[error("sandbox unavailable: {reason}")]
    Unavailable {
        /// Why (image missing, runtime down).
        reason: String,
    },
    /// Another operation held the workspace past the wait limit.
    #[error("sandbox is busy: {} holds it", holder.label)]
    Busy {
        /// The holder.
        holder: LeaseHolder,
    },
    /// The workspace is fenced after an unknown outcome.
    #[error("sandbox is fenced: {reason}")]
    Fenced {
        /// Why.
        reason: String,
    },
    /// A connection was refused by policy.
    #[error("connection to {host}:{port} was refused by the network policy")]
    NetworkDenied {
        /// Requested host.
        host: String,
        /// Requested port.
        port: u16,
    },
    /// The disk quota is reached.
    #[error("disk quota reached: {used_bytes} of {quota_bytes} bytes used")]
    QuotaExceeded {
        /// Bytes in use.
        used_bytes: u64,
        /// Quota.
        quota_bytes: u64,
    },
    /// A path resolves outside the workspace.
    #[error("{path} is outside the workspace")]
    PathOutsideWorkspace {
        /// The path as given.
        path: String,
    },
    /// A file is over the limit for the request.
    #[error("file is {size} bytes, more than the {limit}-byte limit")]
    FileTooLarge {
        /// File size.
        size: u64,
        /// Limit.
        limit: u64,
    },
    /// The path is a directory, link or special file.
    #[error("{path} is not a regular file")]
    NotARegularFile {
        /// The path.
        path: String,
    },
    /// The operation id is known with another argument hash.
    #[error("operation {0} was already dispatched with different arguments")]
    OperationConflict(OperationId),
    /// The request names a generation older than the current one (INV-8).
    #[error("generation {requested} is stale; current is {current}")]
    StaleGeneration {
        /// Generation in the request.
        requested: Generation,
        /// Generation the backend serves.
        current: Generation,
    },
    /// A file, machine or record does not exist.
    #[error("{what} not found")]
    NotFound {
        /// What was looked for.
        what: String,
    },
    /// The caller cancelled before the operation was dispatched: nothing
    /// started.
    #[error("cancelled before the operation started")]
    Cancelled,
    /// Backend fault.
    #[error("sandbox backend failure: {reason}")]
    Backend {
        /// Adapter message.
        reason: String,
    },
    /// Repository refusal or fault.
    #[error(transparent)]
    Repository(#[from] RepositoryError),
}

impl SandboxError {
    /// The tool result code. The first nine are the spec's result envelope
    /// table; the `sandbox_*` codes after them are new and still need rows
    /// in the failure table.
    pub fn code(&self) -> &'static str {
        match self {
            SandboxError::NotAttached => "sandbox_not_attached",
            SandboxError::Unavailable { .. } => "sandbox_unavailable",
            SandboxError::Busy { .. } => "sandbox_busy",
            SandboxError::Fenced { .. } => "sandbox_fenced",
            SandboxError::NetworkDenied { .. } => "network_denied",
            SandboxError::QuotaExceeded { .. } => "quota_exceeded",
            SandboxError::PathOutsideWorkspace { .. } => "path_outside_workspace",
            SandboxError::FileTooLarge { .. } => "file_too_large",
            SandboxError::NotARegularFile { .. } => "not_a_regular_file",
            SandboxError::OperationConflict(_) => "sandbox_operation_conflict",
            SandboxError::StaleGeneration { .. } => "sandbox_stale_generation",
            SandboxError::NotFound { .. } => "sandbox_not_found",
            SandboxError::Cancelled => "sandbox_cancelled",
            SandboxError::Backend { .. } => "sandbox_backend_failed",
            SandboxError::Repository(_) => "sandbox_store_failed",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_follow_the_result_envelope_table() {
        let holder = LeaseHolder {
            agent_id: crate::AgentId::new("a1"),
            run_id: "r1".into(),
            label: "Researcher".into(),
            host_id: "host-1".into(),
            boot_id: "boot-1".into(),
        };
        let busy = SandboxError::Busy { holder };
        assert_eq!(busy.code(), "sandbox_busy");
        assert_eq!(busy.to_string(), "sandbox is busy: Researcher holds it");
        assert_eq!(SandboxError::NotAttached.code(), "sandbox_not_attached");
        let denied = SandboxError::NetworkDenied {
            host: "example.com".into(),
            port: 443,
        };
        assert_eq!(denied.code(), "network_denied");
        let lost = RepositoryError::LeaseLost {
            workspace_id: WorkspaceId::new("w1"),
        };
        assert_eq!(SandboxError::from(lost).code(), "sandbox_store_failed");
    }
}
