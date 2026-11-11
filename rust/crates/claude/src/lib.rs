//! Optional driver implementing the AgentKit native contracts for the
//! official Claude Code executable (AGENTKIT-107 scope).
//!
//! This crate owns vendor protocol knowledge only:
//! * [`discovery`] — canonical executable resolution, official auth-status
//!   reading (never credential material), version/profile capability matrix
//!   (AGENTKIT-109).
//! * [`transport`] — owned streaming process transport: typed argv,
//!   immediate readers, bounded shutdown, generation fencing
//!   (AGENTKIT-110).
//! * [`protocol`] — public structured-stream (`--output-format stream-json`)
//!   adapter and event normalization (AGENTKIT-111).
//! * [`launch`] — the two isolated launch profiles (plain Chat; owned-MCP
//!   Chat) and the frozen `configIdentity`.
//! * [`backend`] — the [`NativeBackend`] implementation composing the above.
//!
//! The crate never touches the durable store, leases or approvals; those
//! remain in `agentkit-host`. Nothing here imports the coding toolpack or
//! the private stdio control protocol.

pub mod backend;
pub mod discovery;
pub mod launch;
pub mod protocol;
pub mod transport;

pub use backend::ClaudeCodeBackend;
pub use discovery::{ClaudeLocator, ClaudeSupportMatrix, QualificationEvidence, recheck_replaced};
pub use launch::{
    ChatLaunchProfiles, PRIVATE_RUN_DIR_PREFIX, build_launch_plan, write_per_run_files,
};
pub use protocol::{
    ClaudeStreamAdapter, QUALIFIED_MIN_VERSION, claude_input_frame, claude_terminal_outcome,
};
pub use transport::OwnedProcessSupervisor;
