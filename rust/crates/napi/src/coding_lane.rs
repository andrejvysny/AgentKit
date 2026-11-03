//! The native coding lane: the Rust coding toolpack registered through the
//! binding as a first-class contributor on the SAME executor lane as the JS
//! tools.
//!
//! Registration is late-bound because the `ChatExecutor` (and with it the
//! contributor/guard chain and the standing-approval policy) is constructed
//! before any JS call can arrive: the executor holds the three adapters
//! below for the service's whole life, and they consult one shared lane
//! state that `registerCodingTools` fills exactly once. Unregistered means
//! "no coding tools exist": the contributor offers nothing, the guard has no
//! namespace to judge, and the standing policy keys nothing.
//!
//! Everything that decides is the Rust coding crate's own code: permission
//! is `CodingPermissionGuard` over the shared decision matrix with the
//! binding's durable `BindingAuthority`, high-risk commands are
//! `HighRiskCommandGuard` (registered statically in `service.rs`), and
//! exact-call remembering is `CodingStandingApproval` (fingerprint v4,
//! NODE_OPTIONS startup-file binding, direct-file binding) over the pack's
//! workspace. There is no TS fallback anywhere on this path.

use std::sync::Arc;

use agentkit_core::permission::{CodingMode, CodingRole};
use agentkit_host::{
    CodingPermissionAuthority, ContributedTool, HostResult, ToolContributionContext,
    ToolContributor, ToolGuard, ToolGuardContext, ToolGuardVerdict,
};
use agentkit_toolpacks_coding::command_policy::{
    CodingStandingApproval, StandingApprovalWorkspace, WorkspaceResolver,
};
use agentkit_toolpacks_coding::{
    CODING_NAMESPACE, CodingError, CodingLimitsOverride, CodingPermissionGuard,
    CodingPermissionGuardOptions, CodingToolPack, CodingToolPackOptions, CodingWorkspace,
    LockDirCoordinatorOptions, SandboxMode, WorkspaceCoordinator, create_coding_tool_pack,
    create_lockdir_workspace_coordinator,
};
use serde_json::Value;

use crate::errors;

/// Everything the lane needs after registration. Built once by `register`;
/// read by the contributor, the guard, and the standing resolver.
struct LaneState {
    pack: Arc<CodingToolPack>,
    guard: Arc<CodingPermissionGuard>,
    standing: StandingApprovalWorkspace,
}

/// The one mutable piece of the native coding lane. All three executor
/// adapters share it; registration is one-shot.
pub(crate) struct CodingLane {
    authority: Arc<dyn CodingPermissionAuthority>,
    state: tokio::sync::Mutex<Option<Arc<LaneState>>>,
}

/// The parsed `optionsJson` of `registerCodingTools` (camelCase on the
/// wire; the JS wrapper owns the public surface).
#[derive(Debug)]
struct CodingToolsInput {
    workspace_root: String,
    sandbox: SandboxMode,
    network: bool,
    test_commands: Vec<String>,
    allow_git_writes: bool,
    allow_sensitive: bool,
    configured_mode: CodingMode,
    role: Option<CodingRole>,
    plan_tool_names: Vec<String>,
    identity: String,
    coordination_dir: Option<String>,
    coordination_wait_ms: Option<u64>,
}

impl CodingLane {
    pub(crate) fn new(authority: Arc<dyn CodingPermissionAuthority>) -> Self {
        Self {
            authority,
            state: tokio::sync::Mutex::new(None),
        }
    }

