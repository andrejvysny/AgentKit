//! Versioned native-execution contracts for optional vendor agent backends.
//!
//! This crate is the shared vocabulary between `agentkit-host`'s generic
//! `NativeAgentExecutor`, injected backend drivers (the optional Claude Code
//! driver lives in `agentkit-claude`), and consumers. It contains NO process
//! logic, NO vendor protocol, NO store access and NO dependency on the host:
//! the durable authority (tasks, attempts, leases, events, receipts) stays in
//! `agentkit-host` / `agentkit-store` (AGENTKIT-107 §3.1).
//!
//! Contract invariants (AGENTKIT-108):
//! * An [`ExecutionTarget`] distinguishes the existing model-provider loop
//!   from a native runtime, survives serialization with a schema version, and
//!   is frozen per admitted turn. `ChatProvider` semantics are untouched.
//! * No contract type carries vendor secrets or raw credential material.
//! * Unsupported/unknown features are explicit ([`CapabilityState`]); nothing
//!   is advertised as supported without qualification evidence.
//! * Vendor tool observations are data
//!   ([`NativeEvent::ToolObservation`]) and are never dispatchable host tool
//!   requests; only the governed MCP bridge may execute application tools.
//! * Terminal outcome, process exit and pipe EOF are three distinct signals
//!   ([`NativeTerminalOutcome`] vs [`ProcessExit`] vs EOF on the line stream).

mod canonical;
mod capability;
mod error;
mod events;
mod interaction;
mod ports;
mod target;

pub use canonical::canonical_digest;
pub use capability::{
    CapabilityMatrix, CapabilitySnapshot, CapabilityState, CapabilityViolation,
    validate_execution_target, well_known_flags,
};
pub use error::{NativeError, NativeRefusal, NativeResult};
pub use events::{
    BillingInclusion, FailureClass, McpServerStatus, NativeEvent, NativeTerminalOutcome,
    NativeUsage, ProcessTerminalEvidence, TokenSet, UsageProvenance, VendorToolPhase,
};
pub use interaction::{
    AnswerDisposition, AnswerSubmission, Decision, InteractionKind, InteractionRejection,
    PendingInteraction,
};
pub use ports::{
    AuthEvidence, AuthMode, ContextBlock, ExecutableEvidence, ExecutableLocator, FinishDisposition,
    FrameLine, InterruptPolicy, InterruptReceipt, LaunchPlan, McpEndpointSpec, NativeBackend,
    OwnedChildHandle, PermissionPromptPolicy, PolicyState, PreflightReport, ProcessExit,
    ProcessSupervisor, ProtocolViolation, ResourceLimits, STDERR_TAIL_MAX_BYTES,
    StreamFrameAdapter, TransportFault, TurnInput, TurnResume, bounded_stderr_tail, collect_frames,
    fingerprint, with_stderr_suffix,
};
pub use target::{
    AuthExpectation, CONTENT_POLICY_VERSION, CONTENT_SCHEMA_VERSION, ContentPolicy,
    EXECUTION_TARGET_SCHEMA_VERSION, ExecutionTarget, ModelLoopTarget, NativeExecutionTarget,
    NativeProfile,
};
