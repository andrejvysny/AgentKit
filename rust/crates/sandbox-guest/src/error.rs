//! Crate-internal failure type. Every guest failure carries a stable code
//! the worker maps into `SandboxErrorCode`; messages are static text plus
//! bounded ids — never file contents or environment values (S18).

use agentkit_sandbox_contracts::error::{SandboxError, SandboxErrorCode};

/// Stable guest-side codes (`GuestOpOutcome::Failed.code`).
pub mod code {
    pub const PATH_ESCAPE: &str = "path_escape";
    pub const DIGEST_MISMATCH: &str = "digest_mismatch";
    pub const NOT_FOUND: &str = "not_found";
    pub const NOT_A_FILE: &str = "not_a_file";
    pub const NOT_A_DIR: &str = "not_a_dir";
    pub const BODY_TOO_LARGE: &str = "body_too_large";
    pub const BAD_BASE64: &str = "bad_base64";
    pub const BAD_INPUT: &str = "bad_input";
    pub const IO: &str = "io_error";
    pub const TIMEOUT: &str = "timeout";
    pub const ROOT_DENIED: &str = "root_denied";
    pub const BAD_EXECUTABLE: &str = "bad_executable";
    pub const NOT_STARTED: &str = "not_started";
    pub fn unsupported() -> &'static str {
        "sandbox_capability_unsupported"
    }
}

/// A guest operation failure ready to be turned into `GuestOpOutcome::Failed`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{code}: {message}")]
pub struct GuestFail {
    pub code: &'static str,
    pub message: String,
}

impl GuestFail {
    #[must_use]
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
    /// errno-shaped io failure with a bounded message.
    #[must_use]
    pub fn io(op: &str, err: &std::io::Error) -> Self {
        Self::new(
            code::IO,
            format!("{op}: {} (errno {:?})", err.kind(), err.raw_os_error()),
        )
    }
    #[must_use]
    pub fn path_escape(what: &str) -> Self {
        Self::new(code::PATH_ESCAPE, format!("rejected: {what}"))
    }

    /// Map into the public error contract for service-layer use.
    #[must_use]
    pub fn to_sandbox_error(&self) -> SandboxError {
        let c = match self.code {
            code::PATH_ESCAPE => SandboxErrorCode::PolicyDenied,
            code::DIGEST_MISMATCH => SandboxErrorCode::ArtifactInvalid,
            code::BODY_TOO_LARGE | code::BAD_BASE64 | code::BAD_INPUT => {
                SandboxErrorCode::ArtifactInvalid
            }
            code::NOT_STARTED => SandboxErrorCode::ExecutionNotStarted,
            code::TIMEOUT => SandboxErrorCode::Unavailable,
            code::NOT_FOUND => SandboxErrorCode::NotFound,
            _ if self.code == code::unsupported() => SandboxErrorCode::CapabilityUnsupported,
            _ => SandboxErrorCode::Unavailable,
        };
        SandboxError::new(c, format!("{}: {}", self.code, self.message))
    }
}

impl From<std::io::Error> for GuestFail {
    fn from(e: std::io::Error) -> Self {
        Self::io("io", &e)
    }
}