    /// One-shot registration of the Rust-owned coding tools. A second call
    /// is refused — the descriptor set of a service is immutable once the
    /// worker can claim turns, and a silent re-registration would swap the
    /// guard under a running turn.
    pub(crate) async fn register(&self, options_json: &str) -> Result<(), napi::Error> {
        let input = parse_coding_options(options_json)?;
        if self.state.lock().await.is_some() {
            return Err(errors::invalid(
                "coding tools are already registered on this service",
            ));
        }
        let pack = create_coding_tool_pack(CodingToolPackOptions {
            workspace: CodingWorkspace {
                root: input.workspace_root.clone(),
            },
            sandbox: input.sandbox,
            network: input.network,
            test_commands: input.test_commands,
            allow_git_writes: input.allow_git_writes,
            allow_sensitive: input.allow_sensitive,
            limits: CodingLimitsOverride::default(),
            clock: None,
            platform: None,
            sandbox_exec_path: None,
            // The same durable, host-owned authority the JS lane is judged
            // by: the session's mode is a fact in the database, never
            // binding memory, and `submit_plan` reports its grant through
            // it.
            permission: Some(Arc::clone(&self.authority)),
            // TS parity: the TS pack's DEFAULT coordinator is the lock-dir
            // coordinator over the per-uid shared directory
            // (`createCodingToolPack` → `defaultWorkspaceCoordinator()`),
            // not an in-process one — durable shell ownership and the
            // cross-process crash-recovery records depend on it being on
            // disk (the example's acceptance suite witnesses exactly this).
            coordinator: Some(lockdir_coordinator(input.coordination_dir.as_deref())),
            coordination_wait_ms: input.coordination_wait_ms,
        })
        .await
        .map_err(coding)?;
        let guard = Arc::new(CodingPermissionGuard::new(CodingPermissionGuardOptions {
            mode: input.configured_mode,
            role: input.role,
            authority: Some(Arc::clone(&self.authority)),
            plan_tool_names: Some(input.plan_tool_names),
        }));
        let standing = StandingApprovalWorkspace {
            root: input.workspace_root,
            // The workspace identity is part of the standing-approval key
            // envelope: two different checkouts mounted at the same path
            // carry independent approvals. Default: the root itself (the
            // `root` field of the envelope already pins it).
            identity: input.identity,
            context: None,
            // None = the lossy-decoded process environment at claim time —
            // exactly the view the exec path spawns with (`process_env`),
            // so a NODE_OPTIONS that enters the EFFECTIVE environment
            // changes the key (fingerprint v4).
            environment: None,
        };
        let mut state = self.state.lock().await;
        if state.is_some() {
            // Racing registration (the service serializes its callers, but
            // the lane keeps its own invariant): the first registration
            // stands, the loser changes nothing.
            return Err(errors::invalid(
                "coding tools are already registered on this service",
            ));
        }
        *state = Some(Arc::new(LaneState {
            pack,
            guard,
            standing,
        }));
        Ok(())
    }

    /// Settle and drain this lane's command executions, then delete the
    /// pack's scratch — `close()` calls it after the worker drained, so a
    /// runtime never exits with marked children alive (ADR 0020). Never
    /// claims outcomes: whatever the receipt log cannot decide stays
    /// uncertain there, exactly what the crate's own `dispose` promises.
    pub(crate) async fn dispose(&self) {
        let state = self.state.lock().await;
        if let Some(state) = state.as_ref() {
            state.pack.dispose().await;
        }
    }

    async fn state(&self) -> Option<Arc<LaneState>> {
        self.state.lock().await.as_ref().map(Arc::clone)
    }

    /// The advisor's read-only view of the pack: contribute under a
    /// neutral context and keep only the `Read` variants. Unregistered
    /// yields nothing; a contribution failure yields nothing (the advisor
    /// degrades to fewer tools, never to a failed review).
    pub(crate) async fn contribute_read_tools(&self) -> Vec<Arc<dyn agentkit_core::Tool>> {
        let Some(lane) = self.state().await else {
            return vec![];
        };
        let context = ToolContributionContext {
            chat_id: None,
            run_id: "advisor-read".into(),
            scope_id: None,
            actor: serde_json::Value::Null,
            actor_id: None,
            bindings: vec![],
            limits: agentkit_core::ToolLimits::default(),
            cancellation: agentkit_core::CancellationToken::new(),
            has_primary_binding: false,
            timeout_ms: 30_000,
            durable: None,
        };
        match lane.pack.contribute(&context).await {
            Ok(contributed) => contributed
                .into_iter()
                .filter_map(|contributed| match contributed {
                    ContributedTool::Read(tool) => Some(tool),
                    _ => None,
                })
                .collect(),
            Err(_) => vec![],
        }
    }
}

