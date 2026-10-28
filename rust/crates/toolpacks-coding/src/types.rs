//! Pack configuration surface. Mirrors `packages/toolpacks-coding/src/types.ts`.

use std::sync::Arc;

use async_trait::async_trait;

use crate::coordination::WorkspaceCoordinator;
use crate::limits::{CodingLimits, CodingLimitsOverride};

#[derive(Clone, Debug)]
pub struct CodingWorkspace {
    /// Absolute path of the workspace root.
    pub root: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CodingMode {
    ReadOnly,
    Ask,
    AutoEdit,
    FullAuto,
    /// Reads only. Writes and commands are refused FINAL (no per-call
    /// approval): the only escalation is a `submit_plan` the human approves,
    /// which the host records as a durable grant before anything may change.
    Plan,
}

impl CodingMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::ReadOnly => "read_only",
            Self::Ask => "ask",
            Self::AutoEdit => "auto_edit",
            Self::FullAuto => "full_auto",
            Self::Plan => "plan",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "read_only" => Self::ReadOnly,
            "ask" => Self::Ask,
            "auto_edit" => Self::AutoEdit,
            "full_auto" => Self::FullAuto,
            "plan" => Self::Plan,
            _ => return None,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CodingRole {
    Implementer,
    Reviewer,
    Supervisor,
}

impl CodingRole {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Implementer => "implementer",
            Self::Reviewer => "reviewer",
            Self::Supervisor => "supervisor",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "implementer" => Self::Implementer,
            "reviewer" => Self::Reviewer,
            "supervisor" => Self::Supervisor,
            _ => return None,
        })
    }
}

/// Context passed to the permission authority; mirrors `{chatId?, runId?}`.
#[derive(Clone, Debug, Default)]
pub struct PermissionContext {
    pub chat_id: Option<String>,
    pub run_id: Option<String>,
}

/// The host's answer to "what is this session allowed to do RIGHT NOW".
/// Optional: without one the guard uses the mode it was constructed with,
/// which is what a single-mode CLI does.
///
/// The host owns it deliberately — a permission mode the model could set is
/// not a permission mode.
/// Host-supplied session policy source. The TypeScript port lets these
/// hooks reject (`roleOf`/`modeOf`/`planApproved` throw), and the host
/// staging then FAILS CLOSED: a guard that could not answer has not allowed
/// anything. The Rust port keeps that contract — an implementation that
/// cannot answer returns `Err`, the guard propagates it, and host staging
/// turns the failed guard call into a `guard error` refusal. Silently
/// returning `None` means "no opinion" (use the configured mode/role),
/// never "the lookup failed".
#[async_trait]
pub trait CodingPermissionAuthority: Send + Sync {
    /// The session's effective mode; `None` means "use the configured mode".
    async fn mode_of(
        &self,
        _ctx: &PermissionContext,
    ) -> agentkit_host::HostResult<Option<CodingMode>> {
        Ok(None)
    }
    /// The role a session runs under; `None` means "the configured role".
    async fn role_of(
        &self,
        _ctx: &PermissionContext,
    ) -> agentkit_host::HostResult<Option<CodingRole>> {
        Ok(None)
    }
    /// True when THIS plan was approved by a human for this session.
    /// `None` means the host never wired a plan authority at all — the TS
    /// `authority?.planApproved === undefined` check; the plan is then only
    /// recorded, never judged.
    async fn plan_approved(
        &self,
        _chat_id: Option<&str>,
        _plan_digest: &str,
    ) -> agentkit_host::HostResult<Option<bool>> {
        Ok(None)
    }
}

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
