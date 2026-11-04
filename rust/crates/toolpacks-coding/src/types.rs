//! Pack configuration surface. Mirrors `packages/toolpacks-coding/src/types.ts`.

use std::sync::Arc;

use crate::coordination::WorkspaceCoordinator;
use crate::limits::{CodingLimits, CodingLimitsOverride};

#[derive(Clone, Debug)]
pub struct CodingWorkspace {
    /// Absolute path of the workspace root.
    pub root: String,
}

/// The mode/role/authority surface is the PORTABLE shared authority —
/// `agentkit-core::permission` holds the single decision matrix, and the
/// host owns the session-policy trait. This crate consumes them; it must
/// never re-define or re-branch on the matrix (a JS tool and a native tool
/// that diverge in verdict is a bug, not per-surface policy).
pub use agentkit_core::permission::{CodingMode, CodingRole};
pub use agentkit_host::{CodingPermissionAuthority, PermissionContext};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum SandboxMode {
    /// Default: shell tools refuse to run without the sandbox.
    #[default]
    Required,
    Off,
}

/// Milliseconds since the Unix epoch.
pub type Clock = Arc<dyn Fn() -> i64 + Send + Sync + 'static>;

pub fn system_clock() -> Clock {
    Arc::new(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|since| since.as_millis() as i64)
            .unwrap_or(0)
    })
}

#[derive(Clone)]
pub struct CodingToolPackOptions {
    pub workspace: CodingWorkspace,
    /// Default `Required`: shell tools refuse to run without the sandbox.
    pub sandbox: SandboxMode,
    /// Default false: the sandbox denies network.
    pub network: bool,
    /// Exact command strings `run_tests` may run, e.g. `["bun test"]`.
    pub test_commands: Vec<String>,
    /// Default false: the sandbox denies every write under `<root>/.git`, so
    /// a command cannot plant config, hooks or refs that later run
    /// unsandboxed. When true, git state is writable but `.git/hooks` and
    /// `.git/config` stay denied.
    pub allow_git_writes: bool,
    /// Default false: credential-like paths are refused for read and write.
    pub allow_sensitive: bool,
    pub limits: CodingLimitsOverride,
    pub clock: Option<Clock>,
    /// Test seam: pretend to run on another platform. Default host platform.
    pub platform: Option<String>,
    /// Test seam: path of the sandbox executable. Default
    /// `/usr/bin/sandbox-exec`.
    pub sandbox_exec_path: Option<String>,
    /// Host authority for the per-session mode/role/plan grant. When wired,
    /// the pack also contributes `submit_plan`, the only way out of plan mode.
    pub permission: Option<Arc<dyn CodingPermissionAuthority>>,
    /// Serialises file mutations and write-capable commands. Default: one
    /// process-global in-process coordinator shared by every pack.
    pub coordinator: Option<Arc<dyn WorkspaceCoordinator>>,
    /// Longest a file mutation or command waits for a lease held by someone
    /// else before failing with `workspace_busy`. Default
    /// `min(20_000, floor(tool_timeout_ms * 0.66))`; must be positive and
    /// below the `tool_timeout_ms` limit so the wait ends before the tool is
    /// timed out.
    pub coordination_wait_ms: Option<u64>,
}

impl Default for CodingToolPackOptions {
    fn default() -> Self {
        Self {
            workspace: CodingWorkspace {
                root: ".".to_owned(),
            },
            sandbox: SandboxMode::Required,
            network: false,
            test_commands: vec![],
            allow_git_writes: false,
            allow_sensitive: false,
            limits: CodingLimitsOverride::default(),
            clock: None,
            platform: None,
            sandbox_exec_path: None,
            permission: None,
            coordinator: None,
            coordination_wait_ms: None,
        }
    }
}

impl CodingToolPackOptions {
    pub fn resolved_limits(&self) -> CodingLimits {
        self.limits.resolve()
    }
}