/// The coding-namespace contributor. Unregistered offers no tools; once
/// registered it forwards EVERY contribution call to the Rust pack, which
/// offers its full descriptor set on every run — the guard decides.
pub(crate) struct CodingLaneContributor {
    lane: Arc<CodingLane>,
}

impl CodingLaneContributor {
    pub(crate) fn new(lane: Arc<CodingLane>) -> Self {
        Self { lane }
    }
}

#[async_trait::async_trait]
impl ToolContributor for CodingLaneContributor {
    fn namespace(&self) -> &str {
        CODING_NAMESPACE
    }

    async fn contribute(
        &self,
        context: &ToolContributionContext,
    ) -> HostResult<Vec<ContributedTool>> {
        match self.lane.state().await {
            Some(lane) => lane.pack.contribute(context).await,
            None => Ok(vec![]),
        }
    }
}

/// The coding-namespace permission guard. Unregistered has nothing to
/// judge (`Allowed`; guards compose with AND, and no coding tool exists to
/// be admitted). Registered, every verdict word is the crate's own.
pub(crate) struct CodingLaneGuard {
    lane: Arc<CodingLane>,
}

impl CodingLaneGuard {
    pub(crate) fn new(lane: Arc<CodingLane>) -> Self {
        Self { lane }
    }
}

#[async_trait::async_trait]
impl ToolGuard for CodingLaneGuard {
    async fn can_execute(&self, context: &ToolGuardContext) -> HostResult<ToolGuardVerdict> {
        match self.lane.state().await {
            Some(lane) => lane.guard.can_execute(context).await,
            None => Ok(ToolGuardVerdict::Allowed),
        }
    }
}

/// The standing-approval resolver for the native shell tools: every chat's
/// commands run inside THIS pack's workspace (the pack is the root the exec
/// path spawns in), so the workspace the key binds is the pack's —
/// unregistered resolves to `None`, which keeps every approvable call
/// asking (no remembering without a registered workspace).
pub(crate) struct LaneWorkspaceResolver {
    lane: Arc<CodingLane>,
}

impl LaneWorkspaceResolver {
    pub(crate) fn new(lane: Arc<CodingLane>) -> Self {
        Self { lane }
    }

    /// The executor slot: the crate's own standing policy over this lane.
    pub(crate) fn standing_policy(lane: Arc<CodingLane>) -> Arc<CodingStandingApproval<Self>> {
        Arc::new(CodingStandingApproval {
            resolver: Self::new(lane),
        })
    }
}

#[async_trait::async_trait]
impl WorkspaceResolver for LaneWorkspaceResolver {
    async fn resolve(&self, _chat_id: &str) -> Option<StandingApprovalWorkspace> {
        self.lane.state().await.map(|lane| lane.standing.clone())
    }
}

// ---------------------------------------------------------------------------
// optionsJson parsing — an unsupported value fails registration with
// `invalid_input`, never a silently ignored policy the binding cannot
// enforce (the same rule as `parse_service_options`).
// ---------------------------------------------------------------------------

const CODING_MODES: [&str; 5] = ["read_only", "ask", "auto_edit", "full_auto", "plan"];

