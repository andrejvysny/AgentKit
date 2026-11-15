use super::authority::{
    ParentAuthority, ParentTool, UNBOUNDED, child_mode, effective_tools, is_writer,
    mode_is_read_only,
};
use super::brief::{
    Brief, BudgetLines, ContextRef, RefKind, RenderedRef, cap_message_text, confine_to_workspace,
    render_brief,
};
use super::budget;
use super::config::{RoleSpec, SubagentConfig};
use super::envelope::{
    BudgetExhausted, ChildReport, EnvelopeArtifact, EnvelopeUsage, ResultEnvelope,
    SUMMARY_CAP_BYTES, cut_summary, cut_summary_to, envelope_status, present_envelope,
    usage_from_events,
};
use super::guard::WorkspaceLeaseRegistry;
use super::memory_refs::MemoryRefReader;
use super::snapshot::{EffectiveRole, RoleSnapshot, effective_role};
use crate::{AgentsError, AgentsResult, LeaseHeld, RoleDenial};
use agentkit_core::{
    HumanRequestSpec, OutputHandle, OutputMeta, OutputStore, SUBAGENT_METADATA_KEY, SessionTree,
    recorded_tools, untrusted_subagent_block,
};
use agentkit_engine::SharedRuntime;
use agentkit_kernel::sha256_hex;
use agentkit_sessions::{
    AgentInputKind, AgentInputQuery, AgentInputStatus, AgentSendResult, AgentSessionService,
    AnswerActor, AnswerHumanRequest, AnswerInput, AnswerReceipt, CHAT_TURN_TASK_KIND,
    CreateSessionInput, FollowUpInput, SESSION_PREFIX_METADATA_KEY, SendInput, SteerInput,
    WhenBusy, read_session_descriptor, read_session_prefix,
};
use agentkit_store::{
    CHILD_WAIT_ID_PREFIX, CHILD_WAIT_KIND, ConversationTimestamp, RegisterSubagent,
    SqliteOutputStore, SqliteStore, StoreError, SubagentRecord, TaskRecord, TaskStatus,
    is_child_derived_message, is_child_wait,
};
use serde::Serialize;
use serde_json::{Map, Value, json};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use tokio_util::sync::CancellationToken;

/// One delegation request, already authenticated by the tool layer: the
/// parent coordinates its OWN children only (the tool passes the running
/// turn's chat/task; callers cannot name someone else's session).
#[derive(Clone, Debug)]
pub struct SpawnRequest {
    pub parent_chat_id: String,
    pub parent_task_id: String,
    /// Stable identity for one logical spawn call. The child's identity is
    /// derived from (parent chat, parent task, spawn key), so replaying the
    /// same tool call after an attempt retry re-admits the SAME child.
    pub spawn_key: String,
    /// The structured brief (role, objective, scope, references, budget).
    pub brief: Brief,
    pub exclusive_workspace: bool,
}

/// A child plus the projection the parent received at spawn time.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SpawnedChild {
    pub child_chat_id: String,
    pub turn_task_id: Option<String>,
    pub status: String,
    pub role: String,
    pub depth: i64,
    pub model: Option<String>,
}

/// The registry row joined with the authoritative live state.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SubagentView {
    pub child_chat_id: String,
    pub parent_chat_id: String,
    pub parent_task_id: Option<String>,
    pub role: String,
    /// Digest of the spawn-time role snapshot; absent on legacy children.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role_digest: Option<String>,
    pub depth: i64,
    pub prompt: String,
    pub model: Option<String>,
    pub status: String,
    pub result: Option<String>,
    pub error: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
    pub turn_task_id: Option<String>,
    /// The child's task status straight from the task store (`None` before
    /// its first turn exists).
    pub task_status: Option<String>,
    /// Whether the child currently waits on a human (approval/answer).
    pub waiting: bool,
    pub queued_inputs: usize,
}

/// Outcome of one `subagent_await` call.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AwaitResult {
    pub child_chat_id: String,
    pub role: String,
    pub status: String,
    /// The result envelope of a settled child; `None` while it still runs
    /// and for an unknown child. Child text lives only in here, and the tool
    /// layer presents it inside the untrusted `<subagent_result>` block.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub envelope: Option<ResultEnvelope>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AwaitOutcome {
    pub settled: bool,
    pub timed_out: bool,
    pub results: Vec<AwaitResult>,
}

/// When a parked `subagent_await` wakes on its children.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AwaitMode {
    /// Every awaited child is terminal.
    #[default]
    All,
    /// The first awaited child to settle.
    Any,
}

impl AwaitMode {
    /// The tool argument: absent means `all`.
    pub fn from_arg(value: Option<&str>) -> Result<Self, String> {
        match value {
            None | Some("all") => Ok(Self::All),
            Some("any") => Ok(Self::Any),
            Some(other) => Err(format!(
                "invalid_mode: mode must be \"all\" or \"any\", got \"{other}\""
            )),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Any => "any",
        }
    }

    fn satisfied(self, views: &[SubagentView]) -> bool {
        match self {
            Self::All => views.iter().all(|view| is_terminal_status(&view.status)),
            Self::Any => {
                views.is_empty() || views.iter().any(|view| is_terminal_status(&view.status))
            }
        }
    }
}

/// What `subagent_await` does for a durable turn: answer now, or park.
#[derive(Debug)]
pub enum AwaitDecision {
    /// The wait is already satisfied (or non-blocking): no park needed.
    Ready(AwaitOutcome),
    /// Park the turn on this request; the waker resolves it.
    Park(HumanRequestSpec),
}

/// Shared by every clone: the background waker's lifecycle and health.
#[derive(Default)]
struct WakerState {
    started: AtomicBool,
    /// Prompt re-evaluation after a park or a child settle, so the idle
    /// backoff never delays a wake the process itself caused.
    nudge: tokio::sync::Notify,
    /// Failed evaluations since the service started.
    failures: AtomicU64,
    /// A failure streak is reported once, not once per tick.
    warned: AtomicBool,
    /// Set when the last owning handle drops or the host shuts down.
    stopped: AtomicBool,
    /// Wakes the loop out of its sleep so it exits at once.
    stop: tokio::sync::Notify,
    /// The loop task, so shutdown can wait until it released everything.
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl WakerState {
    fn request_stop(&self) {
        self.stopped.store(true, Ordering::SeqCst);
        self.stop.notify_one();
    }

    fn note_failure(&self, what: &str, error: &dyn std::fmt::Display) {
        self.failures.fetch_add(1, Ordering::Relaxed);
        if !self.warned.swap(true, Ordering::Relaxed) {
            eprintln!("agentkit: parked-parent waker: {what} failed: {error}");
        }
    }
}

/// What a `subagent_message` did: joined a live turn or started a new one.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageReceipt {
    pub child_chat_id: String,
    pub delivered: String,
    pub turn_task_id: Option<String>,
}

/// Host-level subagent coordination over the durable engine. Cheap-clone:
/// everything shared is `Arc`; the binding keeps one instance and hands it
/// to the contributor, the guard, and the native surface.
#[derive(Clone)]
pub struct SubagentService {
    store: SqliteStore,
    session: AgentSessionService<SqliteStore>,
    runtime: SharedRuntime,
    config: Arc<Mutex<SubagentConfig>>,
    leases: WorkspaceLeaseRegistry,
    /// The contributor namespace each session's tools were staged under, as
    /// the role guard saw them. The recorded manifest carries names only;
    /// spawn pairs them with these to freeze `namespace:name` identities.
    tool_namespaces: ToolNamespaces,
    /// Serializes the spawn check→insert critical section (see `spawn`).
    spawn_gate: Arc<tokio::sync::Mutex<()>>,
    /// Serializes token-gate check + reserve across this service's gates, so
    /// two siblings cannot both pass the tree check against one budget.
    token_gate_lock: Arc<tokio::sync::Mutex<()>>,
    /// Provider ids this service's executor can route to; `None` = unknown
    /// (host-only users), which skips spawn-time validation.
    routable_providers: Option<Arc<std::collections::BTreeSet<String>>>,
    /// Decides whether a parent may hand on a memory record; `None` means no
    /// memory service, and a brief's `memory` references are refused.
    memory_refs: Option<Arc<dyn MemoryRefReader>>,
    /// Parked-parent waker lifecycle (see `ensure_waker`).
    waker: Arc<WakerState>,
    /// Keeps the store alive for as long as any handle exists. The waker
    /// loop holds only a `Weak` to it, so it never pins the store.
    owner: Option<Arc<ServiceOwner>>,
}

/// Session id -> (tool name -> contributor namespace).
type ToolNamespaces =
    Arc<Mutex<std::collections::HashMap<String, std::collections::HashMap<String, String>>>>;

/// The shared core the waker upgrades per tick. Dropping the last handle
/// drops it, which stops the waker and releases the store.
struct ServiceOwner {
    core: SubagentService,
}

impl Drop for ServiceOwner {
    fn drop(&mut self) {
        self.core.waker.request_stop();
    }
}

/// A brief's references after resolution.
#[derive(Default)]
struct ResolvedContext {
    rendered: Vec<RenderedRef>,
    /// Output handles the child may read.
    grants: Vec<String>,
}

/// A human request waiting on the owner, with who raised it.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OwnerRequest {
    pub request_id: String,
    /// The session the request was raised in.
    pub session_id: String,
    /// The raising child's role; `None` for the owner's own session.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// `<child title> (<role>)` for a child's request, so the owner knows
    /// whose approval this is; `None` for the owner's own session.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub content: Value,
}

/// Releases a lease taken during `resume` unless disarmed with `keep`, so
/// no error path after acquisition can leave the workspace locked.
struct LeaseRelease {
    leases: WorkspaceLeaseRegistry,
    child_chat_id: String,
    /// The generation this call took; `None` = nothing to release.
    generation: Option<u64>,
}

impl LeaseRelease {
    fn keep(mut self) {
        self.generation = None;
    }
}

impl Drop for LeaseRelease {
    fn drop(&mut self) {
        if let Some(generation) = self.generation {
            self.leases
                .release_generation(&self.child_chat_id, generation);
        }
    }
}

fn session_error(error: agentkit_sessions::SessionsError<StoreError>) -> AgentsError {
    AgentsError::Sessions(error)
}

pub(super) fn is_terminal_status(status: &str) -> bool {
    matches!(status, "completed" | "failed" | "cancelled")
}

/// Absolute ceilings on model-authored child bounds (`subagent_spawn`
/// arguments come from the model): a role ceiling the host set is never
/// lifted by a request, and no request may exceed these. Chosen far above
/// any legitimate single child turn while staying impossible to mine for
/// sustained provider drain.
pub(super) const SUBAGENT_ITERATIONS_CEILING: u32 = 64;
pub(super) const SUBAGENT_PROVIDER_REQUESTS_CEILING: u64 = 200;
pub(super) const SUBAGENT_OVERALL_MS_CEILING: u64 = 30 * 60 * 1_000;
/// Token budget per child (spec "Budgets"): the default and the ceiling a
/// request is clamped to. Recorded at spawn and enforced before each
/// provider request by the token gate (DEL-06).
pub(super) const SUBAGENT_TOKENS_DEFAULT: u64 = 200_000;
pub(super) const SUBAGENT_TOKENS_CEILING: u64 = 1_000_000;

/// Deterministic child identity for one logical spawn call. Public so
/// callers (tests, recovery tooling) can precompute the id a given
/// (parent, parent task, spawn key) triple will produce.
pub fn child_identity(parent_chat_id: &str, parent_task_id: &str, spawn_key: &str) -> String {
    format!(
        "sub_{}",
        &sha256_hex(format!("{parent_chat_id}|{parent_task_id}|{spawn_key}"))[..24]
    )
}

impl SubagentService {
    pub fn new(
        store: SqliteStore,
        session: AgentSessionService<SqliteStore>,
        runtime: SharedRuntime,
        config: SubagentConfig,
    ) -> Self {
        Self {
            store,
            session,
            runtime,
            config: Arc::new(Mutex::new(config.effective())),
            leases: WorkspaceLeaseRegistry::default(),
            tool_namespaces: ToolNamespaces::default(),
            spawn_gate: Arc::new(tokio::sync::Mutex::new(())),
            token_gate_lock: Arc::new(tokio::sync::Mutex::new(())),
            routable_providers: None,
            memory_refs: None,
            waker: Arc::new(WakerState::default()),
            owner: None,
        }
        .with_owner()
    }

    /// (Re)build the owner from the current fields; call after any field
    /// change so the waker's copy matches.
    fn with_owner(mut self) -> Self {
        self.owner = None;
        // Dropping the previous owner requested a stop; this is a rebuild,
        // not a shutdown.
        self.waker.stopped.store(false, Ordering::SeqCst);
        let owner = Arc::new(ServiceOwner { core: self.clone() });
        self.owner = Some(owner);
        self
    }

