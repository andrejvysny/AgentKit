//! Sandbox tool pack (ADR 0038, layer 4): the `sandbox` toolset that lets a
//! model run commands and move files inside an isolated workspace.
//!
//! [`SandboxToolPack`] contributes six deferred tools under namespace and
//! toolset `sandbox` (`sandbox_run`, `sandbox_read_file`,
//! `sandbox_write_file`, `sandbox_list_files`, `sandbox_export_file`,
//! `sandbox_get_status`), and only to a run the host pinned a workspace to
//! ([`SandboxBindingResolver`], SBX-11): no tool takes a workspace argument.
//! [`SandboxPermissionGuard`] applies the sandbox mode table (D-13, D-29)
//! and raises the two boundary approvals: an export, bound to the exact
//! content the human saw (F-14), and new network hosts, bound to the exact
//! requests shown. Exports leave through the host's [`SandboxExportSink`].
//!
//! Mutating tools run only under the durable execution ledger: the body
//! takes the operation id from the admitted `execution_id`
//! ([`agentkit_governance::EXECUTION_ID_METADATA_KEY`], INV-2) and refuses
//! without one. The pack reaches workspaces only through
//! `agentkit-workspaces` and never names the store or a concrete backend.

mod asks;
mod card;
mod defs;
mod envelope;
mod export_tool;
mod guard;
mod mode;
mod mutate;
mod pack;
mod paths;
mod ports;
mod read_tools;
mod render;
mod run_tool;
mod tool;

pub use guard::SandboxPermissionGuard;
pub use mode::SandboxMode;
pub use pack::{SandboxEnvironment, SandboxPackOptions, SandboxToolPack};
pub use ports::{
    ExportContext, ExportKind, ExportReceipt, ExportSinkError, SandboxBinding,
    SandboxBindingResolver, SandboxExportSink,
};

/// The namespace the pack contributes under; the guard judges only it.
pub const SANDBOX_NAMESPACE: &str = "sandbox";
/// The toolset id of the six tools.
pub const SANDBOX_TOOLSET: &str = "sandbox";
/// The six tools, in toolset order.
pub const SANDBOX_TOOL_NAMES: [&str; 6] = [
    defs::RUN,
    defs::READ_FILE,
    defs::WRITE_FILE,
    defs::LIST_FILES,
    defs::EXPORT_FILE,
    defs::GET_STATUS,
];