fn parse_coding_options(text: &str) -> Result<CodingToolsInput, napi::Error> {
    let value: Value = serde_json::from_str::<Value>(text)
        .ok()
        .filter(|value| value.is_object())
        .ok_or_else(|| errors::invalid("the coding tools options must be a JSON object"))?;
    let object = value.as_object().expect("checked above");
    for key in object.keys() {
        if !matches!(
            key.as_str(),
            "workspaceRoot"
                | "sandbox"
                | "network"
                | "testCommands"
                | "allowGitWrites"
                | "allowSensitive"
                | "configuredMode"
                | "role"
                | "planToolNames"
                | "identity"
                | "coordinationDir"
                | "coordinationWaitMs"
        ) {
            return Err(errors::invalid(format!(
                "unknown coding tools option \"{key}\""
            )));
        }
    }
    let workspace_root = object
        .get("workspaceRoot")
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .ok_or_else(|| errors::invalid("workspaceRoot is required and must be a non-empty string"))?
        .to_owned();
    let sandbox = match object.get("sandbox") {
        None | Some(Value::Null) => SandboxMode::Required,
        Some(Value::String(text)) if text == "required" => SandboxMode::Required,
        Some(Value::String(text)) if text == "off" => SandboxMode::Off,
        Some(_) => {
            return Err(errors::invalid(
                "sandbox must be \"required\" or \"off\" when present",
            ));
        }
    };
    let network = optional_bool(object, "network")?;
    let allow_git_writes = optional_bool(object, "allowGitWrites")?;
    let allow_sensitive = optional_bool(object, "allowSensitive")?;
    let test_commands = match object.get("testCommands") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| {
                item.as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| errors::invalid("testCommands entries must be strings"))
            })
            .collect::<Result<Vec<_>, _>>()?,
        Some(_) => {
            return Err(errors::invalid(
                "testCommands must be an array when present",
            ));
        }
    };
    let configured_mode = match object.get("configuredMode") {
        None | Some(Value::Null) => CodingMode::Ask,
        Some(Value::String(text)) => CodingMode::parse(text.as_str()).ok_or_else(|| {
            errors::invalid(format!(
                "unknown configuredMode \"{text}\" (expected one of: {})",
                CODING_MODES.join(", ")
            ))
        })?,
        Some(_) => {
            return Err(errors::invalid(
                "configuredMode must be a string when present",
            ));
        }
    };
    let role = match object.get("role") {
        None | Some(Value::Null) => None,
        Some(Value::String(text)) => Some(CodingRole::parse(text.as_str()).ok_or_else(|| {
            errors::invalid(format!(
                "unknown role \"{text}\" (expected implementer, reviewer, or supervisor)"
            ))
        })?),
        Some(_) => return Err(errors::invalid("role must be a string when present")),
    };
    let plan_tool_names = match object.get("planToolNames") {
        None | Some(Value::Null) => vec!["submit_plan".to_owned()],
        Some(Value::Array(items)) => {
            if items.is_empty() {
                return Err(errors::invalid("planToolNames must not be empty"));
            }
            let mut parsed: Vec<String> = Vec::with_capacity(items.len());
            for item in items {
                let name = item
                    .as_str()
                    .filter(|text| !text.is_empty())
                    .ok_or_else(|| {
                        errors::invalid("planToolNames entries must be non-empty strings")
                    })?;
                if !parsed.iter().any(|existing| existing == name) {
                    parsed.push(name.to_owned());
                }
            }
            parsed
        }
        Some(_) => {
            return Err(errors::invalid(
                "planToolNames must be an array when present",
            ));
        }
    };
    let identity = match object.get("identity") {
        None | Some(Value::Null) => workspace_root.clone(),
        Some(Value::String(text)) if !text.is_empty() => text.to_owned(),
        Some(_) => {
            return Err(errors::invalid(
                "identity must be a non-empty string when present",
            ));
        }
    };
    let coordination_dir = match object.get("coordinationDir") {
        None | Some(Value::Null) => None,
        Some(Value::String(text)) if !text.is_empty() => Some(text.to_owned()),
        Some(_) => {
            return Err(errors::invalid(
                "coordinationDir must be a non-empty string when present",
            ));
        }
    };
    let coordination_wait_ms = match object.get("coordinationWaitMs") {
        None | Some(Value::Null) => None,
        Some(value) => Some(
            value
                .as_u64()
                .filter(|value| *value > 0 && *value <= u64::from(u32::MAX))
                .ok_or_else(|| errors::invalid("coordinationWaitMs must be a positive integer"))?,
        ),
    };
    Ok(CodingToolsInput {
        workspace_root,
        sandbox,
        network,
        test_commands,
        allow_git_writes,
        allow_sensitive,
        configured_mode,
        role,
        plan_tool_names,
        identity,
        coordination_dir,
        coordination_wait_ms,
    })
}