    /// Let briefs hand on memory records the parent can read; without it
    /// every `memory` reference is refused.
    pub fn with_memory_refs(mut self, reader: Arc<dyn MemoryRefReader>) -> Self {
        self.memory_refs = Some(reader);
        self
    }

    /// Inject the routable provider ids once at bind time so a role pinned
    /// to an unroutable provider fails at spawn, not on the child's turn.
    pub fn with_routable_providers(mut self, ids: std::collections::BTreeSet<String>) -> Self {
        self.routable_providers = Some(Arc::new(ids));
        self.with_owner()
    }

    /// Refuse a role/provider/model combination the executor cannot honor,
    /// before any row or session exists. A provider switch needs a model:
    /// the parent's model name is meaningless on a different provider.
    fn validate_child_provider(
        &self,
        role: &RoleSpec,
        request: &SpawnRequest,
        provider_id: Option<&str>,
        parent_provider: Option<&str>,
    ) -> AgentsResult<()> {
        // The native lane runs turns outside the usage authorizer, so its
        // children would escape the token budgets: refuse it outright.
        if provider_id == Some("native") {
            return Err(AgentsError::SubagentProviderUnavailable(
                "native_lane_unsupported: subagent children run on model providers; \
                 the native lane is not metered by the token budgets"
                    .to_owned(),
            ));
        }
        let (Some(known), Some(provider_id)) = (&self.routable_providers, provider_id) else {
            return Ok(());
        };
        if !known.contains(provider_id) {
            return Err(AgentsError::SubagentProviderUnavailable(format!(
                "subagent_provider_unavailable: {provider_id}"
            )));
        }
        if Some(provider_id) != parent_provider
            && request.brief.model.is_none()
            && role.model.is_none()
        {
            return Err(AgentsError::SubagentModelRequired(format!(
                "subagent_model_required: role {} pins provider {provider_id} without a model",
                role.id
            )));
        }
        Ok(())
    }

