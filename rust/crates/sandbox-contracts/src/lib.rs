//! AgentKit VM sandbox runtime contracts.
//!
//! Single source of truth for the semantics of the sandbox runtime, shared by
//! the trusted AgentKit Host, the Sandbox Worker daemon, and the guest agent.
//! This crate is deliberately dependency-light and contains no I/O: it defines
//! identities, the sandbox specification, effective policy, the lifecycle
//! state machine, the error contract, workspace grants/path validation, hard
//! resource/protocol limits, and the bounded host<->worker and worker<->guest
//! wire protocols.
//!
//! Normative source: `docs/specs/sandbox-runtime.md` (S01-S20 invariants,
//! §5 identities, §6 protocols, §7 state machine, §6.5 error contract).
//! Nothing in this crate performs VM, filesystem, or network operations.

pub mod api;
pub mod canonical;
pub mod error;
pub mod grant;
pub mod ids;
pub mod limits;
pub mod path;
pub mod policy;
pub mod protocol;
pub mod session;
pub mod spec;
pub mod state;

/// Wire protocol version negotiated on every host<->worker and worker<->guest
/// association. A mismatch fails closed (`sandbox_protocol_incompatible`);
/// there is no best-effort downgrade (S16).
pub const PROTOCOL_VERSION: u32 = 1;

/// Accepted `version` value of a `SandboxSpec` (§5.2).
pub const SPEC_VERSION: u32 = 1;

/// Minimum accepted `instance_generation` (generations start at 1; 0 is
/// invalid so "unset" can never alias a real generation).
pub const MIN_GENERATION: u64 = 1;

/// The architecture this build runs on, normalized to the image `arch`
/// vocabulary (`x86_64`/`aarch64`). Unsupported architectures map to an
/// empty string so image validation fails closed.
#[must_use]
pub fn host_arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "x86_64",
        "aarch64" => "aarch64",
        _ => "",
    }
}