fn optional_bool(object: &serde_json::Map<String, Value>, key: &str) -> Result<bool, napi::Error> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(false),
        Some(Value::Bool(value)) => Ok(*value),
        Some(_) => Err(errors::invalid(format!(
            "{key} must be a boolean when present"
        ))),
    }
}

/// TS parity: `defaultWorkspaceCoordinationDir()` is
/// `/tmp/agentkit-workspace-locks-<uid>` (`process.getuid?.() ?? "user"`).
fn lockdir_coordinator(dir: Option<&str>) -> Arc<dyn WorkspaceCoordinator> {
    create_lockdir_workspace_coordinator(LockDirCoordinatorOptions {
        dir: dir
            .map(str::to_owned)
            .unwrap_or_else(default_coordination_dir),
        observer: None,
        poll_ms: None,
        on_error: None,
    })
}

fn default_coordination_dir() -> String {
    #[cfg(unix)]
    {
        // SAFETY: `getuid` takes no arguments, touches no memory, and is
        // always successful.
        let uid = unsafe { libc::getuid() }.to_string();
        format!("/tmp/agentkit-workspace-locks-{uid}")
    }
    #[cfg(not(unix))]
    {
        // TS hard-codes `/tmp` (`coordination-lockdir.ts`
        // `defaultWorkspaceCoordinationDir`), which does not exist on
        // Windows; the per-user temp directory is its intended equivalent
        // ("one same-host namespace … scoped to the OS user") — the
        // `?? "user"` fallback kept there is meaningless on this platform.
        std::env::temp_dir()
            .join("agentkit-workspace-locks-user")
            .to_string_lossy()
            .into_owned()
    }
}