    pub fn config(&self) -> SubagentConfig {
        self.config
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    pub fn configure(&self, config: SubagentConfig) {
        *self.config.lock().unwrap_or_else(PoisonError::into_inner) = config.effective();
    }

    /// The pre-request token-budget check, as a usage authorizer: install it
    /// as the executor's `usage` hook, passing the host's own authorizer (if
    /// any) as `inner` so both run. It reads the live config, so a changed
    /// tree token limit applies without rebuilding the executor.
    pub fn token_gate(
        &self,
        inner: Option<Arc<dyn agentkit_chat::UsageAuthorizer>>,
    ) -> Arc<budget::SubagentTokenGate> {
        Arc::new(budget::SubagentTokenGate::new(
            self.store.clone(),
            Arc::clone(&self.config),
            self.runtime.clone(),
            inner,
            Arc::clone(&self.token_gate_lock),
        ))
    }

    /// Exclusive-lease admission with a refusal instead of silent
    /// preemption when another child already holds the root unexpired.
    fn acquire_lease(
        &self,
        root: Option<&str>,
        exclusive: bool,
        child_chat_id: &str,
        ttl_ms: i64,
    ) -> AgentsResult<()> {
        self.leases
            .try_acquire(root, exclusive, child_chat_id, self.now_ms(), ttl_ms)
            .map_err(|LeaseHeld { holder }| {
                AgentsError::WorkspaceBusy(format!(
                "workspace_busy: {} is exclusively leased to child {holder}; await or cancel it before spawning another writer",
                    root.unwrap_or("<workspace>")
                ))
            })
    }

    /// Record that `chat_id` offers tool `name` under contributor
    /// `namespace`. Called by the role guard while a session stages its
    /// tools; a later stage of the same name overwrites.
    pub fn note_tool_namespace(&self, chat_id: &str, namespace: &str, name: &str) {
        self.tool_namespaces
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entry(chat_id.to_owned())
            .or_default()
            .insert(name.to_owned(), namespace.to_owned());
    }

    fn tool_namespaces_of(&self, chat_id: &str) -> std::collections::HashMap<String, String> {
        self.tool_namespaces
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(chat_id)
            .cloned()
            .unwrap_or_default()
    }

    pub fn leases(&self) -> &WorkspaceLeaseRegistry {
        &self.leases
    }

    pub fn store(&self) -> &SqliteStore {
        &self.store
    }

    pub fn session(&self) -> &AgentSessionService<SqliteStore> {
        &self.session
    }

    pub fn now_ms(&self) -> i64 {
        self.runtime.now_ms()
    }

    // -- spawn -----------------------------------------------------------------

    /// Create + dispatch one child session. Every limit is checked against
    /// DURABLE state, so a restarted process enforces the same fan-out
    /// bounds as the original one.
    pub async fn spawn(&self, request: &SpawnRequest) -> AgentsResult<SpawnedChild> {
        let config = self.config();
        if !config.enabled {
            return Err(AgentsError::SubagentsDisabled);
        }
        request
            .brief
            .validate()
            .map_err(|error| AgentsError::InvalidBrief(error.to_string()))?;
        let role = config.role(&request.brief.role).cloned().ok_or_else(|| {
            AgentsError::UnknownSubagentRole(format!(
                "unknown_subagent_role: {}",
                request.brief.role
            ))
        })?;
        // A host-pinned role model is a ceiling the model-authored request
        // cannot lift: the role may run on exactly that model.
        if let (Some(pinned), Some(requested)) =
            (role.model.as_deref(), request.brief.model.as_deref())
            && pinned != requested
        {
            return Err(AgentsError::ModelNotAllowed(format!(
                "model_not_allowed: role {} is pinned to model {pinned}; requested {requested}",
                role.id
            )));
        }
        let parent_task = self
            .store
            .get_task(&request.parent_task_id)
            .await?
            .filter(|task| task.scope_id == request.parent_chat_id)
            .ok_or_else(|| AgentsError::SpawnParentTurnRequired)?;
        if parent_task.status.is_terminal() {
            return Err(AgentsError::ParentTurnAlreadySettled);
        }
        let limits = &config.limits;
        let parent_row = self.store.get_subagent(&request.parent_chat_id).await?;
        let depth = parent_row.as_ref().map_or(0, |row| row.depth);
        if depth + 1 > limits.max_depth {
            return Err(AgentsError::SubagentDepthExceeded(format!(
                "subagent_depth_exceeded: depth {depth} cannot spawn at max {}",
                limits.max_depth
            )));
        }
        let mut parent_effective: Option<EffectiveRole> = None;
        if let Some(row) = parent_row.as_ref() {
            // Only roles allowed to nest may themselves delegate; the
            // parent's spawn-time snapshot caps what a later config edit
            // could grant.
            let parent_role = match self.effective_role_of(row, &config).await? {
                Ok(effective) => effective,
                Err(reason) => return Err(AgentsError::RoleDenied(reason)),
            };
            if !parent_role.can_spawn {
                return Err(AgentsError::RoleCannotSpawn(parent_role.id.clone()));
            }
            parent_effective = Some(parent_role);
        }
        let parent_chat = self
            .store
            .get_chat(&request.parent_chat_id)
            .await?
            .ok_or_else(|| AgentsError::ParentSessionNotFound)?;
        let descriptor = read_session_descriptor(&parent_chat.metadata)
            .ok_or_else(|| AgentsError::ParentSessionDescriptorMissing)?;
        let workspace = descriptor.get("workspace").cloned();
        let workspace_root = workspace
            .as_ref()
            .and_then(|value| value.get("root"))
            .and_then(Value::as_str);
        let parent_model = descriptor.get("model").and_then(Value::as_str);
        // Role-level provider override: a host that pins a role to its own
        // provider must not have that role's model silently run against the
        // parent's provider binding.
        let parent_provider = descriptor.get("providerId").and_then(Value::as_str);
        let provider_id = role
            .provider_id
            .clone()
            .or_else(|| parent_provider.map(str::to_owned));
        let child_model = request
            .brief
            .model
            .clone()
            .or_else(|| role.model.clone())
            .or_else(|| parent_model.map(str::to_owned));
        self.validate_child_provider(&role, request, provider_id.as_deref(), parent_provider)?;
        // Authority intersection (4.4): the child's tools are the role
        // allowlist intersected with what the parent recorded at its first
        // turn, in the mode the child will run in. Execution-mode
        // inheritance: never more permissive than the parent's durable mode;
        // read-only roles and workspace-less parents pin `read_only`.
        let parent_mode = self
            .session
            .get_execution(&request.parent_chat_id)
            .await
            .map_err(session_error)?
            .and_then(|state| state.get("mode").and_then(Value::as_str).map(str::to_owned));
        let execution_mode =
            child_mode(parent_mode.as_deref(), role.read_only, workspace.is_some());
        // A parent with a recorded session prefix must have a valid
        // manifest: absent, malformed or hash-mismatched means the parent's
        // authority is unknowable, so nothing is delegated. Only a parent
        // with no prefix at all (a non-session chat) takes the legacy
        // path, where the role alone bounds the child.
        let unavailable = || {
            AgentsError::ParentManifestUnavailable(format!(
                "parent_manifest_unavailable: the parent session's tool manifest is missing \
                 or invalid, so the authority to delegate cannot be determined for role {}",
                role.id
            ))
        };
        let recorded = match read_session_prefix(&parent_chat.metadata) {
            Some(prefix) => Some(recorded_tools(&prefix).ok_or_else(unavailable)?),
            // A prefix record that exists but cannot be read is invalid,
            // not absent.
            None if parent_chat
                .metadata
                .get(SESSION_PREFIX_METADATA_KEY)
                .is_some() =>
            {
                return Err(unavailable());
            }
            None => None,
        };
        let namespaces = self.tool_namespaces_of(&request.parent_chat_id);
        // A tool whose namespace was never seen is left out: its identity
        // is unknown, so the child cannot be granted it.
        let parent_tools: Option<Vec<ParentTool>> = recorded.map(|tools| {
            tools
                .into_iter()
                .filter_map(|definition| {
                    let namespace = namespaces.get(&definition.name)?.clone();
                    Some(ParentTool {
                        namespace,
                        definition,
                    })
                })
                .collect()
        });
        let parent_authority = match parent_effective.as_ref() {
            Some(parent) => ParentAuthority::Child(parent),
            None => ParentAuthority::Root,
        };
        // Every new spawn records an effective set. A non-session parent
        // has no manifest to intersect with; it records the explicit
        // unbounded marker and the role allowlist alone applies.
        let effective: Vec<String> = match parent_tools.as_deref() {
            Some(tools) => effective_tools(
                &role,
                tools,
                &parent_authority,
                mode_is_read_only(execution_mode.as_deref()),
            ),
            None => vec![UNBOUNDED.to_owned()],
        };
        // Writer = the effective set can write or run commands. Without a
        // manifest the role's own read-only flag decides (never the
        // request's flag), for the workspace check and the lease alike.
        let writer = match parent_tools.as_deref() {
            Some(tools) => is_writer(tools, &effective),
            None => !role.read_only,
        };
        if writer && workspace.is_none() {
            return Err(AgentsError::WorkspaceRequired(format!(
                "workspace_required: role {} writes, but the parent session has no workspace",
                role.id
            )));
        }

        let child_chat_id = child_identity(
            &request.parent_chat_id,
            &request.parent_task_id,
            &request.spawn_key,
        );
        // Durable per-child exclusivity: recorded at spawn so `resume`
        // re-admits the lease instead of letting a restarted writer run
        // unleased beside a fresh exclusive sibling.
        // A writer ALWAYS takes the exclusive lease (the request flag is no
        // longer its choice); a non-writer never does.
        let exclusive = writer;

        // Replay path FIRST — before the caps: re-admitting a spawn the
        // store already holds is not a new admission, so a replay must not
        // error behind a saturated roster (the retry could never deliver
        // its result). The same logical spawn call (retry of a tool call)
        // re-admits the existing child; nothing is created twice.
        if let Some(existing) = self.store.get_subagent(&child_chat_id).await? {
            if existing.parent_chat_id != request.parent_chat_id
                || existing.parent_task_id.as_deref() != Some(request.parent_task_id.as_str())
                || existing.role != request.brief.role
            {
                return Err(AgentsError::SpawnIdentityConflict(
                    request.spawn_key.clone(),
                ));
            }
            self.require_same_brief(&child_chat_id, request).await?;
            // A crash between registering the child and dispatching its
            // first turn leaves a QUEUED row nobody owns: the replay must
            // re-drive, not just report. The turn submission is
            // deterministic (content-derived input id), so re-driving can
            // never double-execute.
            if !is_terminal_status(&existing.status) && existing.turn_task_id.is_none() {
                // The parent may have settled while this child sat queued
                // and the settle observer may already have swept rows that
                // did not exist yet: re-check liveness before dispatching.
                let parent = self.store.get_task(&request.parent_task_id).await?;
                if parent.is_none_or(|parent| parent.status.is_terminal()) {
                    self.store
                        .update_subagent(
                            &child_chat_id,
                            "cancelled",
                            None,
                            Some("parent turn settled before the queued child was re-driven"),
                            self.now_ms(),
                        )
                        .await?;
                    return Err(AgentsError::QueuedChildCancelled);
                }
                // Re-drive under the SAME lease admission the create path
                // gives a fresh writer: a create refused after register
                // (`workspace_busy`) must not let the retry dispatch an
                // unleased writer beside the current lease holder. The
                // persisted spawn decision governs, as for any replay.
                let persisted = self
                    .store
                    .get_chat(&child_chat_id)
                    .await?
                    .and_then(|chat| chat.metadata["agentkit.subagent"]["exclusive"].as_bool());
                if let Err(error) = self.acquire_lease(
                    workspace_root,
                    persisted.unwrap_or(exclusive),
                    &existing.child_chat_id,
                    config.limits.lease_ttl_ms,
                ) {
                    let reason = format!("workspace_busy before re-drive: {error}");
                    self.store
                        .update_subagent(
                            &child_chat_id,
                            "cancelled",
                            None,
                            Some(&reason),
                            self.now_ms(),
                        )
                        .await?;
                    return Err(error);
                }
                self.dispatch_initial_turn(&child_chat_id, &role, request, workspace_root)
                    .await?;
                let Some(reactualized) = self.store.get_subagent(&child_chat_id).await? else {
                    return Err(AgentsError::SubagentRowVanished);
                };
                let view = self.refresh(&reactualized).await?;
                return Ok(SpawnedChild {
                    child_chat_id: view.child_chat_id.clone(),
                    turn_task_id: view.turn_task_id.clone(),
                    status: view.status.clone(),
                    role: view.role.clone(),
                    depth: view.depth,
                    model: view.model.clone(),
                });
            }
            if !is_terminal_status(&existing.status) {
                // An active child re-admits with its lease refreshed — but
                // never by preempting a foreign holder: if the lease lapsed
                // (TTL) and another writer legitimately took the root, this
                // replay is refused rather than stealing the workspace back.
                // The PERSISTED spawn decision governs the lease: a retried
                // call flipping `exclusiveWorkspace` must not silently drop
                // a live writer's coordination.
                let persisted = self
                    .store
                    .get_chat(&existing.child_chat_id)
                    .await?
                    .and_then(|chat| chat.metadata["agentkit.subagent"]["exclusive"].as_bool());
                self.acquire_lease(
                    workspace_root,
                    persisted.unwrap_or(exclusive),
                    &existing.child_chat_id,
                    config.limits.lease_ttl_ms,
                )?;
            }
            let view = self.refresh(&existing).await?;
            return Ok(SpawnedChild {
                child_chat_id: view.child_chat_id.clone(),
                turn_task_id: view.turn_task_id.clone(),
                status: view.status.clone(),
                role: view.role.clone(),
                depth: view.depth,
                model: view.model.clone(),
            });
        }

        // The store actor serializes single operations, but the cap checks
        // below and the child's row insert are separate messages — parallel
        // tool calls could both pass their count before either row became
        // visible and blow past the configured caps. This in-process gate
        // makes check→insert atomic; a multi-process host sharing one store
        // may still overshoot the (resource-bounding) caps by one wave.
        let _spawn_gate = self.spawn_gate.lock().await;
        // Re-probe under the gate: a concurrent same-key spawn (parallel
        // duplicate tool calls, or a retry racing the original) may have
        // created the row between the probe above and this lock. Take the
        // winner's child through the replay path instead of dying on the
        // register duplicate error — the child exists and is healthy.
        if self.store.get_subagent(&child_chat_id).await?.is_some() {
            drop(_spawn_gate);
            // Single-level recursion: the row exists, so the replayed call
            // takes the replay branch and returns.
            return Box::pin(self.spawn(request)).await;
        }
        self.check_fanout_caps(&request.parent_chat_id).await?;
        // The brief's references are resolved (and the parent's right to hand
        // each one on checked) before the child exists: a refused reference
        // leaves nothing behind.
        let context = self.resolve_context(request, workspace_root, true).await?;

        // Per-role execution limits ride the child chat metadata; the
        // executor reads them at dispatch (`subagents::task_overrides`), so
        // role budgets are durable task properties, not this process's
        // memory. The MODEL may steer a child's bounds, never widen them:
        // a host-set role bound is a ceiling the request cannot lift, and
        // the absolute caps keep a prompt-injected parent from minting
        // run-away children (`maxIterations` etc. are model-authored ints).
        let mut role_limits = Map::new();
        let iterations = match (request.brief.budget.max_iterations, role.max_iterations) {
            (Some(requested), Some(bound)) => Some(requested.min(bound)),
            (Some(requested), None) => Some(requested),
            (None, Some(bound)) => Some(bound),
            (None, None) => None,
        }
        .map(|value| value.clamp(1, SUBAGENT_ITERATIONS_CEILING));
        if let Some(iterations) = iterations {
            role_limits.insert("maxToolIterations".into(), json!(iterations));
        }
        let requested_requests = request
            .brief
            .budget
            .max_provider_requests
            .map(|requests| requests.clamp(1, SUBAGENT_PROVIDER_REQUESTS_CEILING));
        let tree_root =
            budget::root_of(&self.store, &request.parent_chat_id, limits.max_depth).await?;
        // Compute→insert is atomic only because this runs under the same
        // in-process `spawn_gate` as `check_fanout_caps`; the store holds an
        // exclusive per-file owner lock, so one process owns the database
        // and no other writer can interleave a reservation.
        let reserved_requests = match limits.max_tree_provider_requests {
            Some(tree_limit) => Some(
                budget::reserve_for_spawn(
                    &self.store,
                    &tree_root,
                    tree_limit,
                    requested_requests.unwrap_or(SUBAGENT_PROVIDER_REQUESTS_CEILING),
                )
                .await?,
            ),
            None => requested_requests,
        };
        if let Some(requests) = reserved_requests {
            role_limits.insert("maxProviderRequests".into(), json!(requests));
        }
        if let Some(overall) = request.brief.budget.timeout_ms {
            role_limits.insert(
                "overallTimeoutMs".into(),
                json!(overall.clamp(1_000, SUBAGENT_OVERALL_MS_CEILING)),
            );
        }
        let tokens = request
            .brief
            .budget
            .max_tokens
            .unwrap_or(SUBAGENT_TOKENS_DEFAULT);
        role_limits.insert(
            "maxTokens".into(),
            json!(tokens.clamp(1, SUBAGENT_TOKENS_CEILING)),
        );
        let role_snapshot = RoleSnapshot::from_role(&role).with_effective_tools(Some(effective));
        let brief_digest = request.brief.digest();
        let subagent_metadata = json!({
            "agentkit.subagent": {
                "role": role.id,
                "roleSnapshot": role_snapshot,
                "roleDigest": role_snapshot.digest(),
                "parentChatId": request.parent_chat_id,
                "parentTaskId": request.parent_task_id,
                "rootChatId": tree_root,
                "depth": depth + 1,
                "spawnKey": request.spawn_key,
                "briefDigest": brief_digest,
                "exclusive": exclusive,
                "limits": Value::Object(role_limits),
                // The only parent outputs this child may read (DEL-02);
                // the key is `OUTPUT_GRANTS_METADATA_KEY`.
                "outputGrants": context.grants,
            }
        });
        // A crash can land between creating the child session and
        // registering its row — the replay probe above then finds no row,
        // yet the chat exists with exactly this spawn's identity. Adopt it
        // instead of wedging forever on `create_session`'s
        // `input_conflict`; a chat that is NOT ours stays a hard error.
        // Lease admission runs BEFORE the child chat and row exist: a
        // refusal (`workspace_busy`) must not leave a ghost `queued` row
        // counted against the concurrency caps while nothing runs — every
        // model retry derives a fresh spawn key and would mint another.
        self.acquire_lease(
            workspace_root,
            exclusive,
            &child_chat_id,
            config.limits.lease_ttl_ms,
        )?;
        if let Some(chat) = self.store.get_chat(&child_chat_id).await? {
            let ours = chat.metadata.get("agentkit.subagent").is_some_and(|meta| {
                meta["parentChatId"].as_str() == Some(request.parent_chat_id.as_str())
                    && meta["parentTaskId"].as_str() == Some(request.parent_task_id.as_str())
                    && meta["spawnKey"].as_str() == Some(request.spawn_key.as_str())
            });
            if !ours {
                self.leases.release(&child_chat_id);
                return Err(AgentsError::ChildSessionConflict(child_chat_id.clone()));
            }
            if let Err(error) = self.require_same_brief(&child_chat_id, request).await {
                self.leases.release(&child_chat_id);
                return Err(error);
            }
        } else {
            self.session
                .create_session_with_host_metadata(&CreateSessionInput {
                    session_id: Some(child_chat_id.clone()),
                    title: Some(request.brief.title.clone()),
                    profile_id: descriptor["profile"]["id"]
                        .as_str()
                        .unwrap_or("coding")
                        .into(),
                    profile_digest: descriptor["profile"]["digest"].as_str().map(str::to_owned),
                    provider_id: provider_id.clone(),
                    model: child_model.clone(),
                    reasoning_effort: descriptor
                        .get("reasoningEffort")
                        .cloned()
                        .map(serde_json::from_value)
                        .transpose()
                        .map_err(|_| AgentsError::InvalidReasoningEffort)?,
                    content_selection: None,
                    workspace: workspace.clone(),
                    context: descriptor
                        .get("context")
                        .filter(|value| value.is_object())
                        .cloned(),
                    skills: None,
                    turn_kind: None,
                    execution_mode: execution_mode.clone(),
                    metadata: Some(subagent_metadata),
                })
                .await
                .map_err(session_error)
                .inspect_err(|_| {
                    self.leases.release(&child_chat_id);
                })?;
        }

        // Authoritative cap check: count + insert in ONE store job, so a
        // second process sharing the store cannot slip past the gate's
        // in-process check→insert window.
        let registered = self
            .store
            .register_subagent_if_under(
                RegisterSubagent {
                    child_chat_id: child_chat_id.clone(),
                    parent_chat_id: request.parent_chat_id.clone(),
                    parent_task_id: Some(request.parent_task_id.clone()),
                    role: role.id.clone(),
                    depth: depth + 1,
                    prompt: request.brief.objective.clone(),
                    model: child_model.clone(),
                    now: self.now_ms(),
                    turn_task_id: None,
                },
                limits.max_concurrency,
                limits.max_per_parent,
            )
            .await
            .inspect_err(|_| {
                self.leases.release(&child_chat_id);
            })?;
        if !registered {
            self.leases.release(&child_chat_id);
            self.check_fanout_caps(&request.parent_chat_id).await?;
            return Err(AgentsError::RegistrationConcurrencyLimit);
        }

        // Parent-liveness recheck immediately before dispatch: the parent
        // may have settled since the first check, and the settle observer
        // only reaps rows that existed when it scanned — this row is newer
        // than the sweep, so without this window-close the child would run
        // orphaned until the next restart.
        let parent = self.store.get_task(&request.parent_task_id).await?;
        if parent.is_none_or(|parent| parent.status.is_terminal()) {
            self.leases.release(&child_chat_id);
            self.store
                .update_subagent(
                    &child_chat_id,
                    "cancelled",
                    None,
                    Some("parent turn settled before the child was dispatched"),
                    self.now_ms(),
                )
                .await?;
            return Err(AgentsError::ChildCancelledAfterParent);
        }

        self.dispatch_initial_turn(&child_chat_id, &role, request, workspace_root)
            .await
            .inspect_err(|_| {
                // The row stays QUEUED for an honest replay; the replay
                // re-admits its lease, so do not hold the root meanwhile.
                self.leases.release(&child_chat_id);
            })?;
        let record = self
            .store
            .get_subagent(&child_chat_id)
            .await?
            .ok_or_else(|| AgentsError::SubagentRowVanished)?;
        let view = self.refresh(&record).await?;
        Ok(SpawnedChild {
            child_chat_id: view.child_chat_id.clone(),
            turn_task_id: view.turn_task_id.clone(),
            status: view.status.clone(),
            role: view.role.clone(),
            depth: view.depth,
            model: view.model.clone(),
        })
    }

    /// A replay of a spawn key re-admits the child it made, so it must carry
    /// the brief that child was made from. A child that predates the stored
    /// digest has nothing to compare and replays as before.
    async fn require_same_brief(
        &self,
        child_chat_id: &str,
        request: &SpawnRequest,
    ) -> AgentsResult<()> {
        let stored = self.store.get_chat(child_chat_id).await?.and_then(|chat| {
            chat.metadata[SUBAGENT_METADATA_KEY]["briefDigest"]
                .as_str()
                .map(str::to_owned)
        });
        match stored {
            Some(stored) if stored != request.brief.digest() => {
                Err(AgentsError::SpawnKeyConflict(format!(
                    "spawn_key_conflict: spawn key {} already made child {child_chat_id} from a \
                     different brief; use a new spawnKey for different work",
                    request.spawn_key
                )))
            }
            _ => Ok(()),
        }
    }

    /// Submit the child's first turn. The submission is deterministic —
    /// the input id is derived from the child id, so any replay (crash
    /// between register and dispatch, retry after the submission landed)
    /// re-drives the SAME submission and can never double-execute: the
    /// store answers `input_conflict`, and the pre-existing turn is simply
    /// re-attached to the row.
    async fn dispatch_initial_turn(
        &self,
        child_chat_id: &str,
        role: &RoleSpec,
        request: &SpawnRequest,
        workspace_root: Option<&str>,
    ) -> AgentsResult<()> {
        // A replay re-renders the same first message: the grants were taken
        // at spawn, so they are not asked for again.
        let context = self.resolve_context(request, workspace_root, false).await?;
        let limits = self.child_budget_lines(child_chat_id).await?;
        let content = Value::String(render_brief(
            &role.prompt,
            &request.brief,
            &context.rendered,
            &limits,
        ));
        let input_id = format!(
            "subin:{}",
            &sha256_hex(format!("{child_chat_id}:turn"))[..24]
        );
        let send = match self
            .session
            .send(
                child_chat_id,
                &SendInput {
                    frozen_payload: None,
                    content,
                    input_id: Some(input_id.clone()),
                    when_busy: WhenBusy::Queue,
                    turn_id: None,
                    on_settled: None,
                },
            )
            .await
        {
            Ok(result) => Some(result),
            Err(agentkit_sessions::SessionsError::SessionStore(
                agentkit_sessions::SessionStoreError(agentkit_store::StoreError::Duplicate(_)),
            )) => None,
            Err(err) => return Err(session_error(err)),
        };
        match send {
            Some(AgentSendResult::Turn { turn_id, .. }) => {
                self.store
                    .set_subagent_turn(child_chat_id, &turn_id)
                    .await?;
            }
            Some(AgentSendResult::Queued { .. }) => {
                self.store
                    .update_subagent(child_chat_id, "queued", None, None, self.now_ms())
                    .await?;
            }
            None => {
                // The submission predates the crash; the store derives the
                // turn task id from the input id, so the pre-existing turn
                // is locatable without a scan.
                let turn_id = format!("turn:{input_id}");
                if let Some(task) = self.store.get_task(&turn_id).await?
                    && task.scope_id == child_chat_id
                {
                    self.store
                        .set_subagent_turn(child_chat_id, &turn_id)
                        .await?;
                }
            }
        }
        Ok(())
    }

    // -- brief context ----------------------------------------------------------

    /// The output store over this service's database, when the store has an
    /// output root.
    fn output_store(&self) -> AgentsResult<SqliteOutputStore> {
        SqliteOutputStore::new(self.store.clone(), self.runtime.clone()).map_err(|error| {
            AgentsError::ContextRefRefused(format!(
                "context_ref_refused: output handles are unavailable here ({error})"
            ))
        })
    }

    /// Resolve the brief's references into what the first message shows and
    /// the output handles the child is granted. With `enforce`, the parent's
    /// own right to hand each reference on is checked: it must be able to
    /// read the output handle, the path must lie inside the inherited
    /// workspace, and the message must be one of its own user or assistant
    /// messages. A refusal never says whether a handle exists.
    async fn resolve_context(
        &self,
        request: &SpawnRequest,
        workspace_root: Option<&str>,
        enforce: bool,
    ) -> AgentsResult<ResolvedContext> {
        let mut resolved = ResolvedContext::default();
        for reference in &request.brief.context_refs {
            let mut rendered = RenderedRef {
                kind: reference.kind,
                reference: reference.reference.clone(),
                note: reference.note.clone(),
                text: None,
                cut: false,
            };
            match reference.kind {
                RefKind::Output => {
                    if enforce {
                        self.require_output_access(&request.parent_chat_id, reference)
                            .await?;
                    }
                    resolved.grants.push(reference.reference.clone());
                }
                RefKind::Path => {
                    let root = workspace_root.ok_or_else(|| {
                        AgentsError::ContextRefRefused(format!(
                            "context_ref_refused: path {} needs a workspace, and this session \
                             has none",
                            reference.reference
                        ))
                    })?;
                    rendered.reference =
                        confine_to_workspace(root, &reference.reference).map_err(|reason| {
                            AgentsError::ContextRefRefused(format!("context_ref_refused: {reason}"))
                        })?;
                }
                RefKind::Memory => {
                    if enforce {
                        self.require_memory_access(&request.parent_chat_id, reference)
                            .await?;
                    }
                }
                RefKind::Message => {
                    let (text, owner_text) = self
                        .parent_message_text(&request.parent_chat_id, &reference.reference)
                        .await?;
                    let (text, cut) = cap_message_text(&text);
                    // Only the owner's own words are copied as they are; the
                    // assistant's text may echo a child's, so it is wrapped
                    // as untrusted (after the cap, so the cut cannot lose
                    // the closing tag).
                    rendered.text = Some(if owner_text {
                        text
                    } else {
                        untrusted_subagent_block(&reference.reference, "assistant", &text)
                    });
                    rendered.cut = cut;
                }
            }
            resolved.rendered.push(rendered);
        }
        Ok(resolved)
    }

    /// The parent may only grant a handle it can read itself: its own, a
    /// descendant's, or one its own brief granted it.
    async fn require_output_access(
        &self,
        parent_chat_id: &str,
        reference: &ContextRef,
    ) -> AgentsResult<()> {
        let refusal = || {
            AgentsError::ContextRefRefused(format!(
                "context_ref_refused: no output {} is readable from this session",
                reference.reference
            ))
        };
        let handle = OutputHandle::parse(&reference.reference).map_err(|_| refusal())?;
        let outputs = self.output_store()?;
        let info = outputs.info(&handle).await.map_err(|_| refusal())?;
        if info.meta.session != parent_chat_id
            && !outputs
                .may_read_handle(parent_chat_id, &info.meta.session, &handle)
                .await
        {
            return Err(refusal());
        }
        Ok(())
    }

    /// The parent may only hand on a memory record it can read itself, and
    /// only when a memory service is configured. A refusal never says
    /// whether the record exists.
    async fn require_memory_access(
        &self,
        parent_chat_id: &str,
        reference: &ContextRef,
    ) -> AgentsResult<()> {
        let Some(reader) = &self.memory_refs else {
            return Err(AgentsError::ContextRefRefused(
                "context_ref_refused: memory references need a memory service, and none is \
                 configured"
                    .to_owned(),
            ));
        };
        if !reader.can_read(parent_chat_id, &reference.reference).await {
            return Err(AgentsError::ContextRefRefused(format!(
                "context_ref_refused: no memory record {} is readable from this session",
                reference.reference
            )));
        }
        Ok(())
    }

    /// The text of one user or assistant message of the parent session, and
    /// whether it is the owner's own (a user message). A record stamped as
    /// child-derived is not offered at all.
    async fn parent_message_text(
        &self,
        parent_chat_id: &str,
        message_id: &str,
    ) -> AgentsResult<(String, bool)> {
        let records = self.store.messages(parent_chat_id, false).await?;
        records
            .iter()
            .find(|record| {
                record.id == message_id
                    && matches!(record.role.as_str(), "user" | "assistant")
                    && !is_child_derived_message(&record.metadata)
            })
            .map(|record| {
                let text = match &record.content {
                    Value::String(text) => text.clone(),
                    other => other.to_string(),
                };
                (text, record.role == "user")
            })
            .ok_or_else(|| {
                AgentsError::ContextRefRefused(format!(
                    "context_ref_refused: message {message_id} is not a user or assistant \
                     message of this session"
                ))
            })
    }

    /// The limits the child runs under, read from its durable metadata, for
    /// the budget section of the first message.
    async fn child_budget_lines(&self, child_chat_id: &str) -> AgentsResult<BudgetLines> {
        let limits = self
            .store
            .get_chat(child_chat_id)
            .await?
            .map(|chat| chat.metadata[SUBAGENT_METADATA_KEY]["limits"].clone())
            .unwrap_or(Value::Null);
        Ok(BudgetLines {
            max_iterations: limits["maxToolIterations"].as_u64(),
            max_provider_requests: limits["maxProviderRequests"].as_u64(),
            timeout_ms: limits["overallTimeoutMs"].as_u64(),
            max_tokens: limits["maxTokens"].as_u64(),
        })
    }

    // -- queries ----------------------------------------------------------------

    pub async fn list(&self, parent_chat_id: &str) -> AgentsResult<Vec<SubagentView>> {
        let rows = self.store.list_subagents(parent_chat_id).await?;
        let mut views = Vec::with_capacity(rows.len());
        for row in &rows {
            views.push(self.refresh(row).await?);
        }
        Ok(views)
    }

    /// The authority a child may exercise right now: its spawn-time role
    /// snapshot narrowed by the current configuration. `Err(reason)` is a
    /// fail-closed denial (role gone, snapshot malformed).
    pub async fn effective_role_of(
        &self,
        row: &SubagentRecord,
        config: &SubagentConfig,
    ) -> AgentsResult<Result<EffectiveRole, RoleDenial>> {
        let chat = self.store.get_chat(&row.child_chat_id).await?;
        let snapshot = match chat
            .as_ref()
            .and_then(|chat| chat.metadata.get("agentkit.subagent"))
            .map(RoleSnapshot::from_metadata)
            .transpose()
        {
            Ok(snapshot) => snapshot.flatten(),
            Err(reason) => return Ok(Err(reason.into())),
        };
        Ok(effective_role(snapshot.as_ref(), config.role(&row.role))
            .ok_or_else(|| RoleDenial::RoleGone(row.role.clone())))
    }

    pub async fn status(&self, child_chat_id: &str) -> AgentsResult<SubagentView> {
        let row = self
            .store
            .get_subagent(child_chat_id)
            .await?
            .ok_or_else(|| AgentsError::SubagentNotFound(child_chat_id.to_owned()))?;
        self.refresh(&row).await
    }

    pub async fn message(
        &self,
        child_chat_id: &str,
        content: Value,
        mode: &str,
        message_key: &str,
    ) -> AgentsResult<MessageReceipt> {
        let row = self.require_row(child_chat_id).await?;
        // Lifecycle gate: re-project before dispatch. A terminal row is
        // frozen by the store contract, so a follow-up that "succeeded"
        // behind it would run a hidden turn: uncounted by the concurrency
        // cap, invisible to `await`, and (for writer roles) holding no
        // workspace lease. `refresh` persists the projection first, which
        // also closes the stale-`running` window where the turn task has
        // already settled but the row has not been observed since.
        // Revival of a settled child is `resume`'s job (failed/cancelled/
        // interrupted turns only); new work goes to a new child.
        let view = self.refresh(&row).await?;
        if is_terminal_status(&view.status) {
            return Err(AgentsError::SubagentTerminal(format!(
                "subagent_terminal: {child_chat_id} is {}; message is for active children \
                 only — resume restarts a failed or cancelled child, otherwise spawn a new one",
                view.status
            )));
        }
        let input_id = format!(
            "submsg:{}",
            &sha256_hex(format!("{child_chat_id}:{message_key}"))[..24]
        );
        let result = match mode {
            "steer" => {
                self.session
                    .steer(
                        child_chat_id,
                        &SteerInput {
                            frozen_payload: None,
                            content,
                            input_id: Some(input_id),
                            turn_id: view.turn_task_id.clone(),
                            on_settled: None,
                        },
                    )
                    .await
            }
            _ => {
                self.session
                    .follow_up(
                        child_chat_id,
                        &FollowUpInput {
                            frozen_payload: None,
                            content,
                            input_id: Some(input_id),
                        },
                    )
                    .await
            }
        }
        .map_err(session_error)?;
        let (delivered, turn) = match result {
            AgentSendResult::Turn { turn_id, .. } => {
                self.store
                    .set_subagent_turn(child_chat_id, &turn_id)
                    .await?;
                ("turn".to_owned(), Some(turn_id))
            }
            AgentSendResult::Queued { .. } => ("queued".to_owned(), row.turn_task_id.clone()),
        };
        Ok(MessageReceipt {
            child_chat_id: child_chat_id.to_owned(),
            delivered,
            turn_task_id: turn,
        })
    }

    /// Poll children to terminal (or cancellation/timeout). The poll reads
    /// the durable store, so this stays correct while the host's workers —
    /// the ONLY executors — advance the children.
    pub async fn await_children(
        &self,
        parent_chat_id: &str,
        ids: Option<&[String]>,
        timeout_ms: Option<u64>,
        cancellation: &CancellationToken,
    ) -> AgentsResult<AwaitOutcome> {
        self.await_children_mode(
            parent_chat_id,
            ids,
            timeout_ms,
            AwaitMode::All,
            cancellation,
        )
        .await
    }

    /// The awaited rows: the named children (all must be this parent's) or
    /// every still-active child.
    async fn await_rows(
        &self,
        parent_chat_id: &str,
        ids: Option<&[String]>,
    ) -> AgentsResult<Vec<SubagentRecord>> {
        Ok(match ids {
            Some(ids) if !ids.is_empty() => {
                let mut rows = Vec::with_capacity(ids.len());
                for id in ids {
                    rows.push(self.require_row(id).await?);
                }
                if rows.iter().any(|row| row.parent_chat_id != parent_chat_id) {
                    return Err(AgentsError::ForeignChildAwait);
                }
                rows
            }
            _ => self
                .store
                .list_subagents(parent_chat_id)
                .await?
                .into_iter()
                .filter(|row| !is_terminal_status(&row.status))
                .collect(),
        })
    }

    /// [`Self::await_children`] with an explicit wake mode: `Any` returns
    /// at the first settled child.
    pub async fn await_children_mode(
        &self,
        parent_chat_id: &str,
        ids: Option<&[String]>,
        timeout_ms: Option<u64>,
        mode: AwaitMode,
        cancellation: &CancellationToken,
    ) -> AgentsResult<AwaitOutcome> {
        let config = self.config();
        let poll = config.limits.await_poll_ms;
        let timeout = timeout_ms
            .unwrap_or(config.limits.max_await_ms)
            .min(config.limits.max_await_ms);
        let deadline = self.now_ms() + timeout as i64;
        let mut rows = self.await_rows(parent_chat_id, ids).await?;
        let mut views: Vec<SubagentView> = Vec::with_capacity(rows.len());
        loop {
            if cancellation.is_cancelled() {
                return Err(AgentsError::Cancelled);
            }
            // Reload every row from the store each tick: refreshing stale
            // snapshots re-persists unchanged state (update churn) and
            // re-scans history for already-settled children's answers.
            // A terminal row whose durable fingerprint has not moved since
            // the previous tick keeps its view — any revival (rebind or
            // reactivate) moves `updated_at`/`turn_task_id`, invalidating it.
            let mut fresh_rows = Vec::with_capacity(rows.len());
            let mut fresh_views = Vec::with_capacity(rows.len());
            for (index, row) in rows.iter().enumerate() {
                let fresh = self
                    .store
                    .get_subagent(&row.child_chat_id)
                    .await?
                    .ok_or_else(|| {
                        AgentsError::SubagentRowVanishedDetail(row.child_chat_id.clone())
                    })?;
                let cached = views.get(index);
                let view = match cached {
                    Some(previous)
                        if is_terminal_status(&fresh.status)
                            && fresh.turn_task_id == previous.turn_task_id
                            && fresh.updated_at == previous.updated_at =>
                    {
                        previous.clone()
                    }
                    _ => self.refresh(&fresh).await?,
                };
                fresh_rows.push(fresh);
                fresh_views.push(view);
            }
            rows = fresh_rows;
            views = fresh_views;
            if mode.satisfied(&views) {
                return Ok(AwaitOutcome {
                    settled: true,
                    timed_out: false,
                    results: self.await_results(&views).await?,
                });
            }
            if self.now_ms() >= deadline {
                return Ok(AwaitOutcome {
                    settled: false,
                    timed_out: true,
                    results: self.await_results(&views).await?,
                });
            }
            // Bound each nap so a parent cancellation is felt promptly.
            tokio::select! {
                _ = cancellation.cancelled() => return Err(AgentsError::Cancelled),
                _ = tokio::time::sleep(std::time::Duration::from_millis(poll)) => {}
            }
        }
    }

    /// Decide how a durable parent turn awaits: answer now when the wait is
    /// already satisfied (or `timeout` is 0), otherwise build the request the
    /// turn parks on. The parked turn holds no engine slot; the waker
    /// resolves it (`wake_parked_parents`).
    pub async fn plan_await(
        &self,
        parent_chat_id: &str,
        parent_task_id: &str,
        request_id: &str,
        ids: Option<&[String]>,
        timeout_ms: Option<u64>,
        mode: AwaitMode,
    ) -> AgentsResult<AwaitDecision> {
        let max_await_ms = self.config().limits.max_await_ms;
        let timeout = timeout_ms.unwrap_or(max_await_ms).min(max_await_ms);
        let rows = self.await_rows(parent_chat_id, ids).await?;
        let mut views = Vec::with_capacity(rows.len());
        for row in &rows {
            views.push(self.refresh(row).await?);
        }
        let satisfied = mode.satisfied(&views);
        if satisfied || timeout == 0 {
            return Ok(AwaitDecision::Ready(AwaitOutcome {
                settled: satisfied,
                timed_out: !satisfied,
                results: self.await_results(&views).await?,
            }));
        }
        let awaited: Vec<String> = views
            .iter()
            .map(|view| view.child_chat_id.clone())
            .collect();
        let deadline_ms = self.now_ms() + timeout as i64;
        let content = json!({
            "kind": CHILD_WAIT_KIND,
            "toolName": "subagent_await",
            "prompt": format!("Waiting for {} subagent(s).", awaited.len()),
            "awaited": awaited,
            "mode": mode.as_str(),
            // Durable: a restart re-evaluates against this absolute time.
            "deadlineMs": deadline_ms,
            "parentTaskId": parent_task_id,
        });
        let content_hash = sha256_hex(content.to_string());
        self.ensure_waker();
        self.waker.nudge.notify_one();
        // The expiry is the deadline: the store treats it as the timeout,
        // never as a stale request, so a parent cannot park forever.
        let expires_at = ConversationTimestamp::from_epoch_millis(deadline_ms)
            .ok()
            .and_then(|stamp| stamp.wire_text().ok().map(|text| text.into_owned()));
        Ok(AwaitDecision::Park(HumanRequestSpec {
            request_id: format!("{CHILD_WAIT_ID_PREFIX}{request_id}"),
            content,
            content_hash,
            expires_at,
        }))
    }

    /// Start the background waker once per service family. It re-evaluates
    /// every parked parent each poll tick (children settling, the durable
    /// deadline, an owner message), so a wake never depends on an in-process
    /// event and a restarted process resumes the same waits. While nothing
    /// is parked it backs off (a park or a settle nudges it awake) and does
    /// not scan. It ends when every other clone of the service is gone.
    pub fn ensure_waker(&self) {
        let Some(owner) = &self.owner else {
            return;
        };
        if self.waker.started.swap(true, Ordering::SeqCst) {
            return;
        }
        let weak = Arc::downgrade(owner);
        let waker = self.waker.clone();
        let handle = tokio::spawn(async move {
            let mut idle: u32 = 0;
            while let Some(poll) = weak
                .upgrade()
                .map(|owner| owner.core.config().limits.await_poll_ms.max(10))
            {
                let delay = std::time::Duration::from_millis(poll << idle.min(3));
                tokio::select! {
                    _ = tokio::time::sleep(delay) => {}
                    _ = waker.nudge.notified() => idle = 0,
                    _ = waker.stop.notified() => break,
                }
                if waker.stopped.load(Ordering::SeqCst) {
                    break;
                }
                // Strong only for the tick: the loop never pins the store.
                let Some(owner) = weak.upgrade() else {
                    break;
                };
                match owner.core.sweep_parked().await {
                    Ok(Some(_)) => idle = 0,
                    Ok(None) => idle = idle.saturating_add(1),
                    Err(error) => waker.note_failure("sweep", &error),
                }
            }
        });
        *self
            .waker
            .task
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(handle);
    }

    /// Stop the waker and wait until its task has exited, so the store is
    /// no longer referenced by it. Idempotent; a stopped waker never
    /// restarts. The host calls this on shutdown.
    pub async fn shutdown_waker(&self) {
        self.waker.request_stop();
        let handle = self
            .waker
            .task
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(handle) = handle {
            let _ = handle.await;
        }
    }

    /// Wake sweeps that failed since the service started (health counter).
    #[must_use]
    pub fn waker_failures(&self) -> u64 {
        self.waker.failures.load(Ordering::Relaxed)
    }

    /// Evaluate every parked parent against its wake conditions and resume
    /// the ones whose condition holds. Idempotent: the answer transaction
    /// resolves a wait once, and a parent that is cancelled or already
    /// resumed is skipped. Returns how many parents were woken.
    pub async fn wake_parked_parents(&self) -> AgentsResult<usize> {
        Ok(self.sweep_parked().await?.unwrap_or(0))
    }

    /// One waker tick: `None` when no turn is parked (nothing was scanned),
    /// else how many parents woke. A cancelled parent never reaches here:
    /// the store reaps its children as it cancels the parked turn.
    async fn sweep_parked(&self) -> AgentsResult<Option<usize>> {
        if self
            .store
            .count_tasks_by_status(TaskStatus::WaitingChild)
            .await?
            == 0
        {
            return Ok(None);
        }
        let parked = self
            .store
            .list_tasks_by_status(TaskStatus::WaitingChild)
            .await?;
        let mut woken = 0usize;
        let mut failed = false;
        for task in parked
            .iter()
            .filter(|task| task.kind == CHAT_TURN_TASK_KIND)
        {
            match self.wake_parked(task).await {
                Ok(true) => woken += 1,
                Ok(false) => {}
                Err(error) => {
                    failed = true;
                    self.waker
                        .note_failure(&format!("parked parent {}", task.task_id), &error);
                }
            }
        }
        if !failed {
            self.waker.warned.store(false, Ordering::Relaxed);
        }
        Ok(Some(woken))
    }

    async fn wake_parked(&self, task: &TaskRecord) -> AgentsResult<bool> {
        let pending = self
            .store
            .list_inputs(AgentInputQuery {
                session_id: task.scope_id.clone(),
                kinds: Some(vec![AgentInputKind::HumanRequest]),
                statuses: Some(vec![AgentInputStatus::Pending]),
                target_task_id: Some(task.task_id.clone()),
            })
            .await?;
        let Some(request) = pending
            .into_iter()
            .find(|request| is_child_wait(&request.input_id, &request.content))
        else {
            return Ok(false);
        };
        let mode = AwaitMode::from_arg(request.content["mode"].as_str()).unwrap_or_default();
        // Sorted and unique: concurrent sweeps compute the same response.
        let mut awaited: Vec<String> = request.content["awaited"]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        awaited.sort();
        awaited.dedup();
        let deadline = request.content["deadlineMs"].as_i64().unwrap_or(i64::MAX);
        let mut views = Vec::with_capacity(awaited.len());
        let mut results = Vec::with_capacity(awaited.len());
        for id in &awaited {
            match self.store.get_subagent(id).await? {
                Some(row) => {
                    let view = self.refresh(&row).await?;
                    results.push(self.await_result(&view).await?);
                    views.push(view);
                }
                None => results.push(missing_result(id)),
            }
        }
        // A missing row can never settle: it is unsettled until the timeout.
        let satisfied = views.len() == awaited.len() && mode.satisfied(&views);
        let reason = if satisfied {
            match mode {
                AwaitMode::All => "all_settled",
                AwaitMode::Any => "first_settled",
            }
        } else if self.owner_message_pending(&task.scope_id).await? {
            "owner_message"
        } else if self.now_ms() >= deadline {
            "timeout"
        } else {
            return Ok(false);
        };
        if reason != "timeout" {
            results.retain(|result| result.status != MISSING_STATUS);
        }
        let response = json!({
            "wake": reason,
            "settled": satisfied,
            "timedOut": reason == "timeout",
            "results": results,
        });
        let response_hash = sha256_hex(response.to_string());
        let answered = self
            .store
            .answer_human_request(AnswerHumanRequest {
                request_id: request.input_id.clone(),
                answer_id: format!("wake:{}", request.input_id),
                response,
                response_hash,
                now: self.now_ms(),
                message_id: format!("msg_wake:{}", request.input_id),
                plan_tool_names: vec![],
                policy_denial: None,
                granted_mode: None,
                actor: AnswerActor::Waker,
            })
            .await;
        match answered {
            Ok(_) => Ok(true),
            // Cancelled, resolved, or answered differently by a concurrent
            // sweep: the wait is settled either way, so this is a no-op.
            Err(StoreError::AnswerStale(_) | StoreError::AnswerConflict(_)) => Ok(false),
            Err(error) => Err(error.into()),
        }
    }

    /// Whether the owner has a steer waiting in this chat (delivered when
    /// the turn resumes). A queued follow-up is for AFTER the turn, so it
    /// does not interrupt the wait.
    async fn owner_message_pending(&self, chat_id: &str) -> AgentsResult<bool> {
        Ok(!self
            .store
            .list_inputs(AgentInputQuery {
                session_id: chat_id.to_owned(),
                kinds: Some(vec![AgentInputKind::Steer]),
                statuses: Some(vec![AgentInputStatus::Pending]),
                target_task_id: None,
            })
            .await?
            .is_empty())
    }

    /// Cooperatively cancel the child's live turn AND its queued inputs.
    /// The store's durable cancel flag — not this call's success — is
    /// authoritative; the worker settles the task and `refresh` projects it
    /// onto the row.
    pub async fn cancel(&self, child_chat_id: &str) -> AgentsResult<SubagentView> {
        let row = self.require_row(child_chat_id).await?;
        // "Cancel this child" means its whole backlog: a durable queued
        // input left behind would be promoted by the runner's settle plan
        // and rebind would honestly resurrect the child the parent just
        // cancelled.
        self.session
            .clear_queue(child_chat_id)
            .await
            .map_err(session_error)?;
        // Re-project (rebinding a promoted turn if needed) BEFORE
        // cancelling: a settled tracked turn can hide a live promoted turn,
        // and cancelling only the stale id would leave the real work
        // unstoppable.
        let view = self.refresh(&row).await?;
        if let Some(turn_id) = view.turn_task_id.clone()
            && let Some(task) = self.store.get_task(&turn_id).await?
            && !task.status.is_terminal()
        {
            self.session
                .cancel(child_chat_id, &turn_id)
                .await
                .map_err(session_error)?;
        }
        // No eager lease release here: cancellation is cooperative and the
        // child may be mid-tool; the lease goes when `refresh` observes the
        // turn terminal (or steals it back), so a replacement child cannot
        // start writing beside the cancelled one.
        // Keep the registry projection honest even before the worker lands;
        // `update_subagent` is frozen-on-terminal, so a real settle that
        // raced ahead of this keeps its own terminal truth.
        self.store
            .update_subagent(
                child_chat_id,
                "cancelled",
                None,
                Some("cancel requested"),
                self.now_ms(),
            )
            .await?;
        self.status(child_chat_id).await
    }

    /// Resume a child: continue an interrupted turn in place, or re-dispatch
    /// the assignment after a failed/cancelled one (a fresh turn, so the
    /// durable engine — not this module — owns the new attempt).
    pub async fn resume(
        &self,
        child_chat_id: &str,
        resume_key: &str,
    ) -> AgentsResult<SubagentView> {
        self.resume_for_parent(child_chat_id, resume_key, None)
            .await
    }

    /// [`Self::resume`] on behalf of a live parent turn: a re-dispatch
    /// rebinds the row to `parent_task_id`, so the orphan reaper judges the
    /// revived child by the parent that revived it.
    pub async fn resume_for_parent(
        &self,
        child_chat_id: &str,
        resume_key: &str,
        parent_task_id: Option<&str>,
    ) -> AgentsResult<SubagentView> {
        // One resume at a time (and never beside a spawn): the row and turn
        // status are re-read AFTER the gate, so a concurrent resume that
        // already revived the child makes this one bail as not resumable.
        let _spawn_gate = self.spawn_gate.lock().await;
        let row = self.require_row(child_chat_id).await?;
        // Re-project first: a stale row must not decide what resumes.
        let view = self.refresh(&row).await?;
        let task = match view.turn_task_id.as_deref() {
            Some(turn_id) => self.store.get_task(turn_id).await?,
            None => None,
        };
        // The turn status is judged BEFORE any lease is taken: only the
        // paths that actually re-dispatch need the workspace.
        match task.as_ref().map(|task| task.status) {
            Some(TaskStatus::Interrupted) => {
                let turn_id = view.turn_task_id.clone().expect("checked above");
                let lease = self.admit_resume_lease(child_chat_id).await?;
                self.session
                    .resume(child_chat_id, &turn_id)
                    .await
                    .map_err(session_error)?;
                lease.keep();
                // The resumed turn may have been re-enqueued under a new
                // task id; keep the row on the live turn.
                if let Some(live) = self.live_child_turn(child_chat_id).await? {
                    self.bind_turn(child_chat_id, &live, parent_task_id).await?;
                }
            }
            Some(TaskStatus::Failed) | Some(TaskStatus::Cancelled) => {
                // A resume is a fresh admission: it runs under the same
                // gate and the same fan-out caps as `spawn`, so a cancelled
                // roster cannot be revived past the limits.
                self.check_resume_admission(&view, parent_task_id).await?;
                if let Some(tree_limit) = self.config().limits.max_tree_provider_requests {
                    let max_depth = self.config().limits.max_depth;
                    budget::check_resume(&self.store, child_chat_id, tree_limit, max_depth).await?;
                }
                let limits = self.config().limits;
                budget::check_resume_tokens(
                    &self.store,
                    child_chat_id,
                    limits.max_tree_tokens,
                    limits.max_depth,
                )
                .await?;
                let lease = self.admit_resume_lease(child_chat_id).await?;
                // Rebind unconditionally and before dispatch: the reaper must
                // never judge the revived child by the settled original
                // parent, whatever `follow_up` turns out to do.
                if let Some(parent_task_id) = parent_task_id {
                    self.store
                        .rebind_subagent_parent(child_chat_id, parent_task_id)
                        .await?;
                }
                let input_id = format!(
                    "subresume:{}",
                    &sha256_hex(format!("{child_chat_id}:{resume_key}"))[..24]
                );
                let status = task
                    .as_ref()
                    .map(|task| format!("{:?}", task.status))
                    .unwrap_or_default();
                let note = format!(
                    "Resume your previous assignment after it ended {status}{}.\nContinue from \
                     the current state and complete the original task; report the complete \
                     result.",
                    view.error
                        .as_deref()
                        .map(|error| format!(" (error: {error})"))
                        .unwrap_or_default(),
                );
                let sent = self
                    .session
                    .follow_up(
                        child_chat_id,
                        &FollowUpInput {
                            frozen_payload: None,
                            content: Value::String(note),
                            input_id: Some(input_id),
                        },
                    )
                    .await
                    .map_err(session_error)?;
                match sent {
                    AgentSendResult::Turn { turn_id, .. } => {
                        // The turn is durably dispatched: from here the
                        // lease belongs to the live child whatever the
                        // bookkeeping does.
                        lease.keep();
                        if let Some(task) = self.store.get_task(&turn_id).await? {
                            self.bind_turn(child_chat_id, &task, parent_task_id).await?;
                        }
                    }
                    // The input queued behind a turn that is already live:
                    // follow that turn so the row is not left frozen. With
                    // no live turn there is nothing to fence, so the lease
                    // is dropped rather than held with no owner.
                    AgentSendResult::Queued { .. } => {
                        if let Some(live) = self.live_child_turn(child_chat_id).await? {
                            lease.keep();
                            self.bind_turn(child_chat_id, &live, parent_task_id).await?;
                        }
                    }
                }
            }
            Some(status) => {
                return Err(AgentsError::SubagentTurnInvalid(format!(
                    "subagent turn is {status:?}; only interrupted, failed or cancelled turns resume"
                )));
            }
            None => {
                return Err(AgentsError::SubagentHasNoTurnToResume);
            }
        }
        self.status(child_chat_id).await
    }

    /// Point the row at `task` and rejoin the active ledger, while the
    /// caller still holds `spawn_gate`, so the next admission counts this
    /// child.
    async fn bind_turn(
        &self,
        child_chat_id: &str,
        task: &TaskRecord,
        parent_task_id: Option<&str>,
    ) -> AgentsResult<()> {
        self.store
            .set_subagent_turn(child_chat_id, &task.task_id)
            .await?;
        let projected = if task.status == TaskStatus::Queued {
            "queued"
        } else {
            "running"
        };
        self.store
            .reactivate_subagent_for_parent(child_chat_id, projected, self.now_ms(), parent_task_id)
            .await?;
        Ok(())
    }

    /// The active-roster caps `spawn` enforces, against DURABLE state.
    /// Callers hold `spawn_gate`.
    async fn check_fanout_caps(&self, parent_chat_id: &str) -> AgentsResult<()> {
        let config = self.config();
        let limits = &config.limits;
        if self.store.count_active_subagents().await? >= limits.max_concurrency {
            return Err(AgentsError::SubagentConcurrencyLimitDetail(format!(
                "subagent_concurrency_limit: {} active children (max {})",
                limits.max_concurrency, limits.max_concurrency
            )));
        }
        let siblings = self.store.list_subagents(parent_chat_id).await?;
        let active_siblings = siblings
            .iter()
            .filter(|row| !is_terminal_status(&row.status))
            .count();
        if active_siblings as i64 >= limits.max_per_parent {
            return Err(AgentsError::SubagentParentLimit(format!(
                "subagent_parent_limit: {} active children for this parent (max {})",
                active_siblings, limits.max_per_parent
            )));
        }
        Ok(())
    }

    /// Admission checks a re-dispatching resume must pass: depth, a live
    /// caller task (when named), and the fan-out caps.
    async fn check_resume_admission(
        &self,
        view: &SubagentView,
        parent_task_id: Option<&str>,
    ) -> AgentsResult<()> {
        let max_depth = self.config().limits.max_depth;
        if view.depth > max_depth {
            return Err(AgentsError::SubagentDepthExceeded(format!(
                "subagent_depth_exceeded: depth {} exceeds max {max_depth}",
                view.depth
            )));
        }
        if let Some(parent_task_id) = parent_task_id {
            let live = self
                .store
                .get_task(parent_task_id)
                .await?
                .is_some_and(|task| {
                    task.scope_id == view.parent_chat_id && !task.status.is_terminal()
                });
            if !live {
                return Err(AgentsError::ResumeParentTurnRequired);
            }
        }
        self.check_fanout_caps(&view.parent_chat_id).await
    }

    /// A resumed writer re-admits its exclusive lease before its turn runs
    /// again: the lease lapsed when the previous turn settled, and an
    /// unleased resume could mutate beside a fresh exclusive sibling. The
    /// returned guard releases a lease THIS call took if the resume then
    /// fails; `keep` disarms it on success.
    async fn admit_resume_lease(&self, child_chat_id: &str) -> AgentsResult<LeaseRelease> {
        let mut release = LeaseRelease {
            leases: self.leases.clone(),
            child_chat_id: child_chat_id.to_owned(),
            generation: None,
        };
        let Some(chat) = self.store.get_chat(child_chat_id).await? else {
            return Ok(release);
        };
        if !chat.metadata["agentkit.subagent"]["exclusive"]
            .as_bool()
            .unwrap_or(false)
        {
            return Ok(release);
        }
        let root = read_session_descriptor(&chat.metadata).and_then(|descriptor| {
            descriptor
                .get("workspace")?
                .get("root")?
                .as_str()
                .map(str::to_owned)
        });
        let held_before = root
            .as_deref()
            .is_some_and(|root| self.leases.is_held_by(root, child_chat_id, self.now_ms()));
        let ttl = self.config().limits.lease_ttl_ms;
        self.acquire_lease(root.as_deref(), true, child_chat_id, ttl)?;
        if root.is_some() && !held_before {
            release.generation = self.leases.generation_of(child_chat_id);
        }
        Ok(release)
    }

    /// Rebuild the in-memory exclusive leases from durable state after a
    /// restart: every active subagent whose chat metadata marks it
    /// exclusive and whose turn is live (queued/running/waiting approval)
    /// holds its workspace root again, so a restarted writer never runs
    /// unleased beside a fresh exclusive sibling. Idempotent; a root a
    /// foreign child already holds is left alone.
    pub async fn restore_leases(&self) -> AgentsResult<usize> {
        let ttl = self.config().limits.lease_ttl_ms;
        let mut restored = 0usize;
        for row in self.store.list_active_subagents().await? {
            let Some(chat) = self.store.get_chat(&row.child_chat_id).await? else {
                continue;
            };
            if !chat.metadata["agentkit.subagent"]["exclusive"]
                .as_bool()
                .unwrap_or(false)
            {
                continue;
            }
            let live = match row.turn_task_id.as_deref() {
                Some(turn_id) => self.store.get_task(turn_id).await?.is_some_and(|task| {
                    matches!(
                        task.status,
                        TaskStatus::Running
                            | TaskStatus::Queued
                            | TaskStatus::WaitingApproval
                            | TaskStatus::WaitingChild
                    )
                }),
                None => false,
            };
            if !live {
                continue;
            }
            let root = read_session_descriptor(&chat.metadata).and_then(|descriptor| {
                descriptor
                    .get("workspace")?
                    .get("root")?
                    .as_str()
                    .map(str::to_owned)
            });
            if let Some(root) = root
                && self
                    .leases
                    .try_acquire(Some(&root), true, &row.child_chat_id, self.now_ms(), ttl)
                    .is_ok()
            {
                restored += 1;
            }
        }
        Ok(restored)
    }

    // -- reaping ------------------------------------------------------------------

    /// Cancel children whose parent turn already settled. Runs at startup
    /// (crash recovery) and from the host's settle observers; both are
    /// idempotent because cancellation is cooperative and the store's CAS
    /// transitions reject double settles.
    pub async fn reap_orphans(&self) -> AgentsResult<usize> {
        // Startup recovery: parked parents from a previous life are
        // re-evaluated by the waker, never re-run.
        self.ensure_waker();
        let config = self.config();
        self.leases.expire(self.now_ms());
        // Startup path: the lease table is memory-only, so re-derive it from
        // the durable roster before anything is reaped or resumed.
        self.restore_leases().await?;
        if !config.limits.cancel_orphans {
            return Ok(0);
        }
        let rows = self.store.list_active_subagents().await?;
        let mut reaped = 0usize;
        for row in rows {
            let Some(parent_task_id) = row.parent_task_id.clone() else {
                continue;
            };
            let parent = self.store.get_task(&parent_task_id).await?;
            let orphan = match parent.as_ref() {
                Some(task) => task.status.is_terminal(),
                None => true,
            };
            if !orphan {
                continue;
            }
            self.cancel_row(&row).await?;
            reaped += 1;
        }
        Ok(reaped)
    }

    /// Observe one settled task: when a parent turn finishes, reap its
    /// still-running children (cooperative, durable).
    pub async fn observe_settled(&self, task: &TaskRecord) -> AgentsResult<()> {
        self.ensure_waker();
        self.waker.nudge.notify_one();
        if !task.status.is_terminal() {
            return Ok(());
        }
        // A child's own turn just settled: re-project now rather than at
        // the parent's next observation — this releases the lease promptly
        // and follows (rebind + reactivate) a queued input the runner's
        // settle plan promoted into a fresh turn.
        let config = self.config();
        if task.kind == CHAT_TURN_TASK_KIND
            && let Some(row) = self.store.get_subagent(&task.scope_id).await?
        {
            budget::settle_turn(&self.store, task, self.now_ms()).await?;
            let view = self.refresh(&row).await?;
            // If the parent already settled, the reactivated child is
            // reaped here (policy-gated like every other reap); the
            // alternative is an orphan running uncounted until the next
            // restart sweep.
            if config.limits.cancel_orphans
                && !is_terminal_status(&view.status)
                && let Some(parent_task_id) = row.parent_task_id.as_deref()
            {
                let parent = self.store.get_task(parent_task_id).await?;
                if parent.is_none_or(|parent| parent.status.is_terminal()) {
                    let fresh = self
                        .store
                        .get_subagent(&row.child_chat_id)
                        .await?
                        .unwrap_or(row.clone());
                    self.cancel_row(&fresh).await?;
                }
            }
        }
        if !config.limits.cancel_orphans {
            return Ok(());
        }
        let children = self.store.list_subagents(&task.scope_id).await?;
        for row in children
            .iter()
            .filter(|row| !is_terminal_status(&row.status))
            .filter(|row| row.parent_task_id.as_deref() == Some(task.task_id.as_str()))
        {
            self.cancel_row(row).await?;
        }
        Ok(())
    }

    async fn cancel_row(&self, row: &SubagentRecord) -> AgentsResult<()> {
        // Drop the child's not-yet-dispatched inputs too: a promoted turn
        // after this cancel would rebind the row back to life.
        let _ = self.session.clear_queue(&row.child_chat_id).await;
        // Same order as public `cancel`: re-project first. After a crash
        // between child admission and `set_subagent_turn` the row has no
        // turn, and after a follow-up promotion it names the settled
        // previous turn — cancelling only that stale id would leave the
        // real child turn to run after restart.
        let view = self.refresh(row).await?;
        let mut live_turn = false;
        if let Some(turn_id) = view.turn_task_id.clone()
            && let Some(task) = self.store.get_task(&turn_id).await?
            && !task.status.is_terminal()
        {
            live_turn = true;
            let _ = self
                .session
                .cancel(&row.child_chat_id, &turn_id)
                .await
                .map_err(session_error);
        }
        self.store
            .update_subagent(
                &row.child_chat_id,
                "cancelled",
                None,
                Some("parent turn settled before the child finished"),
                self.now_ms(),
            )
            .await?;
        // Cancellation is cooperative: a live turn may be mid-tool, so its
        // exclusive lease stays until `refresh` observes the turn terminal
        // (or the lease TTL lapses). With no live turn there is nothing
        // left to fence.
        if !live_turn {
            self.leases.release(&row.child_chat_id);
        }
        Ok(())
    }

    // -- projection -------------------------------------------------------------

    /// The child session's most recently enqueued live chat turn, if any.
    /// The runner's settle plan promotes queued inputs into a fresh
    /// `chat.turn` task without telling the registry; this probe is how
    /// `refresh` finds that promoted turn so the row can follow it.
    async fn live_child_turn(&self, child_chat_id: &str) -> AgentsResult<Option<TaskRecord>> {
        let tasks = self
            .store
            .list_tasks(Some(child_chat_id.to_owned()))
            .await?;
        Ok(tasks
            .into_iter()
            .filter(|task| {
                task.kind == CHAT_TURN_TASK_KIND
                    && !task.status.is_terminal()
                    && !task.cancel_requested
            })
            .max_by_key(|task| task.enqueued_at))
    }

    /// Join the registry row with the authoritative task/session state,
    /// persisting the projection when it moved. The task store holds the
    /// truth; the row is a cache with durable memory.
    pub async fn refresh(&self, row: &SubagentRecord) -> AgentsResult<SubagentView> {
        let mut row = row.clone();
        let mut task = match row.turn_task_id.as_deref() {
            Some(turn_id) => self.store.get_task(turn_id).await?,
            None => None,
        };
        // Rebind: the tracked turn has settled (or was never bound) while
        // the child session is executing another chat turn — the runner's
        // settle plan promoted a queued input, or a re-dispatch landed
        // between registry writes. Following the live turn and reactivating
        // the frozen row is the only way the child stays visible to the
        // concurrency caps, the orphan reaper, `await` and `cancel`.
        if task.as_ref().is_none_or(|task| task.status.is_terminal())
            && let Some(live) = self.live_child_turn(&row.child_chat_id).await?
        {
            self.store
                .set_subagent_turn(&row.child_chat_id, &live.task_id)
                .await?;
            self.store
                .reactivate_subagent(&row.child_chat_id, "running", self.now_ms())
                .await?;
            row.turn_task_id = Some(live.task_id.clone());
            row.status = "running".to_owned();
            row.result = None;
            row.error = None;
            task = Some(live);
        }
        let projected = task.as_ref().map(|task| match task.status {
            TaskStatus::Queued => "queued",
            TaskStatus::Running | TaskStatus::WaitingApproval | TaskStatus::Interrupted => {
                "running"
            }
            // AGENTKIT-88 statuses on a child turn: a child parked on its
            // own children is still in flight; an unresolved outcome is
            // not a completed child and never a silent success.
            TaskStatus::WaitingChild => "running",
            TaskStatus::Uncertain => "interrupted",
            TaskStatus::Completed => "completed",
            TaskStatus::Failed => "failed",
            TaskStatus::Cancelled => "cancelled",
        });
        let mut status = projected.unwrap_or(row.status.as_str()).to_owned();
        let mut result = if status == "completed" {
            self.final_answer(&row).await?
        } else {
            row.result.clone()
        };
        let mut error = match task.as_ref() {
            Some(task) => match task.status {
                TaskStatus::Completed => {
                    if result.is_some() {
                        None
                    } else {
                        Some("completed without an answer".to_owned())
                    }
                }
                TaskStatus::Failed => Some(
                    task.error
                        .clone()
                        .or_else(|| row.error.clone())
                        .unwrap_or_else(|| "task failed".into()),
                ),
                TaskStatus::Cancelled => Some(
                    task.error
                        .clone()
                        .or_else(|| row.error.clone())
                        .unwrap_or_else(|| "cancelled".into()),
                ),
                _ => row.error.clone(),
            },
            None => row.error.clone(),
        };
        // Reactivate (resume path): a failed/cancelled row whose tracked
        // turn was re-pointed at fresh live work by `resume` must rejoin
        // the active ledger — the freeze keeps `update_subagent` from
        // lifting the status, so the sanctioned transition runs here. A
        // cancel-requested turn is winding DOWN, not running: reviving on
        // it would resurrect a child the reaper or `cancel` just stopped.
        let winding_down = task
            .as_ref()
            .is_some_and(|task| task.cancel_requested || task.status.is_terminal());
        if !winding_down && is_terminal_status(&row.status) && !is_terminal_status(&status) {
            self.store
                .reactivate_subagent(&row.child_chat_id, &status, self.now_ms())
                .await?;
            row.status = status.clone();
            row.result = None;
            row.error = None;
            result = None;
            error = None;
        }
        // Lease sustain (F8): an exclusive child whose turn is verifiably
        // RUNNING re-admits its workspace lease on every observation
        // (status/list/await all funnel through here), so a long turn can
        // never have its root TTL-stolen while it is actively writing. If
        // a foreign holder already took the root, the child's writes are
        // refused by the guard anyway — cancel it honestly instead of
        // letting it grind. Deliberately NOT routed through `cancel()`
        // (which calls `refresh` — this must not recurse).
        if status == "running"
            && task
                .as_ref()
                .is_some_and(|task| task.status == TaskStatus::Running && !task.cancel_requested)
            && let Some(chat) = self.store.get_chat(&row.child_chat_id).await?
        {
            let exclusive = chat.metadata["agentkit.subagent"]["exclusive"]
                .as_bool()
                .unwrap_or(false);
            if exclusive {
                let root = read_session_descriptor(&chat.metadata).and_then(|descriptor| {
                    descriptor
                        .get("workspace")?
                        .get("root")?
                        .as_str()
                        .map(str::to_owned)
                });
                if let Some(root) = root
                    && let Err(LeaseHeld { holder }) = self.leases.try_acquire(
                        Some(&root),
                        true,
                        &row.child_chat_id,
                        self.now_ms(),
                        self.config().limits.lease_ttl_ms,
                    )
                {
                    if let Some(turn_id) = row.turn_task_id.as_deref()
                        && let Some(turn) = self.store.get_task(turn_id).await?
                        && !turn.status.is_terminal()
                    {
                        let _ = self.session.cancel(&row.child_chat_id, turn_id).await;
                    }
                    self.leases.release(&row.child_chat_id);
                    status = "cancelled".to_owned();
                    error = Some(format!(
                        "lease_lost: the workspace root is held by child {holder}"
                    ));
                }
            }
        }
        if status != row.status || result != row.result || error != row.error {
            self.store
                .update_subagent(
                    &row.child_chat_id,
                    &status,
                    result.as_deref(),
                    error.as_deref(),
                    self.now_ms(),
                )
                .await?;
        }
        if is_terminal_status(&status) {
            // Unconditional: after `cancel()` the row may already carry the
            // projected terminal state, and the lease must still go when
            // the turn verifiably settled — idempotent map removal.
            self.leases.release(&row.child_chat_id);
        }
        let waiting = task
            .as_ref()
            .is_some_and(|task| matches!(task.status, TaskStatus::WaitingApproval));
        let queued_inputs = if is_terminal_status(&status) {
            0
        } else {
            match self.session.get_state(&row.child_chat_id).await {
                Ok(state) => state.pending.len(),
                Err(_) => 0,
            }
        };
        let role_digest = self
            .store
            .get_chat(&row.child_chat_id)
            .await?
            .and_then(|chat| {
                chat.metadata["agentkit.subagent"]["roleDigest"]
                    .as_str()
                    .map(str::to_owned)
            });
        Ok(SubagentView {
            child_chat_id: row.child_chat_id.clone(),
            parent_chat_id: row.parent_chat_id.clone(),
            parent_task_id: row.parent_task_id.clone(),
            role: row.role.clone(),
            role_digest,
            depth: row.depth,
            prompt: row.prompt.clone(),
            model: row.model.clone(),
            status,
            result,
            error,
            created_at: row.created_at,
            updated_at: row.updated_at,
            turn_task_id: row.turn_task_id.clone(),
            task_status: task.map(|task| format!("{:?}", task.status)),
            waiting,
            queued_inputs,
        })
    }

    /// The child's final assistant answer for the tracked turn: the durable
    /// assistant message written under the turn's run, not a transcription.
    async fn final_answer(&self, row: &SubagentRecord) -> AgentsResult<Option<String>> {
        let Some(turn_id) = row.turn_task_id.as_deref() else {
            return Ok(row.result.clone());
        };
        let records = self.store.messages(&row.child_chat_id, true).await?;
        let answer = records
            .iter()
            .filter(|record| {
                record.active
                    && record.role == "assistant"
                    && record.run_id.as_deref() == Some(turn_id)
                    && record.metadata.get("placeholder") != Some(&Value::Bool(true))
            })
            .max_by_key(|record| record.order_key)
            .map(|record| match &record.content {
                Value::String(text) => text.clone(),
                other => other.to_string(),
            });
        Ok(answer.or_else(|| row.result.clone()))
    }

    // -- result envelope ----------------------------------------------------------

    async fn await_result(&self, view: &SubagentView) -> AgentsResult<AwaitResult> {
        Ok(AwaitResult {
            child_chat_id: view.child_chat_id.clone(),
            role: view.role.clone(),
            status: view.status.clone(),
            envelope: self.build_envelope(view).await?,
        })
    }

    async fn await_results(&self, views: &[SubagentView]) -> AgentsResult<Vec<AwaitResult>> {
        let mut results = Vec::with_capacity(views.len());
        for view in views {
            results.push(self.await_result(view).await?);
        }
        Ok(results)
    }

    /// The result envelope of a settled child (`None` while it still runs).
    /// The summary is the final message cut at 6 KiB on a paragraph boundary;
    /// a longer message is stored whole in the output store and named by
    /// `full_result_handle`. Findings and open questions come only from the
    /// child's own `report_findings` call; usage from its turns' events.
    pub async fn build_envelope(
        &self,
        view: &SubagentView,
    ) -> AgentsResult<Option<ResultEnvelope>> {
        if !is_terminal_status(&view.status) {
            return Ok(None);
        }
        let metadata = self
            .store
            .get_chat(&view.child_chat_id)
            .await?
            .map(|chat| chat.metadata[SUBAGENT_METADATA_KEY].clone())
            .unwrap_or_default();
        let report = ChildReport::from_metadata(&metadata);
        // A token-budget stop is read from what the gate recorded for this
        // very turn, never from error text a tool or model could forge.
        let stopped_by_tokens = metadata["budgetExhausted"]["taskId"].as_str()
            == view.turn_task_id.as_deref()
            && view.turn_task_id.is_some();
        let budget_exhausted = BudgetExhausted::from_metadata(&metadata).filter(|_| {
            stopped_by_tokens && !matches!(view.status.as_str(), "completed" | "cancelled")
        });
        let status = match envelope_status(&view.status, view.error.as_deref()) {
            "failed" if budget_exhausted.is_some() => "budget_exhausted",
            other => other,
        };
        // A child stopped by its token budget reports what it had said so
        // far; the exhaustion note is appended after the cut so a long
        // partial answer can never push it out of the summary.
        let partial = match &budget_exhausted {
            Some(spent) => Some(self.partial_answer(view, spent).await?),
            None => None,
        };
        let full = match &partial {
            Some((text, _)) => text.clone(),
            None => view
                .result
                .clone()
                .or_else(|| view.error.clone())
                .unwrap_or_default(),
        };
        let (mut summary, mut truncated) = cut_summary(&full);
        if let Some((_, note)) = &partial {
            // The note is part of the cap: cut the partial text to what
            // room it leaves, then append it.
            let room = SUMMARY_CAP_BYTES.saturating_sub(note.len() + 2);
            let (head, cut) = cut_summary_to(&full, room);
            truncated |= cut;
            summary = if head.is_empty() {
                note.clone()
            } else {
                format!("{head}\n\n{note}")
            };
        }
        let mut artifacts = vec![];
        let mut full_result_handle = None;
        if truncated {
            full_result_handle = self.store_full_result(view, &full).await;
            if let Some(handle) = &full_result_handle {
                artifacts.push(EnvelopeArtifact {
                    kind: "output".into(),
                    reference: handle.clone(),
                    description: "the full final message".into(),
                });
            }
        }
        Ok(Some(ResultEnvelope {
            child_id: view.child_chat_id.clone(),
            role: view.role.clone(),
            status: status.to_owned(),
            summary,
            findings: report.findings,
            artifacts,
            open_questions: report.open_questions,
            usage: self.child_usage(&view.child_chat_id).await?,
            truncated,
            full_result_handle,
            budget_exhausted,
        }))
    }

    /// `(the child's last assistant text so far, the exhaustion note)` of a
    /// child that spent a token budget. The text is empty when it had said
    /// nothing yet.
    async fn partial_answer(
        &self,
        view: &SubagentView,
        spent: &BudgetExhausted,
    ) -> AgentsResult<(String, String)> {
        let text = match self.store.get_subagent(&view.child_chat_id).await? {
            Some(row) => self.final_answer(&row).await?,
            None => None,
        };
        let note = format!(
            "[token budget exhausted: the {} budget spent {} of {} tokens; the child stopped before finishing]",
            spent.budget, spent.used, spent.limit
        );
        Ok((text.unwrap_or_default(), note))
    }

    /// Write the child's whole final message to the output store, once per
    /// distinct text: the handle is remembered on the child's chat, so
    /// repeated reads of the same result name the same handle. `None` when
    /// the store has no output root or the write failed; the envelope is
    /// then still marked truncated.
    async fn store_full_result(&self, view: &SubagentView, full: &str) -> Option<String> {
        let outputs = self.output_store().ok()?;
        let digest = sha256_hex(full);
        if let Some(chat) = self
            .store
            .get_chat(&view.child_chat_id)
            .await
            .ok()
            .flatten()
        {
            let recorded = &chat.metadata[SUBAGENT_METADATA_KEY]["resultHandle"];
            if recorded["sha256"].as_str() == Some(digest.as_str())
                && let Some(text) = recorded["handle"].as_str()
                && let Ok(handle) = OutputHandle::parse(text)
                && outputs.info(&handle).await.is_ok()
            {
                return Some(text.to_owned());
            }
        }
        let handle = outputs
            .put(
                OutputMeta {
                    session: view.child_chat_id.clone(),
                    task: view.turn_task_id.clone().unwrap_or_default(),
                    call_id: "subagent_result".into(),
                    tool: "subagent_result".into(),
                    mime: "text/plain".into(),
                    exit_code: None,
                },
                full.as_bytes().to_vec(),
            )
            .await
            .ok()?;
        let record = json!({"handle": handle.as_str(), "sha256": digest});
        let _ = self
            .store
            .mutate_chat(&view.child_chat_id, self.now_ms(), move |chat| {
                chat.metadata[SUBAGENT_METADATA_KEY]["resultHandle"] = record;
                Ok(())
            })
            .await;
        Some(handle.to_string())
    }

    /// Usage of every turn the child ran, from the normalized usage events
    /// of its turn tasks; duration is the sum of the turns' wall clock.
    async fn child_usage(&self, child_chat_id: &str) -> AgentsResult<EnvelopeUsage> {
        const PAGE: usize = 1_000;
        let tasks = self
            .store
            .list_tasks(Some(child_chat_id.to_owned()))
            .await?;
        let mut usage = EnvelopeUsage::default();
        for task in tasks.iter().filter(|task| task.kind == CHAT_TURN_TASK_KIND) {
            if let (Some(started), Some(finished)) = (task.started_at, task.finished_at) {
                usage.duration_ms = usage
                    .duration_ms
                    .saturating_add(u64::try_from(finished.saturating_sub(started)).unwrap_or(0));
            }
            let mut after = None;
            loop {
                let page = self.store.events(&task.task_id, after, PAGE).await?;
                let (tokens, requests) = usage_from_events(page.iter().map(|record| &record.event));
                usage.tokens = usage.tokens.saturating_add(tokens);
                usage.provider_requests = usage.provider_requests.saturating_add(requests);
                match page.last() {
                    Some(last) if page.len() == PAGE => after = Some(last.seq as u64),
                    _ => break,
                }
            }
        }
        Ok(usage)
    }

    /// A child view as the parent model reads it: the child's text is gone
    /// from `result` and `error` and rides in `envelope`, the untrusted
    /// block. Everything else is the registry's own bookkeeping.
    pub async fn present_view(&self, view: &SubagentView) -> AgentsResult<Value> {
        let mut value = serde_json::to_value(view).map_err(|error| {
            AgentsError::Core(agentkit_core::CoreError::new(
                "subagent_view_unserializable",
                &error.to_string(),
            ))
        })?;
        if let Some(object) = value.as_object_mut() {
            object.remove("result");
            object.remove("error");
            if let Some(envelope) = self.build_envelope(view).await? {
                object.insert(
                    "envelope".into(),
                    Value::String(present_envelope(&envelope)),
                );
            }
        }
        Ok(value)
    }

    // -- trust rules (DEL-04) -----------------------------------------------------

    /// Answer a human request on behalf of `actor_chat_id`. A child session
    /// can never answer or approve a request: not its parent's, not a
    /// sibling's, not its own. Requests belong to the owner, who answers
    /// through the session surface (see [`Self::pending_owner_requests`]).
    /// Every agent-originated answer path goes through this check.
    pub async fn answer_as(
        &self,
        actor_chat_id: &str,
        input: &AnswerInput,
    ) -> AgentsResult<AnswerReceipt> {
        if self.store.get_subagent(actor_chat_id).await?.is_some() {
            return Err(AgentsError::ChildCannotAnswer(format!(
                "child_cannot_answer: session {actor_chat_id} is a subagent and cannot answer \
                 request {}; the owner answers it",
                input.request_id
            )));
        }
        // Not even a non-child agent answers: the store refuses the
        // `Agent` actor for every request, so this stays closed whatever
        // the checks above know about the session.
        self.session
            .answer_with_actor(input, AnswerActor::Agent(actor_chat_id.to_owned()))
            .await
            .map_err(session_error)
    }

    /// Every human request waiting in the tree below `root_chat_id` (the
    /// owner's session included), each labelled with the title and role of
    /// the child that raised it. A child's approval request is not hidden in
    /// the child's session: it is listed here for the owner. Child waits are
    /// the host's, never a human's, and are left out.
    pub async fn pending_owner_requests(
        &self,
        root_chat_id: &str,
    ) -> AgentsResult<Vec<OwnerRequest>> {
        let mut sessions: Vec<(String, Option<String>)> = vec![(root_chat_id.to_owned(), None)];
        for row in budget::tree_rows(&self.store, root_chat_id).await? {
            sessions.push((row.child_chat_id, Some(row.role)));
        }
        let mut requests = vec![];
        for (session_id, role) in sessions {
            let title = self
                .store
                .get_chat(&session_id)
                .await?
                .and_then(|chat| chat.title)
                .unwrap_or_else(|| session_id.clone());
            let pending = self
                .store
                .list_inputs(AgentInputQuery {
                    session_id: session_id.clone(),
                    kinds: Some(vec![AgentInputKind::HumanRequest]),
                    statuses: Some(vec![AgentInputStatus::Pending]),
                    target_task_id: None,
                })
                .await?;
            for request in pending
                .into_iter()
                .filter(|request| !is_child_wait(&request.input_id, &request.content))
            {
                requests.push(OwnerRequest {
                    request_id: request.input_id,
                    session_id: session_id.clone(),
                    label: role.as_ref().map(|role| format!("{title} ({role})")),
                    role: role.clone(),
                    content: request.content,
                });
            }
        }
        Ok(requests)
    }

    async fn require_row(&self, child_chat_id: &str) -> AgentsResult<SubagentRecord> {
        self.store
            .get_subagent(child_chat_id)
            .await?
            .ok_or_else(|| AgentsError::SubagentNotFound(child_chat_id.to_owned()))
    }
}

const MISSING_STATUS: &str = "missing";

fn missing_result(child_chat_id: &str) -> AwaitResult {
    AwaitResult {
        child_chat_id: child_chat_id.to_owned(),
        role: String::new(),
        status: MISSING_STATUS.to_owned(),
        envelope: None,
    }
}

#[async_trait::async_trait]
impl agentkit_chat::DelegationGuidance for SubagentService {
    async fn guidance(&self, _chat_id: &str) -> Option<String> {
        let config = self.config();
        config
            .enabled
            .then_some(config.delegation_guidance)
            .flatten()
    }
}

#[async_trait::async_trait]
impl agentkit_engine::TaskSettledObserver for SubagentService {
    async fn observe_settled(
        &self,
        task: &agentkit_store::TaskRecord,
    ) -> Result<(), agentkit_engine::ObserverError> {
        Ok(self
            .observe_settled(task)
            .await
            .map_err(agentkit_engine::EngineError::from)?)
    }
}
