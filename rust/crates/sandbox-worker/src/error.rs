//! Worker-local error type with stable codes, mapped into the frozen
//! contracts error contract (`agentkit_sandbox_contracts::error`).

use agentkit_sandbox_contracts::error::{SandboxError, SandboxErrorCode};
use agentkit_sandbox_contracts::protocol::ProtocolError;

/// Worker failure surface. Every variant carries enough stable information
/// to map into a [`SandboxErrorCode`]; nothing here is `anyhow`-style opaque.
#[derive(Debug, thiserror::Error)]
pub enum WorkerError {
    /// Structured contract error passed through unchanged.
    #[error(transparent)]
    Contract(SandboxError),
    /// Framing / protocol violation (terminates the offending connection or
    /// fails the request with `sandbox_protocol_incompatible`).
    #[error("protocol violation: {0}")]
    Protocol(String),
    /// Journal integrity failure (torn mid-file line, digest chain break).
    #[error("journal corrupt: {0}")]
    JournalCorrupt(String),
    /// Journal/storage I/O failure.
    #[error("storage i/o failure: {0}")]
    Storage(String),
    /// Guest channel failure (link closed, guest silent).
    #[error("guest link failure: {0}")]
    GuestLink(String),
    /// Guest reported a stable failure code that mapped to this contract code.
    #[error("guest failure [{code}]: {message}")]
    GuestFailure {
        code: String,
        message: String,
        mapped: SandboxErrorCode,
    },
    /// Backend (hypervisor launcher) failure.
    #[error("backend failure: {0}")]
    Backend(String),
    /// Internal invariant violation (e.g. illegal state transition attempt);
    /// never silently absorbed.
    #[error("internal invariant violated: {0}")]
    Internal(String),
}

impl WorkerError {
    /// Map into the frozen wire error contract. Messages are log-safe
    /// (ids/static text only; guest content is capped + redacted upstream).
    #[must_use]
    pub fn to_sandbox_error(&self) -> SandboxError {
        match self {
            Self::Contract(e) => e.clone(),
            Self::Protocol(m) => {
                SandboxError::new(SandboxErrorCode::ProtocolIncompatible, m.clone())
            }
            Self::JournalCorrupt(m) => {
                let mut e = SandboxError::new(
                    SandboxErrorCode::Unavailable,
                    "worker journal integrity failure",
                );
                e.detail = Some(m.clone());
                e
            }
            Self::Storage(m) => {
                let mut e =
                    SandboxError::new(SandboxErrorCode::Unavailable, "worker storage failure");
                e.detail = Some(m.clone());
                e
            }
            Self::GuestLink(m) => {
                let mut e =
                    SandboxError::new(SandboxErrorCode::ExecutionUncertain, "guest link lost");
                e.detail = Some(m.clone());
                e
            }
            Self::GuestFailure {
                mapped, message, ..
            } => SandboxError::new(*mapped, message.clone()),
            Self::Backend(m) => {
                SandboxError::new(SandboxErrorCode::IsolationUnavailable, m.clone())
            }
            Self::Internal(m) => {
                let mut e = SandboxError::new(
                    SandboxErrorCode::PolicyDenied,
                    "illegal worker state operation",
                );
                e.detail = Some(m.clone());
                e
            }
        }
    }
}

impl From<SandboxError> for WorkerError {
    fn from(e: SandboxError) -> Self {
        Self::Contract(e)
    }
}

impl From<ProtocolError> for WorkerError {
    fn from(e: ProtocolError) -> Self {
        Self::Protocol(e.to_string())
    }
}

impl From<std::io::Error> for WorkerError {
    fn from(e: std::io::Error) -> Self {
        Self::Storage(e.to_string())
    }
}

impl From<serde_json::Error> for WorkerError {
    fn from(e: serde_json::Error) -> Self {
        Self::Protocol(format!("json: {}", e))
    }
}

pub type WorkerResult<T> = Result<T, WorkerError>;

/// Cap any untrusted (guest-supplied) string for audit/log embedding (S18).
#[must_use]
pub fn redact_cap(s: &str, max: usize) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(max.min(s.len()));
    for b in s.bytes().take(max) {
        if b.is_ascii_graphic() || b == b' ' {
            out.push(b as char);
        } else {
            out.push('?');
        }
    }
    if s.len() > max {
        out.push('~');
        let mut h = sha2::Sha256::new();
        use sha2::Digest;
        h.update(s.as_bytes());
        for b in &h.finalize()[..4] {
            out.push(HEX[(b >> 4) as usize] as char);
            out.push(HEX[(b & 0xf) as usize] as char);
        }
    }
    out
}