/// A pack-configuration refusal is the caller's bad input, not an internal
/// failure: `invalid_configuration` crosses as the contract's
/// `invalid_input`; anything else keeps its origin in `details`.
fn coding(error: CodingError) -> napi::Error {
    let code = if error.code == "invalid_configuration" {
        "invalid_input"
    } else {
        error.code.as_str()
    };
    errors::stable(code, error.message, None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn register_error(text: &str) -> String {
        let error = parse_coding_options(text).unwrap_err();
        error.to_string()
    }

    #[test]
    fn minimal_options_take_the_safe_defaults() {
        let input =
            parse_coding_options(r#"{"workspaceRoot":"/tmp/repo"}"#).expect("valid options");
        assert_eq!(input.workspace_root, "/tmp/repo");
        assert_eq!(input.sandbox, SandboxMode::Required);
        assert!(!input.network);
        assert!(input.test_commands.is_empty());
        // Fail-closed default: an unconfigured native lane ASKS, like the
        // JS authority's default.
        assert_eq!(input.configured_mode, CodingMode::Ask);
        assert_eq!(input.plan_tool_names, vec!["submit_plan".to_owned()]);
        // Identity defaults to the root itself.
        assert_eq!(input.identity, "/tmp/repo");
        assert!(input.coordination_dir.is_none());
    }

    #[test]
    fn full_options_parse_and_validate() {
        let input = parse_coding_options(
            r#"{"workspaceRoot":"/r","sandbox":"off","network":true,
                "testCommands":["bun test"],"allowGitWrites":true,
                "allowSensitive":false,"configuredMode":"full_auto",
                "role":"reviewer","planToolNames":["submit_plan","propose"],
                "identity":"checkout-a","coordinationDir":"/tmp/locks",
                "coordinationWaitMs":5000}"#,
        )
        .expect("valid options");
        assert_eq!(input.sandbox, SandboxMode::Off);
        assert!(input.network && input.allow_git_writes && !input.allow_sensitive);
        assert_eq!(input.test_commands, ["bun test"]);
        assert_eq!(input.configured_mode, CodingMode::FullAuto);
        assert_eq!(input.role, Some(CodingRole::Reviewer));
        assert_eq!(input.plan_tool_names, ["submit_plan", "propose"]);
        assert_eq!(input.identity, "checkout-a");
        assert_eq!(input.coordination_dir.as_deref(), Some("/tmp/locks"));
        assert_eq!(input.coordination_wait_ms, Some(5000));
    }

    #[test]
    fn every_malformed_option_is_invalid_input_not_a_silent_default() {
        assert!(register_error("{}").contains("workspaceRoot"));
        assert!(register_error(r#"{"workspaceRoot":""}"#).contains("workspaceRoot"));
        assert!(register_error(r#"{"workspaceRoot":42}"#).contains("workspaceRoot"));
        assert!(register_error(r#"{"workspaceRoot":"/r","sandbox":"maybe"}"#).contains("sandbox"));
        assert!(
            register_error(r#"{"workspaceRoot":"/r","configuredMode":"yolo"}"#)
                .contains("configuredMode")
        );
        assert!(register_error(r#"{"workspaceRoot":"/r","role":"owner"}"#).contains("role"));
        assert!(register_error(r#"{"workspaceRoot":"/r","planToolNames":[]}"#).contains("empty"));
        assert!(
            register_error(r#"{"workspaceRoot":"/r","testCommands":[7]}"#).contains("testCommands")
        );
        assert!(register_error(r#"{"workspaceRoot":"/r","network":"yes"}"#).contains("network"));
        assert!(
            register_error(r#"{"workspaceRoot":"/r","coordinationWaitMs":0}"#)
                .contains("coordinationWaitMs")
        );
        assert!(register_error(r#"{"workspaceRoot":"/r","mode":"ask"}"#).contains("unknown"));
        assert!(register_error("not json").contains("JSON object"));
        assert!(register_error("[]").contains("JSON object"));
    }

    #[test]
    fn configuration_refusals_cross_as_invalid_input() {
        let error = coding(CodingError::new("invalid_configuration", "nope"));
        assert!(error.to_string().contains("invalid_input"));
        assert!(error.to_string().contains("nope"));
        let foreign = coding(CodingError::new("workspace_busy", "leased"));
        // An unknown origin is redacted to the contract's internal_error
        // but stays inspectable in details.
        assert!(foreign.to_string().contains("workspace_busy"));
    }

    #[test]
    fn default_coordination_dir_matches_the_ts_shape() {
        let dir = default_coordination_dir();
        assert!(dir.starts_with("/tmp/agentkit-workspace-locks-"), "{dir}");
    }

    /// The sandboxed execution stack is Unix-only (roadmap M4): on a
    /// platform without process groups, signals and the marker scan, a
    /// bounded command must refuse with a visible error — never a fabricated
    /// success. This runs in the `native-windows` job's `cargo test` set
    /// (this crate is included there, the toolpack crate itself excluded),
    /// pinning the refusal on the lane where it is the shipped behavior.
    #[cfg(not(unix))]
    #[tokio::test]
    async fn bounded_execution_refuses_windows_with_an_explicit_error() {
        let run = agentkit_toolpacks_coding::process::run_bounded(
            agentkit_toolpacks_coding::process::RunSpec {
                file: "cmd.exe".to_owned(),
                args: Vec::new(),
                cwd: ".".to_owned(),
                env: std::collections::HashMap::new(),
                timeout_ms: 1_000,
                cancellation: None,
                scratch: std::env::temp_dir().to_string_lossy().into_owned(),
                log_name: "refusal.log".to_owned(),
                limits: agentkit_toolpacks_coding::limits::CodingLimits::default(),
                execution_id: "1:windows-refusal".to_owned(),
                pgid_slot: None,
            },
        )
        .await;
        assert_eq!(
            run.spawn_error.as_deref(),
            Some(agentkit_toolpacks_coding::process::UNSUPPORTED_PLATFORM_MESSAGE)
        );
        assert!(run.teardown.outcome().is_clean());
        assert!(!run.truncated);
        assert!(run.exit_code.is_none() && run.signal.is_none());
        assert!(!run.timed_out && !run.aborted);
    }
}
