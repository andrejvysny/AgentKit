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

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use agentkit_core::permission::{CodingMode, CodingRole};
use agentkit_governance::{
    CodingPermissionAuthority, ContributedTool, GovernanceResult, ToolContributionContext,
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

use crate::error::RuntimeError;
use crate::workspace::{canonical_lane_root, permits, session_binding};

/// Everything the lane needs after registration. Built once by `register`;
/// read by the contributor, the guard, and the standing resolver.
struct LaneState {
    pack: Arc<CodingToolPack>,
    guard: Arc<CodingPermissionGuard>,
    standing: StandingApprovalWorkspace,
    /// Canonical registered root; `None` when it could not be resolved, so
    /// every session that names a workspace is refused.
    lane_root: Option<PathBuf>,
    strict: bool,
}

/// The one mutable piece of the native coding lane. All three executor
/// adapters share it; registration is one-shot.
pub struct CodingLane {
    authority: Arc<dyn CodingPermissionAuthority>,
    store: agentkit_store::SqliteStore,
    state: tokio::sync::Mutex<Option<Arc<LaneState>>>,
    /// Set when shutdown begins, before the pack is disposed: no tool may
    /// start executing against a pack that is being torn down.
    disposed: AtomicBool,
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
    /// Base environment for spawned commands AND the standing-approval key;
    /// `None` = the host process environment.
    environment: Option<HashMap<String, String>>,
    /// Also refuse sessions that carry no descriptor workspace.
    require_session_workspace: bool,
}

impl CodingLane {
    pub(crate) fn new(
        authority: Arc<dyn CodingPermissionAuthority>,
        store: agentkit_store::SqliteStore,
    ) -> Self {
        Self {
            authority,
            store,
            state: tokio::sync::Mutex::new(None),
            disposed: AtomicBool::new(false),
        }
    }

    /// Refuse all further executions (`service_closed`). Called before
    /// `dispose` so a still-draining run cannot start a command while the
    /// pack's scratch is being removed. Irreversible.
    pub fn mark_disposed(&self) {
        self.disposed.store(true, Ordering::SeqCst);
    }

    fn is_disposed(&self) -> bool {
        self.disposed.load(Ordering::SeqCst)
    }

    /// One-shot registration of the Rust-owned coding tools. A second call
    /// is refused — the descriptor set of a service is immutable once the
    /// worker can claim turns, and a silent re-registration would swap the
    /// guard under a running turn.
    pub async fn register(&self, options_json: &str) -> Result<(), RuntimeError> {
        let input = parse_coding_options(options_json)?;
        if self.state.lock().await.is_some() {
            return Err(RuntimeError::invalid(
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
            environment: input.environment.clone(),
        })
        .await
        .map_err(coding)?;
        let guard = Arc::new(CodingPermissionGuard::new(CodingPermissionGuardOptions {
            mode: input.configured_mode,
            role: input.role,
            authority: Some(Arc::clone(&self.authority)),
            plan_tool_names: Some(input.plan_tool_names),
        }));
        let lane_root = canonical_lane_root(std::path::Path::new(&input.workspace_root));
        let standing = StandingApprovalWorkspace {
            root: input.workspace_root,
            // The workspace identity is part of the standing-approval key
            // envelope: two different checkouts mounted at the same path
            // carry independent approvals. Default: the root itself (the
            // `root` field of the envelope already pins it).
            identity: input.identity,
            context: None,
            // The SAME map the pack spawns with, so the key fingerprints
            // exactly what runs. None = the lossy-decoded process
            // environment at claim time (`process_env`), so a NODE_OPTIONS
            // that enters the EFFECTIVE environment changes the key
            // (fingerprint v4).
            environment: input.environment,
        };
        let mut state = self.state.lock().await;
        if state.is_some() {
            // Racing registration (the service serializes its callers, but
            // the lane keeps its own invariant): the first registration
            // stands, the loser changes nothing.
            return Err(RuntimeError::invalid(
                "coding tools are already registered on this service",
            ));
        }
        *state = Some(Arc::new(LaneState {
            pack,
            guard,
            standing,
            lane_root,
            strict: input.require_session_workspace,
        }));
        Ok(())
    }

    /// Settle and drain this lane's command executions, then delete the
    /// pack's scratch — `close()` calls it after the worker drained, so a
    /// runtime never exits with marked children alive (ADR 0020). Never
    /// claims outcomes: whatever the receipt log cannot decide stays
    /// uncertain there, exactly what the crate's own `dispose` promises.
    pub async fn dispose(&self) {
        self.mark_disposed();
        let state = self.state.lock().await;
        if let Some(state) = state.as_ref() {
            state.pack.dispose().await;
        }
    }

    async fn state(&self) -> Option<Arc<LaneState>> {
        self.state.lock().await.as_ref().map(Arc::clone)
    }

    /// Whether this chat may use the registered workspace's tools.
    async fn permits_chat(&self, lane: &LaneState, chat_id: Option<&str>) -> bool {
        let binding = session_binding(&self.store, chat_id, lane.lane_root.as_deref()).await;
        permits(binding, lane.strict)
    }

    /// The advisor's read-only view of the pack: contribute under a
    /// neutral context and keep only the `Read` variants. Unregistered
    /// yields nothing; a contribution failure yields nothing (the advisor
    /// degrades to fewer tools, never to a failed review).
    pub(crate) async fn contribute_read_tools(
        &self,
        chat_id: Option<&str>,
    ) -> Vec<Arc<dyn agentkit_core::Tool>> {
        let Some(lane) = self.state().await else {
            return vec![];
        };
        if !self.permits_chat(&lane, chat_id).await {
            return vec![];
        }
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
/// registered it forwards contribution calls from sessions bound to the
/// registered workspace to the Rust pack, which offers its full descriptor
/// set — the guard decides. A session naming a different workspace gets
/// nothing (so neither does its subagent child).
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
    ) -> GovernanceResult<Vec<ContributedTool>> {
        if self.lane.is_disposed() {
            return Ok(vec![]);
        }
        match self.lane.state().await {
            Some(lane)
                if self
                    .lane
                    .permits_chat(&lane, context.chat_id.as_deref())
                    .await =>
            {
                lane.pack.contribute(context).await
            }
            _ => Ok(vec![]),
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
    async fn can_execute(&self, context: &ToolGuardContext) -> GovernanceResult<ToolGuardVerdict> {
        if self.lane.is_disposed() && context.namespace == CODING_NAMESPACE {
            return Ok(ToolGuardVerdict::Refused(
                "service_closed: the session service is closed".to_owned(),
            ));
        }
        match self.lane.state().await {
            Some(lane) => {
                let chat_id = context.contribution.chat_id.as_deref();
                if context.namespace == CODING_NAMESPACE
                    && !self.lane.permits_chat(&lane, chat_id).await
                {
                    return Ok(ToolGuardVerdict::Refused(
                        "workspace_not_granted: this session is not bound to the registered workspace"
                            .to_owned(),
                    ));
                }
                lane.guard.can_execute(context).await
            }
            None => Ok(ToolGuardVerdict::Allowed),
        }
    }
}

/// The standing-approval resolver for the native shell tools: commands run
/// inside THIS pack's workspace (the pack is the root the exec path spawns
/// in), so the workspace the key binds is the pack's — but only for chats
/// bound to it. Unregistered, or a session of another workspace, resolves
/// to `None`, which keeps every approvable call asking.
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
    async fn resolve(&self, chat_id: &str) -> Option<StandingApprovalWorkspace> {
        let lane = self.lane.state().await?;
        if !self.lane.permits_chat(&lane, Some(chat_id)).await {
            return None;
        }
        Some(lane.standing.clone())
    }
}

// ---------------------------------------------------------------------------
// optionsJson parsing — an unsupported value fails registration with
// `invalid_input`, never a silently ignored policy the binding cannot
// enforce (the same rule as `parse_service_options`).
// ---------------------------------------------------------------------------

const CODING_MODES: [&str; 5] = ["read_only", "ask", "auto_edit", "full_auto", "plan"];

fn parse_coding_options(text: &str) -> Result<CodingToolsInput, RuntimeError> {
    let value: Value = serde_json::from_str::<Value>(text)
        .ok()
        .filter(|value| value.is_object())
        .ok_or_else(|| RuntimeError::invalid("the coding tools options must be a JSON object"))?;
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
                | "environment"
                | "requireSessionWorkspace"
        ) {
            return Err(RuntimeError::invalid(format!(
                "unknown coding tools option \"{key}\""
            )));
        }
    }
    let workspace_root = object
        .get("workspaceRoot")
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .ok_or_else(|| {
            RuntimeError::invalid("workspaceRoot is required and must be a non-empty string")
        })?
        .to_owned();
    let sandbox = match object.get("sandbox") {
        None | Some(Value::Null) => SandboxMode::Required,
        Some(Value::String(text)) if text == "required" => SandboxMode::Required,
        Some(Value::String(text)) if text == "off" => SandboxMode::Off,
        Some(_) => {
            return Err(RuntimeError::invalid(
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
                    .ok_or_else(|| RuntimeError::invalid("testCommands entries must be strings"))
            })
            .collect::<Result<Vec<_>, _>>()?,
        Some(_) => {
            return Err(RuntimeError::invalid(
                "testCommands must be an array when present",
            ));
        }
    };
    let configured_mode = match object.get("configuredMode") {
        None | Some(Value::Null) => CodingMode::Ask,
        Some(Value::String(text)) => CodingMode::parse(text.as_str()).ok_or_else(|| {
            RuntimeError::invalid(format!(
                "unknown configuredMode \"{text}\" (expected one of: {})",
                CODING_MODES.join(", ")
            ))
        })?,
        Some(_) => {
            return Err(RuntimeError::invalid(
                "configuredMode must be a string when present",
            ));
        }
    };
    let role = match object.get("role") {
        None | Some(Value::Null) => None,
        Some(Value::String(text)) => Some(CodingRole::parse(text.as_str()).ok_or_else(|| {
            RuntimeError::invalid(format!(
                "unknown role \"{text}\" (expected implementer, reviewer, or supervisor)"
            ))
        })?),
        Some(_) => return Err(RuntimeError::invalid("role must be a string when present")),
    };
    let plan_tool_names = match object.get("planToolNames") {
        None | Some(Value::Null) => vec!["submit_plan".to_owned()],
        Some(Value::Array(items)) => {
            if items.is_empty() {
                return Err(RuntimeError::invalid("planToolNames must not be empty"));
            }
            let mut parsed: Vec<String> = Vec::with_capacity(items.len());
            for item in items {
                let name = item
                    .as_str()
                    .filter(|text| !text.is_empty())
                    .ok_or_else(|| {
                        RuntimeError::invalid("planToolNames entries must be non-empty strings")
                    })?;
                if !parsed.iter().any(|existing| existing == name) {
                    parsed.push(name.to_owned());
                }
            }
            parsed
        }
        Some(_) => {
            return Err(RuntimeError::invalid(
                "planToolNames must be an array when present",
            ));
        }
    };
    let identity = match object.get("identity") {
        None | Some(Value::Null) => workspace_root.clone(),
        Some(Value::String(text)) if !text.is_empty() => text.to_owned(),
        Some(_) => {
            return Err(RuntimeError::invalid(
                "identity must be a non-empty string when present",
            ));
        }
    };
    let coordination_dir = match object.get("coordinationDir") {
        None | Some(Value::Null) => None,
        Some(Value::String(text)) if !text.is_empty() => Some(text.to_owned()),
        Some(_) => {
            return Err(RuntimeError::invalid(
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
                .ok_or_else(|| {
                    RuntimeError::invalid("coordinationWaitMs must be a positive integer")
                })?,
        ),
    };
    let environment = parse_environment(object.get("environment"))?;
    let require_session_workspace = optional_bool(object, "requireSessionWorkspace")?;
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
        environment,
        require_session_workspace,
    })
}

const MAX_ENVIRONMENT_ENTRIES: usize = 1024;

/// A spawn environment is exec-ed verbatim, so a malformed entry (`=` in a
/// name, NUL anywhere) would be silently truncated or rejected by the OS
/// instead of failing at registration.
fn parse_environment(
    value: Option<&Value>,
) -> Result<Option<HashMap<String, String>>, RuntimeError> {
    let object = match value {
        None | Some(Value::Null) => return Ok(None),
        Some(Value::Object(object)) => object,
        Some(_) => {
            return Err(RuntimeError::invalid(
                "environment must be an object of strings when present",
            ));
        }
    };
    if object.len() > MAX_ENVIRONMENT_ENTRIES {
        return Err(RuntimeError::invalid(format!(
            "environment has more than {MAX_ENVIRONMENT_ENTRIES} entries"
        )));
    }
    let mut parsed = HashMap::with_capacity(object.len());
    for (name, value) in object {
        if name.is_empty() || name.contains('=') || name.contains('\0') {
            return Err(RuntimeError::invalid(
                "environment names must be non-empty and contain no '=' or NUL",
            ));
        }
        let text = value
            .as_str()
            .filter(|text| !text.contains('\0'))
            .ok_or_else(|| {
                RuntimeError::invalid("environment values must be strings without NUL")
            })?;
        parsed.insert(name.clone(), text.to_owned());
    }
    Ok(Some(parsed))
}

fn optional_bool(object: &serde_json::Map<String, Value>, key: &str) -> Result<bool, RuntimeError> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(false),
        Some(Value::Bool(value)) => Ok(*value),
        Some(_) => Err(RuntimeError::invalid(format!(
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
        // The id Node's `process.getuid()` reports is the REAL uid, which
        // `agentkit-sys`'s `real_uid` mirrors (`getuid(2)`, not `geteuid`).
        let uid = agentkit_sys::real_uid()
            .expect("getuid never fails on unix")
            .to_string();
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
fn coding(error: CodingError) -> RuntimeError {
    let code = if error.code == "invalid_configuration" {
        "invalid_input"
    } else {
        error.code.as_str()
    };
    RuntimeError::Coded {
        code: code.to_owned(),
        message: error.message,
    }
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
    fn environment_option_accepts_a_string_map_and_defaults_to_none() {
        let input = parse_coding_options(r#"{"workspaceRoot":"/r"}"#).expect("valid options");
        assert!(input.environment.is_none());
        let input = parse_coding_options(
            r#"{"workspaceRoot":"/r","environment":{"PATH":"/bin","EMPTY":""}}"#,
        )
        .expect("valid options");
        let environment = input.environment.expect("environment parsed");
        assert_eq!(environment.len(), 2);
        assert_eq!(environment["PATH"], "/bin");
        assert_eq!(environment["EMPTY"], "");
    }

    #[test]
    fn malformed_environment_is_invalid_input() {
        for bad in [
            r#""PATH=/bin""#,
            r#"["A=b"]"#,
            r#"{"A":1}"#,
            r#"{"A":null}"#,
            r#"{"A=B":"x"}"#,
            r#"{"":"x"}"#,
            r#"{"A\u0000":"x"}"#,
            r#"{"A":"x\u0000y"}"#,
        ] {
            let text = format!(r#"{{"workspaceRoot":"/r","environment":{bad}}}"#);
            assert!(register_error(&text).contains("environment"), "{bad}");
        }
        let many: Vec<String> = (0..=MAX_ENVIRONMENT_ENTRIES)
            .map(|index| format!(r#""K{index}":"v""#))
            .collect();
        let text = format!(
            r#"{{"workspaceRoot":"/r","environment":{{{}}}}}"#,
            many.join(",")
        );
        assert!(register_error(&text).contains("1024"));
    }

    #[test]
    fn configuration_refusals_cross_as_invalid_input() {
        let RuntimeError::Coded { code, message } =
            coding(CodingError::new("invalid_configuration", "nope"))
        else {
            panic!("a pack refusal keeps its code");
        };
        assert_eq!(code, "invalid_input");
        assert_eq!(message, "nope");
        // An unknown origin keeps its code; the binding redacts it to the
        // contract's internal_error with the origin in details.
        let RuntimeError::Coded { code, .. } = coding(CodingError::new("workspace_busy", "leased"))
        else {
            panic!("a pack refusal keeps its code");
        };
        assert_eq!(code, "workspace_busy");
    }

    fn guard_context(chat_id: &str) -> ToolGuardContext {
        ToolGuardContext {
            contribution: ToolContributionContext {
                chat_id: Some(chat_id.to_owned()),
                run_id: "r".into(),
                scope_id: None,
                actor: Value::Null,
                actor_id: None,
                bindings: vec![],
                limits: agentkit_core::ToolLimits::default(),
                cancellation: agentkit_core::CancellationToken::new(),
                has_primary_binding: false,
                timeout_ms: 1_000,
                durable: None,
            },
            namespace: CODING_NAMESPACE.to_owned(),
            tool: agentkit_core::ToolDefinition {
                name: "read".into(),
                version: "1".into(),
                effect: "read".into(),
                capability: "coding.read".into(),
                description: String::new(),
                input_schema: Value::Null,
                output_schema: None,
                timeout_ms: None,
            },
            arguments: None,
        }
    }

    async fn bound_lane(strict: bool) -> (Arc<CodingLane>, agentkit_store::SqliteStore) {
        use crate::workspace::tests::{add_session, open_store, scratch};
        let store = open_store();
        let root = scratch("coding-lane");
        let other = scratch("coding-other");
        add_session(&store, "a", root.to_str()).await;
        add_session(&store, "b", other.to_str()).await;
        add_session(&store, "none", None).await;
        let authority = Arc::new(crate::authority::BindingAuthority::new());
        let lane = Arc::new(CodingLane::new(authority, store.clone()));
        let options = serde_json::json!({
            "workspaceRoot": root, "sandbox": "off", "requireSessionWorkspace": strict,
            "coordinationDir": scratch("locks"),
        });
        lane.register(&options.to_string()).await.expect("register");
        (lane, store)
    }

    #[tokio::test]
    async fn guard_refuses_and_resolver_withholds_for_a_mismatched_session() {
        let (lane, _store) = bound_lane(false).await;
        let guard = CodingLaneGuard::new(Arc::clone(&lane));
        let refused = guard.can_execute(&guard_context("b")).await.unwrap();
        assert!(
            matches!(&refused, ToolGuardVerdict::Refused(text) if text.starts_with("workspace_not_granted")),
        );
        let resolver = LaneWorkspaceResolver::new(Arc::clone(&lane));
        assert!(resolver.resolve("b").await.is_none());
        assert!(resolver.resolve("a").await.is_some());
        // Unbound sessions keep the service-wide behavior when not strict.
        assert!(resolver.resolve("none").await.is_some());
        let contributor = CodingLaneContributor::new(Arc::clone(&lane));
        assert!(
            contributor
                .contribute(&guard_context("b").contribution)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            !contributor
                .contribute(&guard_context("a").contribution)
                .await
                .unwrap()
                .is_empty()
        );
        lane.dispose().await;
    }

    #[tokio::test]
    async fn advisor_read_tools_follow_the_reviewed_session_binding() {
        use agentkit_agents::AdvisorToolSource;
        let (lane, store) = bound_lane(false).await;
        let tools = crate::lanes::advisor::LaneAdvisorTools {
            coding: Arc::clone(&lane),
            lsp: Arc::new(crate::lanes::lsp::LspLane::new(store)),
        };
        assert!(tools.read_tools("b").await.is_empty());
        assert!(!tools.read_tools("a").await.is_empty());
        lane.dispose().await;
    }

    #[tokio::test]
    async fn strict_registration_also_withholds_from_unbound_sessions() {
        let (lane, _store) = bound_lane(true).await;
        let resolver = LaneWorkspaceResolver::new(Arc::clone(&lane));
        assert!(resolver.resolve("none").await.is_none());
        assert!(resolver.resolve("a").await.is_some());
        let contributor = CodingLaneContributor::new(Arc::clone(&lane));
        assert!(
            contributor
                .contribute(&guard_context("none").contribution)
                .await
                .unwrap()
                .is_empty()
        );
        lane.dispose().await;
    }

    #[test]
    fn require_session_workspace_must_be_a_boolean() {
        assert!(
            register_error(r#"{"workspaceRoot":"/r","requireSessionWorkspace":"yes"}"#)
                .contains("requireSessionWorkspace")
        );
        let input =
            parse_coding_options(r#"{"workspaceRoot":"/r","requireSessionWorkspace":true}"#)
                .expect("valid");
        assert!(input.require_session_workspace);
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
