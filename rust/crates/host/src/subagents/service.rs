use super::budget;
use super::config::{RoleSpec, SubagentConfig};
use super::guard::WorkspaceLeaseRegistry;
use super::snapshot::{EffectiveRole, RoleSnapshot, effective_role};
use crate::{
    AgentSendResult, AgentSessionService, CreateSessionInput, FollowUpInput, HostError, HostResult,
    SendInput, SharedRuntime, SteerInput, WhenBusy, read_session_descriptor,
};
use agentkit_store::{RegisterSubagent, SqliteStore, SubagentRecord, TaskRecord, TaskStatus};
use serde::Serialize;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
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
    pub role: String,
    pub prompt: String,
    pub title: Option<String>,
    pub model: Option<String>,
    pub exclusive_workspace: bool,
    pub max_iterations: Option<u32>,
    pub max_provider_requests: Option<u64>,
    pub overall_timeout_ms: Option<u64>,
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
    pub status: String,
    pub result: Option<String>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AwaitOutcome {
    pub settled: bool,
    pub timed_out: bool,
    pub results: Vec<AwaitResult>,
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
    session: AgentSessionService,
    runtime: SharedRuntime,
    config: Arc<Mutex<SubagentConfig>>,
    leases: WorkspaceLeaseRegistry,
    /// Serializes the spawn check→insert critical section (see `spawn`).
    spawn_gate: Arc<tokio::sync::Mutex<()>>,
    /// Provider ids this service's executor can route to; `None` = unknown
    /// (host-only users), which skips spawn-time validation.
    routable_providers: Option<Arc<std::collections::BTreeSet<String>>>,
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

fn session_error(error: crate::SessionError) -> HostError {
    HostError::Execution(format!("{}: {}", error.code, error.message))
}

fn sha256_hex(text: &str) -> String {
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub(super) fn is_terminal_status(status: &str) -> bool {
    matches!(status, "completed" | "failed" | "cancelled")
}

/// Absolute ceilings on model-authored child bounds (`subagent_spawn`
/// arguments come from the model): a role ceiling the host set is never
/// lifted by a request, and no request may exceed these. Chosen far above
/// any legitimate single child turn while staying impossible to mine for
/// sustained provider drain.
const SUBAGENT_ITERATIONS_CEILING: u32 = 64;
pub(super) const SUBAGENT_PROVIDER_REQUESTS_CEILING: u64 = 200;
const SUBAGENT_OVERALL_MS_CEILING: u64 = 30 * 60 * 1_000;

/// Deterministic child identity for one logical spawn call. Public so
/// callers (tests, recovery tooling) can precompute the id a given
/// (parent, parent task, spawn key) triple will produce.
pub fn child_identity(parent_chat_id: &str, parent_task_id: &str, spawn_key: &str) -> String {
    format!(
        "sub_{}",
        &sha256_hex(&format!("{parent_chat_id}|{parent_task_id}|{spawn_key}"))[..24]
    )
}

fn compose_child_prompt(role: &RoleSpec, request: &SpawnRequest) -> Value {
    let mut text = String::new();
    if !role.prompt.is_empty() {
        text.push_str(role.prompt.trim());
        text.push_str("\n\n");
    }
    text.push_str("Assignment:\n");
    text.push_str(request.prompt.trim());
    Value::String(text)
}

impl SubagentService {
    pub fn new(
        store: SqliteStore,
        session: AgentSessionService,
        runtime: SharedRuntime,
        config: SubagentConfig,
    ) -> Self {
        Self {
            store,
            session,
            runtime,
            config: Arc::new(Mutex::new(config.effective())),
            leases: WorkspaceLeaseRegistry::default(),
            spawn_gate: Arc::new(tokio::sync::Mutex::new(())),
            routable_providers: None,
        }
    }

    /// Inject the routable provider ids once at bind time so a role pinned
    /// to an unroutable provider fails at spawn, not on the child's turn.
    pub fn with_routable_providers(mut self, ids: std::collections::BTreeSet<String>) -> Self {
        self.routable_providers = Some(Arc::new(ids));
        self
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
    ) -> HostResult<()> {
        let (Some(known), Some(provider_id)) = (&self.routable_providers, provider_id) else {
            return Ok(());
        };
        if !known.contains(provider_id) {
            return Err(HostError::Execution(format!(
                "subagent_provider_unavailable: {provider_id}"
            )));
        }
        if Some(provider_id) != parent_provider && request.model.is_none() && role.model.is_none() {
            return Err(HostError::Execution(format!(
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

    /// Exclusive-lease admission with a refusal instead of silent
    /// preemption when another child already holds the root unexpired.
    fn acquire_lease(
        &self,
        root: Option<&str>,
        exclusive: bool,
        child_chat_id: &str,
        ttl_ms: i64,
    ) -> HostResult<()> {
        self.leases
            .try_acquire(root, exclusive, child_chat_id, self.now_ms(), ttl_ms)
            .map_err(|holder| {
                HostError::Execution(format!(
                    "workspace_busy: {} is exclusively leased to child {holder}; await or cancel it before spawning another writer",
                    root.unwrap_or("<workspace>")
                ))
            })
    }

    pub fn leases(&self) -> &WorkspaceLeaseRegistry {
        &self.leases
    }

    pub fn store(&self) -> &SqliteStore {
        &self.store
    }

    pub fn session(&self) -> &AgentSessionService {
        &self.session
    }

    pub fn now_ms(&self) -> i64 {
        self.runtime.now_ms()
    }

    // -- spawn -----------------------------------------------------------------

    /// Create + dispatch one child session. Every limit is checked against
    /// DURABLE state, so a restarted process enforces the same fan-out
    /// bounds as the original one.
    pub async fn spawn(&self, request: &SpawnRequest) -> HostResult<SpawnedChild> {
        let config = self.config();
        if !config.enabled {
            return Err(HostError::Execution("subagents_disabled".into()));
        }
        if request.prompt.trim().is_empty() {
            return Err(HostError::Execution(
                "subagent prompt must not be empty".into(),
            ));
        }
        let role = config.role(&request.role).cloned().ok_or_else(|| {
            HostError::Execution(format!("unknown_subagent_role: {}", request.role))
        })?;
        // A host-pinned role model is a ceiling the model-authored request
        // cannot lift: the role may run on exactly that model.
        if let (Some(pinned), Some(requested)) = (role.model.as_deref(), request.model.as_deref())
            && pinned != requested
        {
            return Err(HostError::Execution(format!(
                "model_not_allowed: role {} is pinned to model {pinned}; requested {requested}",
                role.id
            )));
        }
        let parent_task = self
            .store
            .get_task(&request.parent_task_id)
            .await?
            .filter(|task| task.scope_id == request.parent_chat_id)
            .ok_or_else(|| {
                HostError::Execution("spawn requires the caller's own live turn task".into())
            })?;
        if parent_task.status.is_terminal() {
            return Err(HostError::Execution("parent turn already settled".into()));
        }
        let limits = &config.limits;
        let parent_row = self.store.get_subagent(&request.parent_chat_id).await?;
        let depth = parent_row.as_ref().map_or(0, |row| row.depth);
        if depth + 1 > limits.max_depth {
            return Err(HostError::Execution(format!(
                "subagent_depth_exceeded: depth {depth} cannot spawn at max {}",
                limits.max_depth
            )));
        }
        if let Some(row) = parent_row.as_ref() {
            // Only roles allowed to nest may themselves delegate; the
            // parent's spawn-time snapshot caps what a later config edit
            // could grant.
            let parent_role = match self.effective_role_of(row, &config).await? {
                Ok(effective) => effective,
                Err(reason) => return Err(HostError::Execution(reason)),
            };
            if !parent_role.can_spawn {
                return Err(HostError::Execution(format!(
                    "role {} may not spawn children",
                    parent_role.id
                )));
            }
        }
        let parent_chat = self
            .store
            .get_chat(&request.parent_chat_id)
            .await?
            .ok_or_else(|| HostError::Execution("parent session not found".into()))?;
        let descriptor = read_session_descriptor(&parent_chat.metadata).ok_or_else(|| {
            HostError::Execution(
                "parent session carries no descriptor; only sessions delegate".into(),
            )
        })?;
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
            .model
            .clone()
            .or_else(|| role.model.clone())
            .or_else(|| parent_model.map(str::to_owned));
        self.validate_child_provider(&role, request, provider_id.as_deref(), parent_provider)?;
        if !role.read_only && workspace.is_none() {
            return Err(HostError::Execution(format!(
                "workspace_required: role {} writes, but the parent session has no workspace",
                role.id
            )));
        }
        // Execution-mode inheritance: read-only roles are pinned to
        // `read_only`; writing roles inherit the parent's durable mode so a
        // child never exceeds what the human granted the parent.
        let execution_mode = if role.read_only {
            Some("read_only".to_owned())
        } else {
            self.session
                .get_execution(&request.parent_chat_id)
                .await
                .map_err(session_error)?
                .and_then(|state| state.get("mode").and_then(Value::as_str).map(str::to_owned))
        };

        let child_chat_id = child_identity(
            &request.parent_chat_id,
            &request.parent_task_id,
            &request.spawn_key,
        );
        // Durable per-child exclusivity: recorded at spawn so `resume`
        // re-admits the lease instead of letting a restarted writer run
        // unleased beside a fresh exclusive sibling.
        let exclusive = request.exclusive_workspace && !role.read_only;

        // Replay path FIRST — before the caps: re-admitting a spawn the
        // store already holds is not a new admission, so a replay must not
        // error behind a saturated roster (the retry could never deliver
        // its result). The same logical spawn call (retry of a tool call)
        // re-admits the existing child; nothing is created twice.
        if let Some(existing) = self.store.get_subagent(&child_chat_id).await? {
            if existing.parent_chat_id != request.parent_chat_id
                || existing.parent_task_id.as_deref() != Some(request.parent_task_id.as_str())
                || existing.role != request.role
            {
                return Err(HostError::Execution(format!(
                    "spawn key {} already exists with a different identity",
                    request.spawn_key
                )));
            }
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
                    return Err(HostError::Execution(
                        "parent turn already settled; queued child cancelled".into(),
                    ));
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
                self.dispatch_initial_turn(&child_chat_id, &role, request)
                    .await?;
                let Some(reactualized) = self.store.get_subagent(&child_chat_id).await? else {
                    return Err(HostError::Execution("subagent row vanished".into()));
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

        // Per-role execution limits ride the child chat metadata; the
        // executor reads them at dispatch (`subagents::task_overrides`), so
        // role budgets are durable task properties, not this process's
        // memory. The MODEL may steer a child's bounds, never widen them:
        // a host-set role bound is a ceiling the request cannot lift, and
        // the absolute caps keep a prompt-injected parent from minting
        // run-away children (`maxIterations` etc. are model-authored ints).
        let mut role_limits = Map::new();
        let iterations = match (request.max_iterations, role.max_iterations) {
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
        if let Some(overall) = request.overall_timeout_ms {
            role_limits.insert(
                "overallTimeoutMs".into(),
                json!(overall.clamp(1_000, SUBAGENT_OVERALL_MS_CEILING)),
            );
        }
        let role_snapshot = RoleSnapshot::from_role(&role);
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
                "exclusive": exclusive,
                "limits": Value::Object(role_limits),
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
                return Err(HostError::Execution(format!(
                    "child session id {child_chat_id} is already in use by a different spawn"
                )));
            }
        } else {
            self.session
                .create_session(&CreateSessionInput {
                    session_id: Some(child_chat_id.clone()),
                    title: Some(request.title.clone().unwrap_or_else(|| {
                        let head: String = request.prompt.chars().take(72).collect();
                        format!("[{}] {head}", role.id)
                    })),
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
                        .map_err(|_| HostError::Execution("invalid_reasoning_effort".into()))?,
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
                    prompt: request.prompt.clone(),
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
            return Err(HostError::Execution(
                "subagent_concurrency_limit: fan-out cap reached during registration".into(),
            ));
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
            return Err(HostError::Execution(
                "parent turn already settled; child cancelled".into(),
            ));
        }

        self.dispatch_initial_turn(&child_chat_id, &role, request)
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
            .ok_or_else(|| HostError::Execution("subagent row vanished".into()))?;
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
    ) -> HostResult<()> {
        let content = compose_child_prompt(role, request);
        let input_id = format!(
            "subin:{}",
            &sha256_hex(&format!("{child_chat_id}:turn"))[..24]
        );
        let send = match self
            .session
            .send(
                child_chat_id,
                &SendInput {
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
            Err(err) if err.code == "input_conflict" => None,
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

    // -- queries ----------------------------------------------------------------

    pub async fn list(&self, parent_chat_id: &str) -> HostResult<Vec<SubagentView>> {
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
    ) -> HostResult<Result<EffectiveRole, String>> {
        let chat = self.store.get_chat(&row.child_chat_id).await?;
        let snapshot = match chat
            .as_ref()
            .and_then(|chat| chat.metadata.get("agentkit.subagent"))
            .map(RoleSnapshot::from_metadata)
            .transpose()
        {
            Ok(snapshot) => snapshot.flatten(),
            Err(reason) => return Ok(Err(reason)),
        };
        Ok(effective_role(snapshot.as_ref(), config.role(&row.role))
            .ok_or_else(|| format!("subagent role {} is no longer configured", row.role)))
    }

    pub async fn status(&self, child_chat_id: &str) -> HostResult<SubagentView> {
        let row = self
            .store
            .get_subagent(child_chat_id)
            .await?
            .ok_or_else(|| HostError::NotFound(format!("subagent_not_found: {child_chat_id}")))?;
        self.refresh(&row).await
    }

    pub async fn message(
        &self,
        child_chat_id: &str,
        content: Value,
        mode: &str,
        message_key: &str,
    ) -> HostResult<MessageReceipt> {
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
            return Err(HostError::Execution(format!(
                "subagent_terminal: {child_chat_id} is {}; message is for active children \
                 only — resume restarts a failed or cancelled child, otherwise spawn a new one",
                view.status
            )));
        }
        let input_id = format!(
            "submsg:{}",
            &sha256_hex(&format!("{child_chat_id}:{message_key}"))[..24]
        );
        let result = match mode {
            "steer" => {
                self.session
                    .steer(
                        child_chat_id,
                        &SteerInput {
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
    ) -> HostResult<AwaitOutcome> {
        let config = self.config();
        let poll = config.limits.await_poll_ms;
        let timeout = timeout_ms
            .unwrap_or(config.limits.max_await_ms)
            .min(config.limits.max_await_ms);
        let deadline = self.now_ms() + timeout as i64;
        let mut rows: Vec<SubagentRecord> = match ids {
            Some(ids) if !ids.is_empty() => {
                let mut rows = Vec::with_capacity(ids.len());
                for id in ids {
                    rows.push(self.require_row(id).await?);
                }
                if rows.iter().any(|row| row.parent_chat_id != parent_chat_id) {
                    return Err(HostError::Execution(
                        "a parent may only await its own children".into(),
                    ));
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
        };
        let mut views: Vec<SubagentView> = Vec::with_capacity(rows.len());
        loop {
            if cancellation.is_cancelled() {
                return Err(HostError::Cancelled);
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
                        HostError::Execution(format!(
                            "subagent row vanished: {}",
                            row.child_chat_id
                        ))
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
            if views.iter().all(|view| is_terminal_status(&view.status)) {
                return Ok(AwaitOutcome {
                    settled: true,
                    timed_out: false,
                    results: views.iter().map(await_result).collect(),
                });
            }
            if self.now_ms() >= deadline {
                return Ok(AwaitOutcome {
                    settled: false,
                    timed_out: true,
                    results: views.iter().map(await_result).collect(),
                });
            }
            // Bound each nap so a parent cancellation is felt promptly.
            tokio::select! {
                _ = cancellation.cancelled() => return Err(HostError::Cancelled),
                _ = tokio::time::sleep(std::time::Duration::from_millis(poll)) => {}
            }
        }
    }

    /// Cooperatively cancel the child's live turn AND its queued inputs.
    /// The store's durable cancel flag — not this call's success — is
    /// authoritative; the worker settles the task and `refresh` projects it
    /// onto the row.
    pub async fn cancel(&self, child_chat_id: &str) -> HostResult<SubagentView> {
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
    pub async fn resume(&self, child_chat_id: &str, resume_key: &str) -> HostResult<SubagentView> {
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
    ) -> HostResult<SubagentView> {
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
                    &sha256_hex(&format!("{child_chat_id}:{resume_key}"))[..24]
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
                return Err(HostError::Execution(format!(
                    "subagent turn is {status:?}; only interrupted, failed or cancelled turns resume"
                )));
            }
            None => {
                return Err(HostError::Execution(
                    "subagent has no turn to resume".into(),
                ));
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
    ) -> HostResult<()> {
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
    async fn check_fanout_caps(&self, parent_chat_id: &str) -> HostResult<()> {
        let config = self.config();
        let limits = &config.limits;
        if self.store.count_active_subagents().await? >= limits.max_concurrency {
            return Err(HostError::Execution(format!(
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
            return Err(HostError::Execution(format!(
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
    ) -> HostResult<()> {
        let max_depth = self.config().limits.max_depth;
        if view.depth > max_depth {
            return Err(HostError::Execution(format!(
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
                return Err(HostError::Execution(
                    "resume requires the caller's own live turn task".into(),
                ));
            }
        }
        self.check_fanout_caps(&view.parent_chat_id).await
    }

    /// A resumed writer re-admits its exclusive lease before its turn runs
    /// again: the lease lapsed when the previous turn settled, and an
    /// unleased resume could mutate beside a fresh exclusive sibling. The
    /// returned guard releases a lease THIS call took if the resume then
    /// fails; `keep` disarms it on success.
    async fn admit_resume_lease(&self, child_chat_id: &str) -> HostResult<LeaseRelease> {
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
    pub async fn restore_leases(&self) -> HostResult<usize> {
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
                        TaskStatus::Running | TaskStatus::Queued | TaskStatus::WaitingApproval
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
    pub async fn reap_orphans(&self) -> HostResult<usize> {
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
    pub async fn observe_settled(&self, task: &TaskRecord) -> HostResult<()> {
        if !task.status.is_terminal() {
            return Ok(());
        }
        // A child's own turn just settled: re-project now rather than at
        // the parent's next observation — this releases the lease promptly
        // and follows (rebind + reactivate) a queued input the runner's
        // settle plan promoted into a fresh turn.
        let config = self.config();
        if task.kind == "chat.turn"
            && let Some(row) = self.store.get_subagent(&task.scope_id).await?
        {
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

    async fn cancel_row(&self, row: &SubagentRecord) -> HostResult<()> {
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
    async fn live_child_turn(&self, child_chat_id: &str) -> HostResult<Option<TaskRecord>> {
        let tasks = self
            .store
            .list_tasks(Some(child_chat_id.to_owned()))
            .await?;
        Ok(tasks
            .into_iter()
            .filter(|task| {
                task.kind == "chat.turn" && !task.status.is_terminal() && !task.cancel_requested
            })
            .max_by_key(|task| task.enqueued_at))
    }

    /// Join the registry row with the authoritative task/session state,
    /// persisting the projection when it moved. The task store holds the
    /// truth; the row is a cache with durable memory.
    pub async fn refresh(&self, row: &SubagentRecord) -> HostResult<SubagentView> {
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
                    && let Err(holder) = self.leases.try_acquire(
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
    async fn final_answer(&self, row: &SubagentRecord) -> HostResult<Option<String>> {
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

    async fn require_row(&self, child_chat_id: &str) -> HostResult<SubagentRecord> {
        self.store
            .get_subagent(child_chat_id)
            .await?
            .ok_or_else(|| HostError::NotFound(format!("subagent_not_found: {child_chat_id}")))
    }
}

fn await_result(view: &SubagentView) -> AwaitResult {
    AwaitResult {
        child_chat_id: view.child_chat_id.clone(),
        status: view.status.clone(),
        result: view.result.clone(),
        error: view.error.clone(),
    }
}

#[async_trait::async_trait]
impl crate::DelegationGuidance for SubagentService {
    async fn guidance(&self, _chat_id: &str) -> Option<String> {
        let config = self.config();
        config
            .enabled
            .then_some(config.delegation_guidance)
            .flatten()
    }
}

#[async_trait::async_trait]
impl crate::TaskSettledObserver for SubagentService {
    async fn observe_settled(&self, task: &agentkit_store::TaskRecord) -> HostResult<()> {
        self.observe_settled(task).await
    }
}

/// Per-role execution limits read from the child chat's durable metadata,
/// consumed by the executor when the turn dispatches. Returns defaults for
/// ordinary (non-subagent) sessions, so the executor path pays only one
/// cheap metadata read it already performs.
pub async fn task_overrides(
    store: &SqliteStore,
    chat_id: &str,
) -> HostResult<crate::budget::RoleTaskOverrides> {
    let record = match store.get_chat(chat_id).await? {
        Some(chat) => chat,
        None => return Ok(crate::budget::RoleTaskOverrides::default()),
    };
    let limits = &record.metadata["agentkit.subagent"]["limits"];
    Ok(crate::budget::RoleTaskOverrides {
        max_tool_iterations: limits["maxToolIterations"]
            .as_u64()
            .map(|value| value.max(1) as u32),
        max_provider_requests: limits["maxProviderRequests"]
            .as_u64()
            .map(|value| value.max(1)),
        overall_timeout_ms: limits["overallTimeoutMs"]
            .as_u64()
            .filter(|value| *value >= 1000),
    })
}
