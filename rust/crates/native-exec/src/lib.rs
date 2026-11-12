//! Generic durable native-turn executor (AGENTKIT-113, with the 115/116
//! host obligations).
//!
//! One native-turn attempt owns exactly one vendor process through the
//! ports (`NativeBackend`, `ProcessSupervisor`, `ExecutableLocator`) and the
//! durable store primitives (`native_exec.rs`). The host never inspects
//! vendor wire formats: the adapter is the only terminal-outcome authority
//! (AGENTKIT-107 §7). Invariants enforced here:
//!
//! * dispatch guard: model-loop targets are refused (`wrong_dispatch`);
//! * capability policy runs before any probe or spawn;
//! * exact resume requires an observed lineage (`lineage_missing`), and a
//!   turn whose vendor identity contradicts the frozen binding fails closed
//!   (`session_identity_mismatch`) — never silent adoption;
//! * EOF/exit without a root result can only end `Uncertain`/`Failed` — the
//!   adapter guarantees it; this executor never converts silence to success;
//! * every terminal path revokes the attempt broker and invalidates pending
//!   durable approvals, bounded by `cleanup_timeout_ms`;
//! * cancellation (host shutdown) and durable interruption (user stop)
//!   interrupt the owned child with distinct policies and are reaped to
//!   completion before settling.

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use agentkit_native::{
    AttemptBrokerHandle, BindNativeSession, BrokerLimits, BrokerToolDefinition, CommitContinuation,
    ExecutableLocator, ExecutionTarget, FailureClass, FinishDisposition, FrameLine,
    InterruptPolicy, NativeBackend, NativeContinuationBinding, NativeError, NativeEvent,
    NativeExecutionTarget, NativeProfile, NativeRunRepository, NativeTerminalOutcome,
    PreflightReport, ProcessExit, ProcessSupervisor, ResourceLimits, TransportFault, TurnInput,
    TurnResume, validate_execution_target,
};

use agentkit_engine::{EngineError, ExecutionFailure, RetrySafety, TaskExecutor, TaskRepository};

// The executor is generic over the repository (phase N1): every durable
// fact comes through the engine's task port and the native lane's run port,
// both closed over the same error type.
type HostError<R> = agentkit_engine::EngineError<<R as TaskRepository>::Error>;
type HostResult<R, T> = Result<T, HostError<R>>;
type ExecutionContext<R> = agentkit_engine::ExecutionContext<R>;

pub use agentkit_native::{
    BROKER_PAYLOAD_KEY, BrokerLimitsSpec, BrokerSpec, BrokerToolSpec,
    CONTINUATION_SCOPE_PAYLOAD_KEY, ContinuationScope, EXECUTION_TARGET_PAYLOAD_KEY,
    TURN_INPUT_PAYLOAD_KEY,
};

/// Everything a factory needs to build the per-attempt broker authority.
pub struct AttemptBrokerContext<'a> {
    pub task_id: &'a str,
    pub attempt_id: &'a str,
    pub server_name: &'a str,
    pub target: &'a NativeExecutionTarget,
    pub catalog: Vec<BrokerToolDefinition>,
    pub limits: BrokerLimits,
    pub cancellation: CancellationToken,
    /// Durable execution authority of the attempt the broker dispatches
    /// under (AGENTKIT-115). The consumer MUST hand this to every
    /// state-mutating tool execution (`ToolContext::dispatch_guard`) and
    /// check it immediately before dispatch: an in-memory cancellation
    /// token is a convenience, never authority — a cancelled or
    /// lease-superseded attempt must be refused from the durable task
    /// row, not from process-local state.
    pub dispatch_guard: Option<Arc<dyn agentkit_core::DispatchGuard>>,
}

/// Consumer-supplied authority source: the factory owns the
/// `BrokerAuthority` implementation (approvals, effects, argument
/// validation — AGENTKIT-115) and the attempt's liveness view.
#[async_trait]
pub trait AttemptBrokerFactory: Send + Sync {
    async fn start(
        &self,
        ctx: &AttemptBrokerContext<'_>,
    ) -> Result<Arc<dyn AttemptBrokerHandle>, agentkit_engine::ExecutionFailure>;
}

/// Tunables for the executor; defaults are the qualified bounds from
/// AGENTKIT-107 §8. `kinds` records the task kinds the consumer registers
/// this executor under (registry keys decide dispatch).
#[derive(Clone, Debug)]
pub struct NativeExecutorOptions {
    pub kinds: Vec<String>,
    pub limits: ResourceLimits,
    pub broker_limits: BrokerLimits,
}

impl Default for NativeExecutorOptions {
    fn default() -> Self {
        Self {
            kinds: vec!["native.turn".to_string()],
            limits: ResourceLimits::default(),
            broker_limits: BrokerLimits::default(),
        }
    }
}

/// The ledger claim an exact-resume attempt holds on its INPUT-position
/// binding (review round-4 F3). Taken atomically (`active → owned`) under
/// the current lease BEFORE the vendor can advance the session, it is a
/// resource of the attempt like the broker or the child: every exit path
/// must discharge it — provable non-acceptance of the input releases it
/// (back to Available), an accepted input with a non-completed terminal
/// outcome marks it `uncertain` (exact resume from the pre-claim position
/// stays refused), and a completed turn settles it through the commit
/// sweep. The owner's Drop discharges an undispositioned claim so an
/// aborted future can never strand the position in `owned` forever.
struct ContinuationClaim<R> {
    store: R,
    attempt_id: String,
    binding_id: String,
    input_accepted: std::sync::atomic::AtomicBool,
    settled: std::sync::atomic::AtomicBool,
}

impl<R> ContinuationClaim<R>
where
    R: TaskRepository + NativeRunRepository<Error = <R as TaskRepository>::Error>,
{
    fn new(store: &R, attempt_id: &str, binding_id: &str) -> Self {
        Self {
            store: store.clone(),
            attempt_id: attempt_id.to_owned(),
            binding_id: binding_id.to_owned(),
            input_accepted: std::sync::atomic::AtomicBool::new(false),
            settled: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Input delivery is about to start: from here on the
    /// vendor may have consumed the prompt, so only `uncertain` is an
    /// honest non-completion disposition.
    fn mark_input_accepted(&self) {
        self.input_accepted
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// Whether input may have reached the transport — the honest selector for
    /// `discharge` on terminal paths.
    fn accepted(&self) -> bool {
        self.input_accepted
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Conclusive completion: the commit (or its visible error) already
    /// disposed of the position — Drop owes nothing.
    fn settle(&self) {
        self.settled
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// Discharge the claim on a terminal path. `input_accepted` selects
    /// the honest transition; a store failure leaves the row `owned`,
    /// which fails CLOSED (exact resume stays refused), never open.
    async fn discharge(&self, input_accepted: bool) {
        if self.settled.swap(true, std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        let _ = if input_accepted {
            self.store
                .mark_continuation_uncertain(&self.binding_id, &self.attempt_id)
                .await
        } else {
            self.store
                .release_continuation_claim(&self.binding_id, &self.attempt_id)
                .await
        };
    }
}

/// The attempt resource owner. Every execution obligation that must
/// survive the REQUEST FUTURE being dropped — aborted futures, `?`
/// returns mid-stream, panics at persistence boundaries — is held here
/// with strong references and discharged on drop: broker authority is
/// revoked (immediately, synchronously) and settled, pending durable
/// interactions are invalidated, the private per-run directory is
/// removed. Explicit settlement marks the owner settled so the ordinary
/// path stays deterministic; the Drop path is the bounded safety net,
/// which is what makes future cancellation safe (review §2.5: "adding
/// more explicit cleanup calls will not cover future abortion").
pub struct AttemptOwner<R>
where
    R: TaskRepository + NativeRunRepository<Error = <R as TaskRepository>::Error>,
{
    store: R,
    attempt_id: String,
    supervisor: Arc<dyn ProcessSupervisor>,
    /// Identity of the process THIS attempt spawned (pid + generation),
    /// attached right after spawn. Drop terminates exactly this process —
    /// never the supervisor's other live children, which belong to other
    /// concurrent attempts sharing the runtime supervisor (AGENTKIT-110).
    child: std::sync::Mutex<Option<(u32, u64)>>,
    broker: Option<Arc<dyn AttemptBrokerHandle>>,
    per_run_dir: Option<std::path::PathBuf>,
    /// The exact-resume ledger claim, attached once taken (F3). Its
    /// discharge belongs to this owner: an aborted future must not strand
    /// the input position in `owned`.
    continuation: std::sync::Mutex<Option<Arc<ContinuationClaim<R>>>>,
    settled: std::sync::atomic::AtomicBool,
}

impl<R> AttemptOwner<R>
where
    R: TaskRepository + NativeRunRepository<Error = <R as TaskRepository>::Error>,
{
    fn new(
        store: R,
        attempt_id: &str,
        supervisor: Arc<dyn ProcessSupervisor>,
        broker: Option<Arc<dyn AttemptBrokerHandle>>,
        per_run_dir: Option<std::path::PathBuf>,
    ) -> Self {
        Self {
            store,
            attempt_id: attempt_id.to_owned(),
            supervisor,
            child: std::sync::Mutex::new(None),
            broker,
            per_run_dir,
            continuation: std::sync::Mutex::new(None),
            settled: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Record the spawned process so an aborted attempt cleans up exactly
    /// its own process generation.
    fn attach_child(&self, pid: u32, generation: u64) {
        *self.child.lock().unwrap() = Some((pid, generation));
    }

    fn set_per_run_dir(&mut self, dir: std::path::PathBuf) {
        self.per_run_dir = Some(dir);
    }

    /// Attach the ledger claim taken for this attempt (F3): from this
    /// moment the owner owes its discharge on every exit path, including
    /// future abortion.
    fn set_continuation(&self, claim: Option<Arc<ContinuationClaim<R>>>) {
        *self.continuation.lock().unwrap() = claim;
    }

    fn mark_settled(&self) {
        self.settled
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

impl<R> Drop for AttemptOwner<R>
where
    R: TaskRepository + NativeRunRepository<Error = <R as TaskRepository>::Error>,
{
    fn drop(&mut self) {
        if self.settled.load(std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        // Immediate synchronous revocation: tokens stop authenticating
        // the instant this frame unwinds, whatever happens next.
        if let Some(broker) = &self.broker {
            broker.revoke();
        }
        let store = self.store.clone();
        let attempt_id = self.attempt_id.clone();
        let supervisor = self.supervisor.clone();
        let child = self.child.lock().unwrap().take();
        let broker = self.broker.take();
        let per_run_dir = self.per_run_dir.take();
        let continuation = self.continuation.lock().unwrap().take();
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                // Undispositioned ledger claim (aborted future, `?`
                // return): discharge honestly — release only while the
                // input provably never reached the transport, uncertain
                // once it may have (review round-4 F3-A).
                if let Some(claim) = &continuation {
                    claim.discharge(claim.accepted()).await;
                }
                // Scoped to THIS attempt's process generation: the child
                // is reaped even though the request future (and its
                // handle) is gone — no `?`, no await, no abort can strand
                // a vendor process, and no sibling attempt is touched
                // (review §2.5, AGENTKIT-110). Whole-runtime shutdown owns
                // the global drain, not the per-attempt destructor.
                if let Some((pid, generation)) = child {
                    supervisor
                        .terminate_owned(pid, generation, InterruptPolicy::Quarantine)
                        .await;
                }
                if let Some(broker) = broker {
                    broker.shutdown().await;
                }
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |elapsed| elapsed.as_millis() as i64);
                let _ = store.invalidate_interactions(&attempt_id, now).await;
                if let Some(dir) = per_run_dir {
                    let _ = std::fs::remove_dir_all(dir);
                }
            });
        } else {
            // No runtime (sync drop outside async context): best effort,
            // still immediate for the authority bits.
            if let Some(broker) = broker {
                broker.revoke();
            }
            if let Some(dir) = per_run_dir {
                let _ = std::fs::remove_dir_all(dir);
            }
        }
    }
}

/// The generic durable executor. Stateless across attempts; all per-attempt
/// state lives in the store, the child handle and the adapter.
pub struct NativeAgentExecutor<R> {
    store: R,
    backend: Arc<dyn NativeBackend>,
    supervisor: Arc<dyn ProcessSupervisor>,
    locator: Arc<dyn ExecutableLocator>,
    broker_factory: Option<Arc<dyn AttemptBrokerFactory>>,
    options: NativeExecutorOptions,
}

impl<R> NativeAgentExecutor<R>
where
    R: TaskRepository + NativeRunRepository<Error = <R as TaskRepository>::Error>,
{
    pub fn new(
        store: R,
        backend: Arc<dyn NativeBackend>,
        supervisor: Arc<dyn ProcessSupervisor>,
        locator: Arc<dyn ExecutableLocator>,
        broker_factory: Option<Arc<dyn AttemptBrokerFactory>>,
        options: NativeExecutorOptions,
    ) -> Self {
        Self {
            store,
            backend,
            supervisor,
            locator,
            broker_factory,
            options,
        }
    }

    /// The kinds the consumer registers this executor under.
    pub fn kinds(&self) -> &[String] {
        &self.options.kinds
    }

    fn terminal(&self, code: &str, message: impl Into<String>) -> HostError<R> {
        HostError::<R>::Failure(ExecutionFailure {
            code: Some(code.to_string()),
            retryable: Some(false),
            retry_safety: RetrySafety::UnknownOutcome,
            ..ExecutionFailure::new(message)
        })
    }

    fn transient(&self, code: &str, message: impl Into<String>) -> HostError<R> {
        HostError::<R>::Failure(ExecutionFailure {
            code: Some(code.to_string()),
            retryable: Some(true),
            retry_safety: RetrySafety::SafeToRetry,
            ..ExecutionFailure::new(message)
        })
    }

    /// Bind (or idempotently re-bind) the attempt's frozen vendor session,
    /// then promote it to `observed` — the only identity later exact
    /// resumes may use.
    async fn bind_and_observe(
        &self,
        context: &ExecutionContext<R>,
        backend: &str,
        vendor_session_id: &str,
        parent: Option<String>,
    ) -> HostResult<R, ()> {
        self.store
            .bind_native_session(BindNativeSession {
                attempt_id: context.attempt_id.clone(),
                task_id: context.task.task_id.clone(),
                backend: backend.to_string(),
                vendor_session_id: vendor_session_id.to_string(),
                parent_vendor_session_id: parent,
                assigned_at: context.runtime.now_ms(),
            })
            .await
            .map_err(EngineError::Store)?;
        self.store
            .observe_native_session(
                &context.attempt_id,
                vendor_session_id,
                context.runtime.now_ms(),
            )
            .await
            .map_err(EngineError::Store)?;
        Ok(())
    }

    /// Settlement obligations every terminal path runs: pending durable
    /// approvals die with the attempt, the broker stops authenticating
    /// immediately (AGENTKIT-115/116), and the attempt's private per-run
    /// directory — staged config, frozen prompt, revoked broker token file
    /// — is removed once the child is gone (the vendor CLI may still read
    /// those files while it lives, so removal belongs at settlement).
    async fn settle_cleanup(
        &self,
        context: &ExecutionContext<R>,
        broker: Option<&Arc<dyn AttemptBrokerHandle>>,
        per_run_dir: Option<&std::path::Path>,
    ) {
        let _ = self
            .store
            .invalidate_interactions(&context.attempt_id, context.runtime.now_ms())
            .await;
        if let Some(broker) = broker {
            broker.shutdown().await;
        }
        if let Some(dir) = per_run_dir {
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    async fn emit(
        &self,
        context: &ExecutionContext<R>,
        kind: &str,
        payload: Value,
    ) -> HostResult<R, ()> {
        context.emit(kind, payload).await
    }

    async fn emit_session(
        &self,
        context: &ExecutionContext<R>,
        kind: &str,
        session_id: &str,
        fingerprint: &str,
    ) -> HostResult<R, ()> {
        context
            .emit(
                kind,
                json!({"sessionId": session_id, "fingerprint": fingerprint}),
            )
            .await
    }
}

#[async_trait]
impl<R> TaskExecutor<R> for NativeAgentExecutor<R>
where
    R: TaskRepository + NativeRunRepository<Error = <R as TaskRepository>::Error>,
{
    async fn execute(&self, context: ExecutionContext<R>) -> HostResult<R, ()> {
        // 1. Typed dispatch: only native targets run here.
        let target = match agentkit_native::parse_target(&context.task.payload) {
            Ok(ExecutionTarget::Native(target)) => target,
            Ok(ExecutionTarget::ModelLoop(_)) => {
                return Err(self.terminal(
                    "wrong_dispatch",
                    "model-loop target dispatched to the native executor",
                ));
            }
            Err(reason) => return Err(self.terminal("execution_target_invalid", reason)),
        };

        // 2. Capability policy before any probe or spawn (AGENTKIT-112).
        if let Err(violations) = validate_execution_target(&ExecutionTarget::Native(target.clone()))
        {
            let structural = violations.iter().any(|v| v.code != "capability_unknown");
            let detail = violations
                .iter()
                .map(|v| format!("{}: {}", v.code, v.detail))
                .collect::<Vec<_>>()
                .join("; ");
            return Err(if structural {
                self.terminal(
                    "capability_violation",
                    format!("capability policy refused the target: {detail}"),
                )
            } else {
                // Unknown evidence may become known after a re-probe of the
                // same frozen target; transient, never blind-looping.
                self.transient(
                    "capability_violation",
                    format!("capability evidence incomplete: {detail}"),
                )
            });
        }

        // 3. Frozen input.
        let input: TurnInput = serde_json::from_value(
            context
                .task
                .payload
                .get(TURN_INPUT_PAYLOAD_KEY)
                .cloned()
                .ok_or_else(|| {
                    self.terminal(
                        "turn_input_missing",
                        format!("payload is missing '{TURN_INPUT_PAYLOAD_KEY}'"),
                    )
                })?,
        )
        .map_err(|e| self.terminal("turn_input_invalid", e.to_string()))?;

        // 3b. Frozen continuation scope (consumer-defined, opaque to the
        // host). Present ⇒ exact resume is scoped-validated against the
        // durable continuation ledger and completed turns commit a
        // binding. Absent ⇒ legacy generic path (host-global observed
        // identity) is kept for consumers without frozen provenance.
        let continuation_scope: Option<ContinuationScope> =
            match context.task.payload.get(CONTINUATION_SCOPE_PAYLOAD_KEY) {
                None | Some(Value::Null) => None,
                Some(raw) => Some(
                    serde_json::from_value::<ContinuationScope>(raw.clone()).map_err(|e| {
                        self.terminal(
                            "continuation_scope_invalid",
                            format!("invalid '{CONTINUATION_SCOPE_PAYLOAD_KEY}': {e}"),
                        )
                    })?,
                ),
            };
        if let Some(scope) = &continuation_scope {
            if let Err(detail) = scope.validate() {
                return Err(self.terminal(
                    "continuation_scope_invalid",
                    format!("invalid '{CONTINUATION_SCOPE_PAYLOAD_KEY}': {detail}"),
                ));
            }
            // F8-b: the SAME cross-field validator admission runs before
            // persisting — kept here because the claim boundary is the
            // last line of defence against a stanza that entered through
            // a path without the admission gate.
            let stanza = context
                .task
                .payload
                .as_object()
                .expect("task payload is a JSON object");
            if let Err(detail) = agentkit_native::validate_frozen_semantics(stanza) {
                return Err(self.terminal(
                    "continuation_scope_mismatch",
                    format!("frozen continuation scope contradicts the frozen target: {detail}"),
                ));
            }
        }

        // 4. Lineage preconditions for exact resume (AGENTKIT-113,
        // hardened by review round-4 F3-B). The task-local observed
        // session proves IDENTITY only: WITH a frozen scope, reuse
        // additionally requires the durable continuation LEDGER to vouch
        // for the binding at the frozen scope — still available
        // (`active`) or `owned` by THIS attempt (claim re-entry), every
        // configuration component matching, lease validated when the
        // claim is taken. A retired, superseded, uncertain or
        // foreign-owned row is a MISS no matter what this task observed
        // locally — OneMind's same-task Resume (engine.resume on the
        // ORIGINAL task, unlike Resend/Regenerate) must never respawn
        // from a session the ledger has put out of service. Without a
        // frozen scope only the task's own observed identity remains
        // (documented same-task re-entry); the host-global "observed
        // somewhere" path stays retired (AGENTKIT-113).
        let mut exact_binding: Option<NativeContinuationBinding> = None;
        let lineage_parent: Option<String> = match &input.resume {
            TurnResume::Exact { vendor_session_id } => {
                let observed = self
                    .store
                    .current_native_binding(&context.task.task_id)
                    .await
                    .map_err(EngineError::Store)?
                    .filter(|binding| binding.vendor_session_id == *vendor_session_id);
                if let Some(scope) = &continuation_scope {
                    let vouched = self
                        .store
                        .resumable_continuation(
                            &scope.conversation,
                            &scope.parent,
                            scope.history_position,
                            &scope.backend,
                            &scope.executable,
                            &scope.config_identity,
                            scope.system_prompt_revision.as_deref(),
                            &context.attempt_id,
                        )
                        .await
                        .map_err(EngineError::Store)?
                        .filter(|binding| binding.vendor_session_id == *vendor_session_id);
                    match vouched {
                        Some(binding) => {
                            let session = binding.vendor_session_id.clone();
                            exact_binding = Some(binding);
                            Some(session)
                        }
                        None => {
                            let seen = self
                                .store
                                .active_continuation(
                                    &scope.conversation,
                                    &scope.parent,
                                    scope.history_position,
                                    &scope.backend,
                                    &scope.executable,
                                    &scope.config_identity,
                                    scope.system_prompt_revision.as_deref(),
                                )
                                .await
                                .map_err(EngineError::Store)?
                                .map(|b| format!("bound to '{}'", b.vendor_session_id))
                                .unwrap_or_else(|| "no active binding".to_owned());
                            let local_note = if observed.is_some() {
                                "restates a task-local observation the ledger does not vouch for and "
                            } else {
                                ""
                            };
                            return Err(self.terminal(
                                "continuation_mismatch",
                                format!(
                                    "exact resume of '{vendor_session_id}' {local_note}has no active-or-owned continuation at conversation '{}' parent '{}' position {history} ({seen})",
                                    scope.conversation,
                                    scope.parent,
                                    history = scope.history_position,
                                ),
                            ));
                        }
                    }
                } else {
                    match observed {
                        // Same-task re-entry without frozen provenance:
                        // the task's OWN observed identity is the only
                        // authority the generic path ever grants.
                        Some(binding) => Some(binding.vendor_session_id.clone()),
                        None => {
                            // No frozen scope ⇒ no authority (AGENTKIT-113).
                            // A host-global "we have seen this id" check
                            // proves only that some attempt somewhere on
                            // this machine observed the id — not that this
                            // conversation, this configuration and this
                            // history position own it. The generic path is
                            // retired: exact resume requires the complete
                            // scoped binding.
                            return Err(self.terminal(
                                "continuation_scope_missing",
                                format!(
                                    "exact resume of '{vendor_session_id}' was submitted without a frozen continuation scope; refusing an unscoped resume"
                                ),
                            ));
                        }
                    }
                }
            }
            TurnResume::Fork {
                vendor_session_id: _,
                expected_parent,
            } => Some(expected_parent.clone()),
            TurnResume::Fresh { .. } => None,
        };

        // 5. Preflight through the backend (executable identity, auth,
        // managed policy). Refusals never fall back to another route.
        let preflight: PreflightReport = match self
            .backend
            .preflight(&target, self.locator.as_ref(), &context.cancellation)
            .await
        {
            Ok(report) if report.blocking_issues.is_empty() => report,
            Ok(report) => {
                return Err(self.terminal("preflight_not_ready", report.blocking_issues.join("; ")));
            }
            Err(refusal) if refusal.retryable => {
                return Err(self.transient(&refusal.code, refusal.message));
            }
            Err(refusal) => return Err(self.terminal(&refusal.code, refusal.message)),
        };
        let _ = &preflight;

        // 6. Broker for the governed profile: started BEFORE the vendor
        // process, so a bridge failure never spawns a tool-less orphan.
        let broker = if target.profile == NativeProfile::OwnedMcpChat {
            let spec: BrokerSpec = serde_json::from_value(
                context
                    .task
                    .payload
                    .get(BROKER_PAYLOAD_KEY)
                    .cloned()
                    .ok_or_else(|| {
                        self.terminal(
                            "broker_catalog_missing",
                            "OwnedMcpChat requires a frozen broker catalog",
                        )
                    })?,
            )
            .map_err(|e| self.terminal("broker_catalog_invalid", e.to_string()))?;
            let Some(factory) = &self.broker_factory else {
                return Err(self.terminal(
                    "broker_unavailable",
                    "OwnedMcpChat requires a broker factory on the executor",
                ));
            };
            let catalog: Vec<BrokerToolDefinition> = spec
                .tools
                .into_iter()
                .map(|t| BrokerToolDefinition {
                    name: t.name,
                    description: t.description,
                    input_schema: t.input_schema,
                    writes: t.writes,
                })
                .collect();
            let limits = spec
                .limits
                .map_or(self.options.broker_limits, |l| BrokerLimits {
                    max_calls: l.max_calls,
                    max_args_bytes: l.max_args_bytes,
                    max_result_bytes: l.max_result_bytes,
                    call_timeout_ms: l.call_timeout_ms,
                    // The human-gate budget is a host policy, not a
                    // consumer wire field (review R5): consumers tune the
                    // effect timeout, never the approval deadline.
                    authorize_timeout_ms: self.options.broker_limits.authorize_timeout_ms,
                });
            let broker = factory
                .start(&AttemptBrokerContext {
                    task_id: &context.task.task_id,
                    attempt_id: &context.attempt_id,
                    server_name: &spec.server_name,
                    target: &target,
                    catalog,
                    limits,
                    cancellation: context.cancellation.clone(),
                    // The durable lease fence travels with the broker:
                    // governed tool dispatch is authorised from the task
                    // row, never from process-local state (AGENTKIT-115).
                    dispatch_guard: Some(context.dispatch_guard()),
                })
                .await?;
            Some(broker)
        } else {
            if context.task.payload.get(BROKER_PAYLOAD_KEY).is_some() {
                return Err(self.terminal(
                    "broker_not_allowed",
                    "plain Chat must not carry a broker catalog",
                ));
            }
            None
        };

        // 6b. Attempt resource owner: from here on, EVERY exit path —
        // including whole-future abortion — loses authority immediately
        // and gets bounded cleanup (broker revoke/shutdown, interaction
        // invalidation, per-run directory removal).
        let mut owner = AttemptOwner::new(
            self.store.clone(),
            &context.attempt_id,
            self.supervisor.clone(),
            broker.clone(),
            None,
        );

        // 7. Frozen plan.
        let plan = match self.backend.plan_turn(
            &target,
            &input,
            broker.as_ref().map(|b| b.spec()).as_ref(),
        ) {
            Ok(plan) => plan,
            Err(refusal) if refusal.retryable => {
                self.settle_cleanup(&context, broker.as_ref(), None).await;
                owner.mark_settled();
                return Err(self.transient(&refusal.code, refusal.message));
            }
            Err(refusal) => {
                self.settle_cleanup(&context, broker.as_ref(), None).await;
                owner.mark_settled();
                return Err(self.terminal(&refusal.code, refusal.message));
            }
        };
        owner.set_per_run_dir(plan.per_run_dir.clone());

        // 8. Frozen lineage BEFORE spawn: the frozen identity is bound
        // first (crash-mid-attempt can still be reasoned about). An exact
        // resume restates an identity the vendor already attested in a
        // prior attempt, so it is observed immediately; a FRESH identity
        // is ASSIGNED only — `observed` is reserved for the vendor's own
        // init frame, so a spawn that dies before init cannot mint a
        // resumable session that never existed at the vendor (AGENTKIT-113).
        let mut bound_session: Option<String> = None;
        let mut observed_session: Option<String> = None;
        let frozen_session: Option<String> = match &input.resume {
            TurnResume::Exact { vendor_session_id } => Some(vendor_session_id.clone()),
            _ => plan.native_session_id.clone(),
        };
        if let Some(session_id) = &frozen_session {
            match &input.resume {
                TurnResume::Exact { .. } => {
                    self.bind_and_observe(
                        &context,
                        &target.backend,
                        session_id,
                        lineage_parent.clone(),
                    )
                    .await?;
                    observed_session = Some(session_id.clone());
                }
                _ => {
                    self.store
                        .bind_native_session(BindNativeSession {
                            attempt_id: context.attempt_id.clone(),
                            task_id: context.task.task_id.clone(),
                            backend: target.backend.clone(),
                            vendor_session_id: session_id.clone(),
                            parent_vendor_session_id: lineage_parent.clone(),
                            assigned_at: context.runtime.now_ms(),
                        })
                        .await
                        .map_err(EngineError::Store)?;
                }
            }
            bound_session = Some(session_id.clone());
            self.emit_session(
                &context,
                "native.session.bound",
                session_id,
                &plan.fingerprint,
            )
            .await?;
        }

        // 8b. Atomic ledger claim (review round-4 F3-A/C): reserve the
        // INPUT-position binding before the vendor process exists, so a
        // concurrent attempt can never start from the same position
        // (compare-and-set at the store: exactly one winner, lease
        // fenced) and an interrupted attempt's position never reads as
        // available again. The loser takes the fresh-session decision —
        // refusal here, from which the consumer falls back to Fresh with
        // seeded ancestry — and never consumes the binding.
        let continuation_claim: Option<Arc<ContinuationClaim<R>>> = match (
            &continuation_scope,
            &exact_binding,
        ) {
            (Some(scope), Some(binding)) => {
                let claimed = self
                    .store
                    .claim_selected_continuation(
                        binding,
                        &context.task.task_id,
                        &context.attempt_id,
                        context.runtime.now_ms(),
                    )
                    .await
                    .map_err(|error| {
                        self.terminal(
                            "continuation_claim_refused",
                            format!("ledger claim refused: {error}"),
                        )
                    })?;
                match claimed {
                    Some(claimed) => Some(Arc::new(ContinuationClaim::new(
                        &self.store,
                        &context.attempt_id,
                        &claimed.binding_id,
                    ))),
                    None => {
                        return Err(self.terminal(
                                "continuation_mismatch",
                                format!(
                                    "exact resume of '{}' lost the atomic claim: another attempt owns the continuation at conversation '{}' parent '{}' position {history}",
                                    binding.vendor_session_id,
                                    scope.conversation,
                                    scope.parent,
                                    history = scope.history_position,
                                ),
                            ));
                    }
                }
            }
            _ => None,
        };
        owner.set_continuation(continuation_claim.clone());

        // 9. Spawn the owned process (readers start immediately inside).
        let mut child = match self
            .supervisor
            .spawn(&plan, &self.options.limits, &context.cancellation)
            .await
        {
            Ok(child) => child,
            Err(err) => {
                // Nothing was ever handed to the transport: the claim is
                // released (Available again), not left dangling (F3-A).
                if let Some(claim) = &continuation_claim {
                    claim.discharge(false).await;
                }
                self.settle_cleanup(&context, broker.as_ref(), Some(&plan.per_run_dir))
                    .await;
                let (code, message) = native_error_code(&err);
                // A spawn failure may have touched the OS; uncertain-safe.
                return Err(HostError::<R>::Failure(ExecutionFailure {
                    code: Some(code),
                    retryable: Some(true),
                    retry_safety: RetrySafety::UnknownOutcome,
                    ..ExecutionFailure::new(message)
                }));
            }
        };
        // The owner now carries this attempt's process identity: an
        // aborted future terminates THIS generation and only this one.
        owner.attach_child(child.pid(), child.generation());
        if let Err(error) = self
            .emit(
                &context,
                "native.spawn",
                json!({
                    "pid": child.pid(),
                    "generation": child.generation(),
                    "fingerprint": plan.fingerprint,
                    "nativeSessionId": bound_session,
                }),
            )
            .await
        {
            // Post-spawn persistence failure must never leave a running
            // vendor process behind: quarantine-interrupt, reap, settle.
            // The input never reached the transport — release the claim.
            if let Some(claim) = &continuation_claim {
                claim.discharge(false).await;
            }
            let _ = child.interrupt(InterruptPolicy::Quarantine).await;
            let _ = child.wait().await;
            self.settle_cleanup(&context, broker.as_ref(), Some(&plan.per_run_dir))
                .await;
            owner.mark_settled();
            return Err(error);
        }

        // 10. Deliver the admitted input. A transport fault here (early exit
        // before stdin drained) is recorded, not fatal: the stream loop and
        // the adapter decide the terminal outcome honestly.
        let input_bytes = match self.backend.encode_input(&input) {
            Ok(bytes) => bytes,
            Err(refusal) => {
                // Encoding died before any byte left: provable
                // non-acceptance — release the claim.
                if let Some(claim) = &continuation_claim {
                    claim.discharge(false).await;
                }
                let _ = child.interrupt(InterruptPolicy::Quarantine).await;
                let _ = child.wait().await;
                self.settle_cleanup(&context, broker.as_ref(), Some(&plan.per_run_dir))
                    .await;
                owner.mark_settled();
                return Err(self.terminal(&refusal.code, refusal.message));
            }
        };
        // Delivery can be cancelled after bytes reached the child but before
        // its future resolves. Reserve consumption before entering that future.
        if let Some(claim) = &continuation_claim {
            claim.mark_input_accepted();
        }
        if let Some(fault) = child.deliver_input(&input_bytes, true).await.err() {
            // Bytes entered the transport (possibly partially): from here
            // the vendor MAY have consumed the prompt — the honest
            // non-completion disposition is `uncertain`, never release.
            let emission = self
                .emit(
                    &context,
                    "native.input.fault",
                    json!({"code": fault.code, "message": fault.message}),
                )
                .await;
            if let Err(error) = emission {
                if let Some(claim) = &continuation_claim {
                    claim.discharge(true).await;
                }
                let _ = child.interrupt(InterruptPolicy::Quarantine).await;
                let _ = child.wait().await;
                self.settle_cleanup(&context, broker.as_ref(), Some(&plan.per_run_dir))
                    .await;
                owner.mark_settled();
                return Err(error);
            }
        }

        // 11. Stream loop with startup/idle/overall bounds and dual cancel.
        let mut adapter = self.backend.new_frame_adapter();
        let mut saw_event = false;
        let mut protocol_violation: Option<(String, String)> = None;
        let mut identity_mismatch: Option<String> = None;
        // Fix B: an owned-MCP attempt whose vendor `init` does not report the
        // broker's server as `connected` runs WITHOUT its tools; the model
        // may then fabricate tool calls as plain text. Fail closed.
        let mut mcp_bridge_failure: Option<String> = None;
        let mut transport_faults: Vec<TransportFault> = Vec::new();
        let mut timed_out: Option<FailureClass> = None;
        let mut interrupt_receipt = None;

        let (tx, mut rx) = mpsc::unbounded_channel::<InterruptPolicy>();
        let watcher = {
            let cancel = context.cancellation.clone();
            let interrupt = context.interruption.clone();
            let tx1 = tx.clone();
            let tx2 = tx.clone();
            tokio::spawn(async move {
                tokio::select! {
                    _ = cancel.cancelled() => { let _ = tx1.send(InterruptPolicy::HostShutdown); }
                    _ = interrupt.cancelled() => { let _ = tx2.send(InterruptPolicy::UserStop); }
                }
            })
        };
        drop(tx);

        // Persistence inside the stream phase may fail at any point; every
        // such failure must quarantine-interrupt, reap and settle BEFORE
        // returning — a `?` that skips the child is the leak review §2.5
        // names. The owner's Drop remains the safety net for whole-future
        // abortion (it needs no await here).
        macro_rules! try_reap {
            ($e:expr) => {
                match $e.await {
                    Ok(value) => value,
                    Err(error) => {
                        watcher.abort();
                        // Persistence died AFTER the input entered the
                        // transport: the position goes `uncertain`, never
                        // silently back to available (F3-A).
                        if let Some(claim) = &continuation_claim {
                            claim.discharge(true).await;
                        }
                        let _ = child.interrupt(InterruptPolicy::Quarantine).await;
                        let _ = child.wait().await;
                        self.settle_cleanup(&context, broker.as_ref(), Some(&plan.per_run_dir))
                            .await;
                        owner.mark_settled();
                        return Err(error.into());
                    }
                }
            };
        }

        let started = Instant::now();
        let overall = Duration::from_millis(input.turn_deadline_ms.max(1) as u64);
        let startup = Duration::from_millis(self.options.limits.startup_timeout_ms.max(1) as u64);
        let idle = Duration::from_millis(self.options.limits.idle_timeout_ms.max(1) as u64);
        // One cleanup round may escalate through every rung; bound the wait.
        let reap = Duration::from_millis(
            (self.options.limits.interrupt_grace_ms
                + self.options.limits.terminate_grace_ms
                + self.options.limits.cleanup_timeout_ms.max(1)) as u64,
        );

        loop {
            // Review §2.9: the turn deadline is wall clock MINUS the time
            // the broker spent parked behind human approval gates. A
            // parked approval is a decision being made, not the vendor
            // thinking; the consumer's approvalTimeoutMs bounds the wait,
            // the broker call quota bounds the loop.
            let parked = broker.as_ref().map_or(0, |b| b.parked_ms());
            let charged = started
                .elapsed()
                .as_millis()
                .saturating_sub(u128::from(parked))
                .min(u128::from(u64::MAX)) as u64;
            let overall_left = overall.saturating_sub(Duration::from_millis(charged));
            if overall_left.is_zero() && interrupt_receipt.is_none() {
                timed_out = Some(FailureClass::TurnDeadline);
                interrupt_receipt = Some(child.interrupt(InterruptPolicy::UserStop).await);
                continue;
            }
            if interrupt_receipt.is_some() {
                // Already interrupted: keep draining frames until EOF or the
                // reap window closes, then stop waiting on the stream.
                tokio::select! {
                    frame = child.next_frame() => {
                        match frame {
                            None => break,
                            Some(FrameLine::Line(line)) => {
                                if let Err(violation) = adapter.push_line(&line) {
                                    protocol_violation = Some((violation.code, violation.message));
                                }
                            }
                            Some(FrameLine::Fault(fault)) => transport_faults.push(fault),
                        }
                    }
                    _ = tokio::time::sleep(reap) => break,
                }
                continue;
            }
            let step_budget = if saw_event { idle } else { startup }.min(overall_left);
            tokio::select! {
                frame = child.next_frame() => match frame {
                    None => break,
                    Some(FrameLine::Line(line)) => match adapter.push_line(&line) {
                        Ok(new) => {
                            for event in new {
                                saw_event = true;
                                if let NativeEvent::Init { session_id, .. } = &event {
                                    if let Some(bound) = &bound_session {
                                        if bound != session_id {
                                            // Loud refusal, never absorbed.
                                            identity_mismatch = Some(format!(
                                                "init reports '{session_id}' but the attempt is bound to '{bound}'"
                                            ));
                                            interrupt_receipt = Some(
                                                child.interrupt(InterruptPolicy::Quarantine).await,
                                            );
                                            break;
                                        }
                                        if observed_session.is_none() {
                                            // Fresh attempt: the vendor has
                                            // now attested the assigned
                                            // identity — only now does it
                                            // become an exact-resume target.
                                            let obs_attempt = context.attempt_id.clone();
                                            let obs_session = session_id.clone();
                                            let obs_now = context.runtime.now_ms();
                                            try_reap!(async move {
                                                self.store
                                                    .observe_native_session(
                                                        &obs_attempt,
                                                        &obs_session,
                                                        obs_now,
                                                    )
                                                    .await
                                                    .map_err(EngineError::Store)
                                            });
                                            observed_session = Some(session_id.clone());
                                            try_reap!(self.emit_session(
                                                &context,
                                                "native.session.observed",
                                                session_id,
                                                &plan.fingerprint,
                                            ));
                                        }
                                    } else {
                                        try_reap!(self.bind_and_observe(
                                            &context,
                                            &target.backend,
                                            session_id,
                                            lineage_parent.clone(),
                                        ));
                                        bound_session = Some(session_id.clone());
                                        observed_session = Some(session_id.clone());
                                        try_reap!(self.emit_session(
                                            &context,
                                            "native.session.observed",
                                            session_id,
                                            &plan.fingerprint,
                                        ));
                                    }
                                }
                                try_reap!(self.emit(
                                    &context,
                                    "native.event",
                                    json!({
                                        "event": event,
                                        "fingerprint": plan.fingerprint,
                                    }),
                                ));
                                // Evidence is durable (above) before failing.
                                if let (Some(broker), NativeEvent::Init { mcp_servers, .. }) =
                                    (broker.as_ref(), &event)
                                    && mcp_bridge_failure.is_none()
                                {
                                    let server = broker.spec().server_name;
                                    let reported = mcp_servers
                                        .iter()
                                        .find(|entry| entry.name == server)
                                        .map(|entry| entry.status.as_str());
                                    if reported != Some("connected") {
                                        mcp_bridge_failure = Some(format!(
                                            "the owned MCP server '{server}' is not connected \
                                             (vendor reported status: {}); the turn would run \
                                             without its governed tools",
                                            reported.unwrap_or("not reported")
                                        ));
                                        interrupt_receipt = Some(
                                            child.interrupt(InterruptPolicy::Quarantine).await,
                                        );
                                    }
                                }
                            }
                        }
                        Err(violation) => {
                            protocol_violation = Some((violation.code, violation.message));
                            interrupt_receipt =
                                Some(child.interrupt(InterruptPolicy::Quarantine).await);
                        }
                    },
                    Some(FrameLine::Fault(fault)) => transport_faults.push(fault),
                },
                policy = rx.recv() => {
                    if let Some(policy) = policy {
                        interrupt_receipt = Some(child.interrupt(policy).await);
                    }
                },
                _ = tokio::time::sleep(step_budget) => {
                    // A call parked in the broker (human approval, governed
                    // effect) legitimately silences stdout: the vendor is
                    // waiting on US, not thinking. Idle and startup budgets
                    // never preempt such a wait, and approval parks are
                    // additionally credited against the turn deadline
                    // above; the broker's call quota and the consumer's
                    // approval timeout keep the whole thing bounded
                    // (review §2.9: a human decision is not inference
                    // silence).
                    if broker.as_ref().is_some_and(|b| b.live_calls() > 0) {
                        continue;
                    }
                    timed_out = Some(if saw_event {
                        FailureClass::IdleTimeout
                    } else {
                        FailureClass::StartupTimeout
                    });
                    interrupt_receipt = Some(child.interrupt(InterruptPolicy::UserStop).await);
                },
            }
        }
        watcher.abort();
        drop(rx);

        // 12. Reap to completion, bounded.
        let exit: ProcessExit = match tokio::time::timeout(
            Duration::from_millis((2 * self.options.limits.cleanup_timeout_ms.max(1)) as u64),
            child.wait(),
        )
        .await
        {
            Ok(exit) => exit,
            Err(_) => ProcessExit {
                pid: child.pid(),
                generation: child.generation(),
                exit_code: None,
                signal: None,
                after_interrupt: interrupt_receipt.is_some(),
            },
        };

        // Fix C: the CLI explains early deaths on stderr. Read the bounded,
        // sanitised tail once the child is reaped.
        let stderr_tail = agentkit_native::bounded_stderr_tail(&child.stderr_tail());

        // 13. Authoritative terminal outcome: the adapter decides (the only
        // component that saw the vendor's root result facts).
        if let Some(detail) = &identity_mismatch {
            // The vendor spoke under a different identity than the claim
            // vouches for: input was delivered, so the position is
            // uncertain, never handed back.
            if let Some(claim) = &continuation_claim {
                claim.discharge(true).await;
            }
            self.settle_cleanup(&context, broker.as_ref(), Some(&plan.per_run_dir))
                .await;
            owner.mark_settled();
            return Err(self.terminal("session_identity_mismatch", detail.clone()));
        }
        let outcome = if let Some((code, message)) = &protocol_violation {
            NativeTerminalOutcome::Failed {
                class: FailureClass::ProtocolViolation,
                message: format!("{code}: {message}"),
                partial_text: None,
            }
        } else if let Some(message) = &mcp_bridge_failure {
            NativeTerminalOutcome::Failed {
                class: FailureClass::McpBridgeUnavailable,
                message: message.clone(),
                partial_text: None,
            }
        } else if let Some(class) = timed_out {
            let partial = match adapter.finish() {
                FinishDisposition::NoResult { partial_text } => partial_text,
            };
            NativeTerminalOutcome::Failed {
                class,
                message: format!("turn exceeded its {} budget", class_code(class)),
                partial_text: partial,
            }
        } else {
            adapter.terminal_outcome(&exit)
        };

        // 14. Cleanup obligations (always run, bounded, failure is visible).
        // Review round-4 F5: a NORMAL completion is not proof the owned
        // process group went away — the supervisor's group reconcile reports
        // what it could not verify, and that uncertainty must reach the
        // attempt record alongside the interrupt-receipt and fault sources.
        let cleanup_uncertain = interrupt_receipt
            .as_ref()
            .is_some_and(|receipt| receipt.cleanup_uncertain)
            || transport_faults
                .iter()
                .any(|f| f.code == "cleanup_uncertain")
            || self
                .supervisor
                .descendant_cleanup(exit.pid, exit.generation)
                .as_ref()
                .is_some_and(|receipt| receipt.cleanup_uncertain);
        let mut cleanup_error = false;
        if tokio::time::timeout(
            Duration::from_millis(self.options.limits.cleanup_timeout_ms.max(1) as u64),
            self.settle_cleanup(&context, broker.as_ref(), Some(&plan.per_run_dir)),
        )
        .await
        .is_err()
        {
            cleanup_error = true;
        }
        // Settlement ran (or ran out of its bound, visibly): the owner no
        // longer owes the detached safety round.
        owner.mark_settled();

        // 15. Map the outcome; nothing here converts silence to success.
        match outcome {
            NativeTerminalOutcome::Completed {
                text,
                session_id,
                usage,
                permission_denials,
                observed_model,
                observed_effort,
            } => {
                if let Some(bound) = &bound_session
                    && bound != &session_id
                {
                    // The result arrived from a session the claim never
                    // vouched for: the claimed position already ate the
                    // input — uncertain, never handed back for exact
                    // resume.
                    if let Some(claim) = &continuation_claim {
                        claim.discharge(true).await;
                    }
                    return Err(self.terminal(
                        "session_identity_mismatch",
                        format!(
                            "vendor result came from '{session_id}' but the attempt is bound to '{bound}'"
                        ),
                    ));
                }
                if bound_session.is_none() {
                    self.bind_and_observe(&context, &target.backend, &session_id, lineage_parent)
                        .await?;
                }
                // Durable continuation binding: the FULL provenance scope
                // this completed turn establishes for a later exact resume
                // (review §2.3). The binding records the COMPLETED
                // position — the assistant message this answer becomes —
                // never the input position: regenerating from the input
                // must then miss this binding instead of resuming a
                // session that already contains the discarded answer. A
                // binding the ledger refuses (fenced prior owner, frozen
                // facts, missing authority) is surfaced, never silent —
                // and never converts a completed turn into a failure: the
                // next turn then honestly falls to Fresh with seeded
                // ancestry instead of guessing.
                // Conclusive completion settles the claim: the vendor
                // consumed the input and the commit below publishes the
                // NEW active position (the commit's alias sweep retires
                // the claimed input-position row — self-transition, never
                // fenced against this task).
                if let Some(claim) = &continuation_claim {
                    claim.settle();
                }
                if let Some(scope) = &continuation_scope {
                    match self
                        .store
                        .commit_continuation(CommitContinuation {
                            conversation: scope.conversation.clone(),
                            parent: scope.commit_parent.clone(),
                            history_position: scope.commit_history_position,
                            backend: scope.backend.clone(),
                            executable: scope.executable.clone(),
                            config_identity: scope.config_identity.clone(),
                            system_prompt_revision: scope.system_prompt_revision.clone(),
                            vendor_session_id: session_id.clone(),
                            owning_attempt_id: context.attempt_id.clone(),
                            owning_task_id: context.task.task_id.clone(),
                            committed_at: context.runtime.now_ms(),
                        })
                        .await
                    {
                        Ok(binding) => {
                            self.emit(
                                &context,
                                "native.continuation.bound",
                                json!({
                                    "bindingId": binding.binding_id,
                                    "conversation": binding.conversation,
                                    "parent": binding.parent,
                                    "historyPosition": binding.history_position,
                                    "vendorSessionId": binding.vendor_session_id,
                                    "fingerprint": plan.fingerprint,
                                }),
                            )
                            .await?;
                        }
                        Err(error) => {
                            self.emit(
                                &context,
                                "native.continuation.error",
                                json!({"detail": error.to_string()}),
                            )
                            .await?;
                        }
                    }
                }
                self.emit(
                    &context,
                    "native.completed",
                    json!({
                        "text": text,
                        "sessionId": session_id,
                        "usage": usage,
                        "permissionDenials": permission_denials,
                        "observedModel": observed_model,
                        "observedEffort": observed_effort,
                        "fingerprint": plan.fingerprint,
                        "cleanupUncertain": cleanup_uncertain,
                    }),
                )
                .await?;
                if cleanup_error {
                    // Result is durable; residual cleanup is visible, never silent.
                    self.emit(
                        &context,
                        "native.cleanup.uncertain",
                        json!({"detail": "attempt cleanup exceeded its bound"}),
                    )
                    .await?;
                }
                Ok(())
            }
            NativeTerminalOutcome::Interrupted { partial_text } => {
                // Review round-4 F3-A: interrupted after the input was
                // handed to the transport — the vendor may have consumed
                // the prompt and part of the answer. The claimed position
                // goes `uncertain`: exact resume from the pre-claim
                // position is refused until reconciled (the consumer's
                // honest path is a fresh session or reconciliation).
                if let Some(claim) = &continuation_claim {
                    claim.discharge(claim.accepted()).await;
                }
                self.emit(
                    &context,
                    "native.interrupted",
                    json!({
                        "partialText": partial_text,
                        "cleanupUncertain": cleanup_uncertain,
                        "fingerprint": plan.fingerprint,
                    }),
                )
                .await?;
                if context.cancellation.is_cancelled() || context.task.cancel_requested {
                    // Host shutdown or durable stop: the runner settles the
                    // task Cancelled from this error class.
                    return Err(HostError::<R>::Cancelled);
                }
                Err(self.terminal(
                    "native_interrupted",
                    "the vendor turn was interrupted before completing",
                ))
            }
            NativeTerminalOutcome::Failed {
                class,
                message,
                partial_text,
            } => {
                // Non-completion after the input entered the transport
                // leaves the position uncertain too — a failed turn may
                // still have advanced the mutable vendor session (F3-A).
                if let Some(claim) = &continuation_claim {
                    claim.discharge(claim.accepted()).await;
                }
                // Only exit-derived outcomes (no root result decided) get
                // the stderr suffix on the message returned to the runner.
                let message = agentkit_native::with_stderr_suffix(class, message, &stderr_tail);
                let mut payload = json!({
                    "class": class,
                    "message": message,
                    "partialText": partial_text,
                    "transportFaults": transport_faults,
                    "cleanupUncertain": cleanup_uncertain,
                    "fingerprint": plan.fingerprint,
                });
                if !stderr_tail.is_empty() {
                    payload["stderrTail"] = json!(stderr_tail);
                }
                self.emit(&context, "native.failed", payload).await?;
                let code = format!("native_{}", class_code(class));
                // Retryable only when the class proves the failure happened
                // before inference and the profile cannot have run an
                // application effect.
                let pre_inference = matches!(
                    class,
                    FailureClass::NotSignedIn
                        | FailureClass::AuthExpired
                        | FailureClass::QuotaExhausted
                        | FailureClass::ExecutableMissing
                        | FailureClass::ExecutableReplaced
                        | FailureClass::UnsupportedVersion
                        | FailureClass::ManagedPolicyConflict
                );
                let effect_free = target.profile == NativeProfile::PlainChat;
                if pre_inference && effect_free {
                    Err(HostError::<R>::Failure(ExecutionFailure {
                        code: Some(code),
                        retryable: Some(true),
                        retry_safety: RetrySafety::SafeToRetry,
                        ..ExecutionFailure::new(message)
                    }))
                } else {
                    Err(HostError::<R>::Failure(ExecutionFailure {
                        code: Some(code),
                        retryable: Some(false),
                        retry_safety: RetrySafety::UnknownOutcome,
                        ..ExecutionFailure::new(message)
                    }))
                }
            }
            NativeTerminalOutcome::Uncertain {
                class,
                detail,
                partial_text,
            } => {
                // An accepted-but-inconclusive turn is the textbook
                // uncertain case for the ledger too (F3-A).
                if let Some(claim) = &continuation_claim {
                    claim.discharge(claim.accepted()).await;
                }
                let detail = agentkit_native::with_stderr_suffix(class, detail, &stderr_tail);
                let mut payload = json!({
                    "class": class,
                    "detail": detail,
                    "partialText": partial_text,
                    "cleanupUncertain": cleanup_uncertain,
                    "fingerprint": plan.fingerprint,
                });
                if !stderr_tail.is_empty() {
                    payload["stderrTail"] = json!(stderr_tail);
                }
                self.emit(&context, "native.uncertain", payload).await?;
                Err(HostError::<R>::Failure(ExecutionFailure {
                    code: Some(format!("native_{}", class_code(class))),
                    retryable: Some(true),
                    // Unknown outcomes are never blindly replayed: with
                    // UnknownOutcome safety the runner parks the task for a
                    // human recovery decision (AGENTKIT-107 §7).
                    retry_safety: RetrySafety::UnknownOutcome,
                    ..ExecutionFailure::new(format!(
                        "native turn accepted without a conclusive outcome: {detail}"
                    ))
                }))
            }
            NativeTerminalOutcome::DeferredBlocked {
                reason,
                partial_text,
            } => {
                if let Some(claim) = &continuation_claim {
                    claim.discharge(claim.accepted()).await;
                }
                self.emit(
                    &context,
                    "native.deferred.blocked",
                    json!({"reason": reason, "partialText": partial_text}),
                )
                .await?;
                Err(self.terminal(
                    "native_deferred",
                    format!("native deferral is not supported in this profile: {reason}"),
                ))
            }
        }
    }
}

fn class_code(class: FailureClass) -> &'static str {
    match class {
        FailureClass::NotSignedIn => "not_signed_in",
        FailureClass::AuthExpired => "auth_expired",
        FailureClass::QuotaExhausted => "quota_exhausted",
        FailureClass::BillingBlocked => "billing_blocked",
        FailureClass::ModelNotEntitled => "model_not_entitled",
        FailureClass::UnsupportedVersion => "unsupported_version",
        FailureClass::ManagedPolicyConflict => "managed_policy_conflict",
        FailureClass::ExecutableMissing => "executable_missing",
        FailureClass::ExecutableReplaced => "executable_replaced",
        FailureClass::SpawnFailed => "spawn_failed",
        FailureClass::ProtocolViolation => "protocol_violation",
        FailureClass::StartupTimeout => "startup_timeout",
        FailureClass::IdleTimeout => "idle_timeout",
        FailureClass::TurnDeadline => "turn_deadline",
        FailureClass::ExitWithoutResult => "exit_without_result",
        FailureClass::ProcessLost => "process_lost",
        FailureClass::UncertainOutcome => "uncertain_outcome",
        FailureClass::CleanupUncertain => "cleanup_uncertain",
        FailureClass::McpBridgeUnavailable => "mcp_bridge_unavailable",
        FailureClass::UnsupportedContent => "unsupported_content",
        FailureClass::UnsupportedCapability => "unsupported_capability",
        FailureClass::DeferredUnsupported => "deferred_unsupported",
        FailureClass::Internal => "internal",
    }
}

fn native_error_code(error: &NativeError) -> (String, String) {
    match error {
        NativeError::Refusal { refusal } => (refusal.code.clone(), refusal.message.clone()),
        NativeError::Transport { code, message } => (format!("transport_{code}"), message.clone()),
        NativeError::Protocol { code, message } => (format!("protocol_{code}"), message.clone()),
        NativeError::StaleAttempt { attempt_id } => (
            "stale_attempt".into(),
            format!("attempt {attempt_id} lost authority"),
        ),
        NativeError::Cancelled => ("cancelled".into(), "cancelled".into()),
    }
}

/// Kind-dispatching executor: consumers keep ONE task kind per chat turn
/// (e.g. `onemind.chat`) and route by the frozen target — native payloads
/// run the durable native path; everything else (model-loop targets, and
/// legacy tasks without any target) delegates untouched to the existing
/// executor (AGENTKIT-107 §2 "one execution owner per turn", ONEMIND-340).
pub struct NativeDispatchExecutor<R> {
    pub native: Arc<dyn TaskExecutor<R>>,
    pub fallback: Arc<dyn TaskExecutor<R>>,
}

#[async_trait]
impl<R> TaskExecutor<R> for NativeDispatchExecutor<R>
where
    R: TaskRepository + NativeRunRepository<Error = <R as TaskRepository>::Error>,
{
    fn handles_verification(&self) -> bool {
        // Host-level (kind-wide) queries ask about the lane this wrapper
        // serves: the model-loop route is the one whose verification and
        // provider-snapshot behaviour any outside caller conditions on.
        // Native turns never resolve provider snapshots and are judged
        // per-payload through the _for variants.
        self.fallback.handles_verification()
    }
    fn resolves_provider_snapshots(&self) -> bool {
        self.fallback.resolves_provider_snapshots()
    }
    fn handles_verification_for(&self, payload: &Value) -> bool {
        match agentkit_native::parse_target(payload) {
            Ok(ExecutionTarget::Native(_)) => self.native.handles_verification_for(payload),
            _ => self.fallback.handles_verification_for(payload),
        }
    }
    fn resolves_provider_snapshots_for(&self, payload: &Value) -> bool {
        match agentkit_native::parse_target(payload) {
            Ok(ExecutionTarget::Native(_)) => self.native.resolves_provider_snapshots_for(payload),
            _ => self.fallback.resolves_provider_snapshots_for(payload),
        }
    }
    async fn execute(&self, context: ExecutionContext<R>) -> HostResult<R, ()> {
        match agentkit_native::parse_target(&context.task.payload) {
            Ok(ExecutionTarget::Native(_)) => self.native.execute(context).await,
            Ok(ExecutionTarget::ModelLoop(_)) => self.fallback.execute(context).await,
            Err(error) => {
                if context
                    .task
                    .payload
                    .get(EXECUTION_TARGET_PAYLOAD_KEY)
                    .is_some()
                {
                    // The key was frozen at admission but no longer parses
                    // (upgrade schema skew, corruption): the turn was
                    // explicitly admitted under one execution owner and
                    // MUST NOT be silently billed/executed by another.
                    // Refuse terminally — only a legacy task with no
                    // frozen target may reach the model-loop fallback.
                    return Err(HostError::<R>::Failure(ExecutionFailure {
                        code: Some("execution_target_invalid".into()),
                        retryable: Some(false),
                        retry_safety: RetrySafety::UnknownOutcome,
                        ..ExecutionFailure::new(error)
                    }));
                }
                self.fallback.execute(context).await
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentkit_engine::{SharedRuntime, SystemRuntime};
    use agentkit_mcp::{Backend as _, SessionScope};
    use agentkit_native::{
        AuthEvidence, AuthMode, CapabilitySnapshot, CapabilityState, ExecutableEvidence,
        LaunchPlan, McpEndpointSpec, NativeRefusal, OwnedChildHandle, PermissionPromptPolicy,
        PolicyState, StreamFrameAdapter,
    };
    use agentkit_native_broker::{AttemptBroker, BrokerAuthority};
    use agentkit_store::{ClaimInput, SqliteStore, SubmitTask};

    // The tests run the generic executor against the concrete SQLite
    // repository, which implements both ports.
    type HostError = agentkit_engine::EngineError<agentkit_store::StoreError>;
    type HostResult<T> = Result<T, HostError>;
    type ExecutionContext = agentkit_engine::ExecutionContext<SqliteStore>;
    use std::collections::{BTreeMap, VecDeque};
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    const BACKEND: &str = "claude-code";

    fn supported() -> CapabilityState {
        CapabilityState::Supported {
            qualified_version: "2.1.310".into(),
            evidence: "test".into(),
        }
    }

    fn snapshot(profile: NativeProfile, plain_unknown: bool) -> CapabilitySnapshot {
        let mut flags = BTreeMap::new();
        flags.insert("text-input".to_string(), supported());
        flags.insert("extracted-text-input".to_string(), supported());
        flags.insert(
            "plain-chat".to_string(),
            if plain_unknown {
                CapabilityState::Unknown
            } else {
                supported()
            },
        );
        flags.insert(
            "owned-mcp-chat".to_string(),
            if profile == NativeProfile::OwnedMcpChat {
                supported()
            } else {
                CapabilityState::Unsupported {
                    reason: "not exercised".into(),
                }
            },
        );
        CapabilitySnapshot {
            backend: BACKEND.into(),
            matrix_version: 1,
            qualified_version: "2.1.310".into(),
            flags,
        }
    }

    fn native_target(profile: NativeProfile, plain_unknown: bool) -> NativeExecutionTarget {
        NativeExecutionTarget {
            schema_version: agentkit_native::EXECUTION_TARGET_SCHEMA_VERSION,
            backend: BACKEND.into(),
            backend_version: Some("2.1.310".into()),
            profile,
            executable: "/usr/bin/claude".into(),
            requested_model: Some("sonnet".into()),
            requested_effort: None,
            auth_expectation: agentkit_native::AuthExpectation::SubscriptionLogin,
            capabilities: snapshot(profile, plain_unknown),
            config_identity: "cfg-digest".into(),
            system_prompt_revision: None,
            content: agentkit_native::ContentPolicy::default(),
        }
    }

    fn turn_input(resume: TurnResume) -> TurnInput {
        TurnInput {
            input_id: "in-1".into(),
            text: "hello".into(),
            context_blocks: vec![],
            resume,
            system_prompt_revision: None,
            system_prompt: None,
            max_vendor_turns: None,
            turn_deadline_ms: 5_000,
        }
    }

    fn fresh_input() -> TurnInput {
        turn_input(TurnResume::Fresh {
            seed_history: vec![],
        })
    }

    fn broker_spec() -> BrokerSpec {
        BrokerSpec {
            server_name: "onemind".into(),
            tools: vec![BrokerToolSpec {
                name: "note_search".into(),
                description: "Search notes".into(),
                input_schema: json!({"type": "object"}),
                writes: false,
            }],
            limits: None,
            authorization_policy: None,
        }
    }

    /// Round-4 F6a contract: the stanza carries the consumer's frozen
    /// authorization policy as an OPAQUE value (parse-and-preserve;
    /// `deny_unknown_fields` still refuses anything unnamed).
    #[test]
    fn broker_stanza_carries_the_frozen_policy_opaquely() {
        let with_policy = json!({
            "serverName": "onemind",
            "tools": [{"name": "note_search", "inputSchema": {"type": "object"}}],
            "authorizationPolicy": {
                "autoApproveReadOnly": false,
                "approvalTimeoutMs": 300000,
                "maxCallsPerTurn": 8,
                "turnDeadlineMs": 300000,
                "installAllowlist": ["note_search", "note_read"]
            }
        });
        let spec: BrokerSpec = serde_json::from_value(with_policy.clone()).unwrap();
        assert_eq!(
            spec.authorization_policy.as_ref().unwrap(),
            with_policy.get("authorizationPolicy").unwrap(),
            "the policy travels verbatim — the host never reshapes it"
        );
        // Absent stays None (pre-F6a stanzas keep parsing).
        let without = json!({
            "serverName": "onemind",
            "tools": [{"name": "note_search", "inputSchema": {"type": "object"}}]
        });
        assert!(
            serde_json::from_value::<BrokerSpec>(without)
                .unwrap()
                .authorization_policy
                .is_none()
        );
        // The gate stays strict for keys nobody declared.
        let mut rogue = with_policy;
        rogue["authorizationPolicy"]["injected"] = json!(true);
        assert!(
            serde_json::from_value::<BrokerSpec>(rogue.clone()).is_ok(),
            "the carrier is opaque"
        );
        rogue["rogueKey"] = json!(1);
        assert!(
            serde_json::from_value::<BrokerSpec>(rogue).is_err(),
            "deny_unknown_fields still fences the stanza shape"
        );
    }

    fn payload(
        target: &NativeExecutionTarget,
        input: &TurnInput,
        broker: Option<&BrokerSpec>,
    ) -> Value {
        let mut value = json!({
            EXECUTION_TARGET_PAYLOAD_KEY: {"native": serde_json::to_value(target).unwrap()},
            TURN_INPUT_PAYLOAD_KEY: serde_json::to_value(input).unwrap(),
        });
        if let Some(broker) = broker {
            value[BROKER_PAYLOAD_KEY] = serde_json::to_value(broker).unwrap();
        }
        value
    }

    // ----- scripted ports -------------------------------------------------

    #[derive(Clone, Default)]
    struct Shared {
        spawn_count: Arc<AtomicUsize>,
        interrupts: Arc<Mutex<Vec<InterruptPolicy>>>,
        delivered: Arc<Mutex<Vec<Vec<u8>>>>,
        frames: Arc<Mutex<VecDeque<Vec<u8>>>>,
        hang: Arc<AtomicBool>,
        delivery_hang: Arc<AtomicBool>,
        preflight_hang: Arc<AtomicBool>,
        preflight_entered: Arc<AtomicBool>,
        outcome: Arc<Mutex<Option<NativeTerminalOutcome>>>,
        preflight_refusal: Arc<Mutex<Option<NativeRefusal>>>,
        preflight_notes: Arc<Mutex<Vec<String>>>,
        preflight_blocking: Arc<Mutex<Vec<String>>>,
        plan_refusal: Arc<Mutex<Option<NativeRefusal>>>,
        preassigned: Arc<Mutex<Option<String>>>,
        seen_mcp: Arc<Mutex<Option<Option<McpEndpointSpec>>>>,
        child_live: Arc<AtomicBool>,
        /// Scripted stderr tail returned by the fake child.
        stderr: Arc<Mutex<Vec<u8>>>,
        /// Scripted exit code reported by the fake child's `wait`.
        exit_code: Arc<std::sync::atomic::AtomicI32>,
    }

    impl Shared {
        fn queue(&self, lines: &[&str]) {
            self.frames
                .lock()
                .unwrap()
                .extend(lines.iter().map(|l| l.as_bytes().to_vec()));
        }

        fn set_outcome(&self, outcome: NativeTerminalOutcome) {
            *self.outcome.lock().unwrap() = Some(outcome);
        }

        fn completed(session: &str, text: &str) -> NativeTerminalOutcome {
            NativeTerminalOutcome::Completed {
                text: text.into(),
                session_id: session.into(),
                usage: None,
                permission_denials: vec![],
                observed_model: Some("sonnet-observed".into()),
                observed_effort: None,
            }
        }
    }

    struct ScriptAdapter {
        shared: Shared,
    }

    impl StreamFrameAdapter for ScriptAdapter {
        fn push_line(
            &mut self,
            line: &[u8],
        ) -> Result<Vec<NativeEvent>, agentkit_native::ProtocolViolation> {
            let text = String::from_utf8_lossy(line).into_owned();
            if text.starts_with("VIOLATION") {
                return Err(agentkit_native::ProtocolViolation {
                    code: "bad_frame".into(),
                    message: text,
                });
            }
            let value: Value =
                serde_json::from_str(&text).map_err(|_| agentkit_native::ProtocolViolation {
                    code: "not_json".into(),
                    message: text,
                })?;
            match value["e"].as_str().unwrap_or_default() {
                "init" => Ok(vec![NativeEvent::Init {
                    session_id: value["s"].as_str().unwrap_or_default().to_string(),
                    observed_model: value["m"].as_str().map(str::to_string),
                    observed_effort: None,
                    observed_version: None,
                    capabilities: vec![],
                    // Scripted as `"mcp": {"<server>": "<status>"}`.
                    mcp_servers: value["mcp"]
                        .as_object()
                        .map(|servers| {
                            servers
                                .iter()
                                .map(|(name, status)| agentkit_native::McpServerStatus {
                                    name: name.clone(),
                                    status: status.as_str().unwrap_or_default().to_string(),
                                })
                                .collect()
                        })
                        .unwrap_or_default(),
                }]),
                "delta" => Ok(vec![NativeEvent::TextDelta {
                    text: value["t"].as_str().unwrap_or_default().to_string(),
                }]),
                _ => Ok(vec![]),
            }
        }

        fn finish(&mut self) -> FinishDisposition {
            FinishDisposition::NoResult { partial_text: None }
        }

        fn terminal_outcome(&self, _exit: &ProcessExit) -> NativeTerminalOutcome {
            self.shared.outcome.lock().unwrap().clone().unwrap_or(
                NativeTerminalOutcome::Uncertain {
                    class: FailureClass::ExitWithoutResult,
                    detail: "no scripted outcome".into(),
                    partial_text: None,
                },
            )
        }
    }

    struct FakeBackend {
        shared: Shared,
    }

    #[async_trait]
    impl NativeBackend for FakeBackend {
        fn backend_id(&self) -> &str {
            BACKEND
        }

        async fn preflight(
            &self,
            _target: &NativeExecutionTarget,
            _locator: &dyn ExecutableLocator,
            _cancel: &CancellationToken,
        ) -> Result<PreflightReport, NativeRefusal> {
            self.shared.preflight_entered.store(true, Ordering::SeqCst);
            while self.shared.preflight_hang.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
            if let Some(refusal) = self.shared.preflight_refusal.lock().unwrap().clone() {
                return Err(refusal);
            }
            Ok(PreflightReport {
                executable: ExecutableEvidence {
                    path: "/usr/bin/claude".into(),
                    version: "2.1.310".into(),
                    digest: None,
                },
                auth: AuthEvidence {
                    mode: AuthMode::Subscription {
                        account_scope: "pro".into(),
                    },
                    policy: PolicyState::None,
                    config_directory: None,
                },
                blocking_issues: self.shared.preflight_blocking.lock().unwrap().clone(),
                readiness_notes: self.shared.preflight_notes.lock().unwrap().clone(),
            })
        }

        fn plan_turn(
            &self,
            _target: &NativeExecutionTarget,
            _input: &TurnInput,
            mcp: Option<&McpEndpointSpec>,
        ) -> Result<LaunchPlan, NativeRefusal> {
            if let Some(refusal) = self.shared.plan_refusal.lock().unwrap().clone() {
                return Err(refusal);
            }
            *self.shared.seen_mcp.lock().unwrap() = Some(mcp.cloned());
            Ok(LaunchPlan {
                argv: vec!["claude".into(), "-p".into()],
                env: vec![],
                cwd: "/tmp".into(),
                per_run_dir: "/tmp/run".into(),
                permission_policy: PermissionPromptPolicy::None,
                max_vendor_turns: None,
                native_session_id: self.shared.preassigned.lock().unwrap().clone(),
                fingerprint: "fp-test".into(),
            })
        }

        fn encode_input(&self, input: &TurnInput) -> Result<Vec<u8>, NativeRefusal> {
            Ok(input.text.as_bytes().to_vec())
        }

        fn new_frame_adapter(&self) -> Box<dyn StreamFrameAdapter> {
            Box::new(ScriptAdapter {
                shared: self.shared.clone(),
            })
        }

        fn terminal_outcome(
            &self,
            _exit: &ProcessExit,
            _observed: &[NativeEvent],
            _partial: Option<&str>,
        ) -> NativeTerminalOutcome {
            unreachable!("the executor must ask the adapter, not the backend")
        }
    }

    struct FakeChild {
        shared: Shared,
    }

    #[async_trait]
    impl OwnedChildHandle for FakeChild {
        fn pid(&self) -> u32 {
            4242
        }
        fn generation(&self) -> u64 {
            7
        }
        async fn next_frame(&mut self) -> Option<FrameLine> {
            if let Some(line) = self.shared.frames.lock().unwrap().pop_front() {
                return Some(FrameLine::Line(line));
            }
            if self.shared.hang.load(Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_secs(3600)).await;
                return None;
            }
            None
        }
        fn stderr_tail(&self) -> Vec<u8> {
            self.shared.stderr.lock().unwrap().clone()
        }
        async fn deliver_input(
            &mut self,
            bytes: &[u8],
            _close: bool,
        ) -> Result<(), TransportFault> {
            self.shared.delivered.lock().unwrap().push(bytes.to_vec());
            if self.shared.delivery_hang.load(Ordering::SeqCst) {
                std::future::pending::<()>().await;
            }
            Ok(())
        }
        async fn interrupt(
            &mut self,
            policy: InterruptPolicy,
        ) -> agentkit_native::InterruptReceipt {
            self.shared.interrupts.lock().unwrap().push(policy);
            agentkit_native::InterruptReceipt {
                terminated: true,
                escalated_to_sigterm: false,
                escalated_to_sigkill: false,
                cleanup_uncertain: false,
                detail: None,
            }
        }
        async fn wait(&mut self) -> ProcessExit {
            self.shared.child_live.store(false, Ordering::SeqCst);
            ProcessExit {
                pid: 4242,
                generation: 7,
                exit_code: Some(self.shared.exit_code.load(Ordering::SeqCst)),
                signal: None,
                after_interrupt: !self.shared.interrupts.lock().unwrap().is_empty(),
            }
        }
    }

    struct FakeSupervisor {
        shared: Shared,
    }

    #[async_trait]
    impl ProcessSupervisor for FakeSupervisor {
        async fn spawn(
            &self,
            _plan: &LaunchPlan,
            _limits: &ResourceLimits,
            _cancellation: &CancellationToken,
        ) -> Result<Box<dyn OwnedChildHandle>, NativeError> {
            self.shared.spawn_count.fetch_add(1, Ordering::SeqCst);
            self.shared.child_live.store(true, Ordering::SeqCst);
            Ok(Box::new(FakeChild {
                shared: self.shared.clone(),
            }))
        }
        async fn drain_owned(
            &self,
            policy: InterruptPolicy,
        ) -> Vec<agentkit_native::InterruptReceipt> {
            // Mirrors the real supervisor: whatever is still owned and live
            // gets the policy applied exactly once.
            if !self.shared.child_live.swap(false, Ordering::SeqCst) {
                return vec![];
            }
            self.shared.interrupts.lock().unwrap().push(policy);
            vec![agentkit_native::InterruptReceipt {
                terminated: true,
                escalated_to_sigterm: false,
                escalated_to_sigkill: false,
                cleanup_uncertain: false,
                detail: None,
            }]
        }
        async fn terminate_owned(
            &self,
            pid: u32,
            generation: u64,
            policy: InterruptPolicy,
        ) -> Option<agentkit_native::InterruptReceipt> {
            // Mirrors the real supervisor's scoping: only the live child
            // with the exact identity is terminated; all others survive.
            if pid != 4242 || generation != 7 {
                return None;
            }
            if !self.shared.child_live.swap(false, Ordering::SeqCst) {
                return None;
            }
            self.shared.interrupts.lock().unwrap().push(policy);
            Some(agentkit_native::InterruptReceipt {
                terminated: true,
                escalated_to_sigterm: false,
                escalated_to_sigkill: false,
                cleanup_uncertain: false,
                detail: None,
            })
        }
    }

    /// Review round-4 F5: a supervisor whose normal-completion group
    /// reconcile could NOT confirm the owned process group empty. Spawning,
    /// draining and scoped termination are exactly [`FakeSupervisor`]'s —
    /// only the F5 read-back differs, so the assertion isolates it. The
    /// plain `FakeSupervisor` deliberately does NOT implement
    /// `descendant_cleanup`, pinning the port default (`None` = no report)
    /// as clean-looking-but-unreported.
    struct UncertainDescendantSupervisor {
        inner: FakeSupervisor,
    }

    #[async_trait]
    impl ProcessSupervisor for UncertainDescendantSupervisor {
        async fn spawn(
            &self,
            plan: &LaunchPlan,
            limits: &ResourceLimits,
            cancellation: &CancellationToken,
        ) -> Result<Box<dyn OwnedChildHandle>, NativeError> {
            self.inner.spawn(plan, limits, cancellation).await
        }
        async fn drain_owned(
            &self,
            policy: InterruptPolicy,
        ) -> Vec<agentkit_native::InterruptReceipt> {
            self.inner.drain_owned(policy).await
        }
        async fn terminate_owned(
            &self,
            pid: u32,
            generation: u64,
            policy: InterruptPolicy,
        ) -> Option<agentkit_native::InterruptReceipt> {
            self.inner.terminate_owned(pid, generation, policy).await
        }
        fn descendant_cleanup(
            &self,
            pid: u32,
            generation: u64,
        ) -> Option<agentkit_native::InterruptReceipt> {
            if pid != 4242 || generation != 7 {
                return None;
            }
            Some(agentkit_native::InterruptReceipt {
                terminated: true,
                escalated_to_sigterm: true,
                escalated_to_sigkill: true,
                cleanup_uncertain: true,
                detail: Some(
                    "policy=normalCompletion; 1 process-group member(s) survived SIGKILL: \
                     descendants unverified"
                        .to_owned(),
                ),
            })
        }
    }

    struct FakeLocator;

    #[async_trait]
    impl ExecutableLocator for FakeLocator {
        async fn locate(&self) -> Result<ExecutableEvidence, NativeRefusal> {
            Ok(ExecutableEvidence {
                path: "/usr/bin/claude".into(),
                version: "2.1.310".into(),
                digest: None,
            })
        }
        async fn auth_status(&self) -> Result<AuthEvidence, NativeRefusal> {
            Ok(AuthEvidence {
                mode: AuthMode::Subscription {
                    account_scope: "pro".into(),
                },
                policy: PolicyState::None,
                config_directory: None,
            })
        }
    }

    // ----- harness ---------------------------------------------------------

    struct Harness {
        store: SqliteStore,
        shared: Shared,
        runtime: SharedRuntime,
        factory: Arc<RecordingFactory>,
    }

    type BrokerContextRecord = (String, String, String, usize);

    struct RecordingFactory {
        started: Arc<Mutex<Vec<Arc<AttemptBroker>>>>,
        contexts: Arc<Mutex<Vec<BrokerContextRecord>>>,
    }

    #[async_trait]
    impl AttemptBrokerFactory for RecordingFactory {
        async fn start(
            &self,
            ctx: &AttemptBrokerContext<'_>,
        ) -> Result<Arc<dyn AttemptBrokerHandle>, agentkit_engine::ExecutionFailure> {
            self.contexts.lock().unwrap().push((
                ctx.task_id.to_string(),
                ctx.attempt_id.to_string(),
                ctx.server_name.to_string(),
                ctx.catalog.len(),
            ));
            let authority = Arc::new(AllowAll);
            let broker = AttemptBroker::start(
                ctx.task_id,
                ctx.attempt_id,
                ctx.server_name,
                ctx.catalog.clone(),
                ctx.limits,
                authority,
            )
            .await
            .map_err(ExecutionFailure::new)?;
            self.started.lock().unwrap().push(broker.clone());
            Ok(broker)
        }
    }

    struct AllowAll;

    #[async_trait]
    impl BrokerAuthority for AllowAll {
        async fn record_lifecycle(
            &self,
            _invocation: &agentkit_native_broker::BrokerInvocation,
            _phase: &str,
            _outcome: &Value,
        ) -> Result<(), agentkit_native_broker::BrokerDenial> {
            Ok(())
        }
        fn attempt_generation(&self) -> Option<u64> {
            Some(7)
        }
        async fn authorize(
            &self,
            _invocation: &agentkit_native_broker::BrokerInvocation,
            _arguments: &Value,
        ) -> Result<(), agentkit_native_broker::BrokerDenial> {
            Ok(())
        }
        async fn execute(
            &self,
            _invocation: &agentkit_native_broker::BrokerInvocation,
            _arguments: Value,
        ) -> Result<agentkit_native_broker::BrokerExecuted, agentkit_native_broker::BrokerDenial>
        {
            Ok(agentkit_native_broker::BrokerExecuted {
                content: vec![json!({"type": "text", "text": "ok"})],
                structured: None,
            })
        }
    }

    fn harness() -> Harness {
        Harness {
            store: SqliteStore::memory(64).unwrap(),
            shared: Shared::default(),
            runtime: Arc::new(SystemRuntime),
            factory: Arc::new(RecordingFactory {
                started: Arc::new(Mutex::new(Vec::new())),
                contexts: Arc::new(Mutex::new(Vec::new())),
            }),
        }
    }

    impl Harness {
        async fn submit(&self, task_id: &str, payload: &Value) {
            self.store
                .submit(SubmitTask {
                    task_id: task_id.into(),
                    kind: "native.turn".into(),
                    scope_id: task_id.into(),
                    payload: payload.clone(),
                    priority: 0,
                    now: self.runtime.now_ms(),
                    available_at: None,
                    depends_on: vec![],
                    parent_task_id: None,
                    exclusive_scope: false,
                })
                .await
                .unwrap();
        }

        async fn claim(&self, task_id: &str) -> agentkit_store::ClaimedTask {
            let claimed = self
                .store
                .claim(ClaimInput {
                    owner_id: "worker-1".into(),
                    now: self.runtime.now_ms(),
                    ttl_ms: 600_000,
                    kinds: vec!["native.turn".into()],
                    busy_scopes: vec![],
                    max_attempts: 5,
                })
                .await
                .unwrap()
                .unwrap_or_else(|| panic!("task {task_id} was not claimable"));
            assert_eq!(claimed.task.task_id, task_id);
            claimed
        }

        fn context(
            &self,
            claimed: &agentkit_store::ClaimedTask,
            cancellation: CancellationToken,
            interruption: CancellationToken,
        ) -> ExecutionContext {
            ExecutionContext {
                task: claimed.task.clone(),
                cancellation,
                interruption,
                suspension: Arc::new(tokio::sync::Mutex::new(None)),
                attempt_id: claimed.lease.attempt_id.clone(),
                store: self.store.clone(),
                fence: claimed.lease.fence(),
                runtime: self.runtime.clone(),
                event_gate: Arc::new(tokio::sync::Mutex::new(())),
            }
        }

        fn executor(&self) -> NativeAgentExecutor<SqliteStore> {
            self.executor_with(Arc::new(FakeSupervisor {
                shared: self.shared.clone(),
            }))
        }

        /// Round-4 F5: the same executor over a caller-chosen supervisor, so
        /// a test can vary only the supervisor's descendant-cleanup report.
        fn executor_with(
            &self,
            supervisor: Arc<dyn ProcessSupervisor>,
        ) -> NativeAgentExecutor<SqliteStore> {
            let backend = Arc::new(FakeBackend {
                shared: self.shared.clone(),
            });
            NativeAgentExecutor::new(
                self.store.clone(),
                backend,
                supervisor,
                Arc::new(FakeLocator),
                Some(self.factory.clone()),
                NativeExecutorOptions {
                    kinds: vec!["native.turn".into()],
                    limits: ResourceLimits {
                        max_frame_bytes: 1 << 20,
                        max_stderr_bytes: 4096,
                        max_pending_events: 64,
                        max_stdin_bytes: 1 << 20,
                        startup_timeout_ms: 250,
                        idle_timeout_ms: 250,
                        interrupt_grace_ms: 5,
                        terminate_grace_ms: 5,
                        cleanup_timeout_ms: 100,
                    },
                    broker_limits: BrokerLimits::default(),
                },
            )
        }

        async fn event_types(&self, task_id: &str) -> Vec<String> {
            self.store
                .events(task_id, None, 4096)
                .await
                .unwrap()
                .into_iter()
                .filter_map(|record| {
                    record.event["type"]
                        .as_str()
                        .map(|t| t.strip_prefix("task.").map_or(t, |rest| rest).to_string())
                })
                .collect()
        }

        /// Payload (`data`) of the first durable event whose type ends with
        /// `suffix`.
        async fn event_data(&self, task_id: &str, suffix: &str) -> Option<Value> {
            self.store
                .events(task_id, None, 4096)
                .await
                .unwrap()
                .into_iter()
                .find(|record| {
                    record.event["type"]
                        .as_str()
                        .is_some_and(|t| t.ends_with(suffix))
                })
                .map(|record| record.event["data"].clone())
        }

        fn failure_message(err: &HostError) -> String {
            match err {
                HostError::Failure(failure) => failure.message.clone(),
                other => panic!("expected an execution failure, got {other:?}"),
            }
        }

        fn failure(err: HostError) -> (Option<String>, Option<bool>) {
            match err {
                HostError::Failure(failure) => (failure.code, failure.retryable),
                other => panic!("expected an execution failure, got {other:?}"),
            }
        }
    }

    // ----- tests ------------------------------------------------------------

    #[tokio::test]
    async fn completed_turn_binds_lineage_and_emits() {
        let h = harness();
        h.shared.queue(&[
            r#"{"e":"init","s":"vendor-1","m":"sonnet-x"}"#,
            r#"{"e":"delta","t":"hel"}"#,
            r#"{"e":"delta","t":"lo"}"#,
        ]);
        h.shared.set_outcome(Shared::completed("vendor-1", "hello"));
        h.submit(
            "t1",
            &payload(
                &native_target(NativeProfile::PlainChat, false),
                &fresh_input(),
                None,
            ),
        )
        .await;
        let claimed = h.claim("t1").await;
        let executor = h.executor();
        executor
            .execute(h.context(&claimed, CancellationToken::new(), CancellationToken::new()))
            .await
            .expect("turn completes");
        let binding = h.store.current_native_binding("t1").await.unwrap().unwrap();
        assert_eq!(binding.vendor_session_id, "vendor-1");
        assert_eq!(binding.state, "observed");
        let types = h.event_types("t1").await;
        for expected in [
            "native.spawn",
            "native.event",
            "native.session.observed",
            "native.completed",
        ] {
            assert!(
                types.iter().any(|t| t == expected),
                "missing {expected} in {types:?}"
            );
        }
        assert_eq!(
            h.shared.delivered.lock().unwrap().as_slice(),
            [b"hello".to_vec()]
        );
        assert_eq!(h.shared.spawn_count.load(Ordering::SeqCst), 1);
    }

    /// Round-4 F5: an unconfirmed normal-completion process-group reconcile
    /// must reach the durable attempt record as `cleanupUncertain: true`.
    /// No interrupt ladder ran and no transport fault occurred, so the
    /// supervisor's read-back is the ONLY possible source of the flag.
    #[tokio::test]
    async fn unconfirmed_descendant_cleanup_marks_completed_attempt_uncertain() {
        let h = harness();
        h.shared.queue(&[
            r#"{"e":"init","s":"vendor-1","m":"sonnet-x"}"#,
            r#"{"e":"delta","t":"hel"}"#,
            r#"{"e":"delta","t":"lo"}"#,
        ]);
        h.shared.set_outcome(Shared::completed("vendor-1", "hello"));
        h.submit(
            "t1",
            &payload(
                &native_target(NativeProfile::PlainChat, false),
                &fresh_input(),
                None,
            ),
        )
        .await;
        let claimed = h.claim("t1").await;
        let executor = h.executor_with(Arc::new(UncertainDescendantSupervisor {
            inner: FakeSupervisor {
                shared: h.shared.clone(),
            },
        }));
        executor
            .execute(h.context(&claimed, CancellationToken::new(), CancellationToken::new()))
            .await
            .expect("the turn itself completes normally");
        let completed = h
            .store
            .events("t1", None, 4096)
            .await
            .unwrap()
            .into_iter()
            .find(|record| {
                record.event["type"]
                    .as_str()
                    .is_some_and(|t| t.ends_with("native.completed"))
            })
            .expect("a completed attempt record");
        assert!(
            h.shared.interrupts.lock().unwrap().is_empty(),
            "no ladder ran: the supervisor report alone must set the flag"
        );
        assert_eq!(
            completed.event["data"]["cleanupUncertain"],
            json!(true),
            "unverified descendants must be visible uncertainty on a completed turn: {:?}",
            completed.event
        );
    }

    /// Round-4 F5 (pinned default): a supervisor that reports nothing — the
    /// port default, what every pre-existing implementor gets — leaves a
    /// completed turn's record clean, exactly as before the F5 wiring.
    #[tokio::test]
    async fn reported_nothing_keeps_completed_attempt_certain() {
        let h = harness();
        h.shared.queue(&[
            r#"{"e":"init","s":"vendor-1","m":"sonnet-x"}"#,
            r#"{"e":"delta","t":"hel"}"#,
            r#"{"e":"delta","t":"lo"}"#,
        ]);
        h.shared.set_outcome(Shared::completed("vendor-1", "hello"));
        h.submit(
            "t1",
            &payload(
                &native_target(NativeProfile::PlainChat, false),
                &fresh_input(),
                None,
            ),
        )
        .await;
        let claimed = h.claim("t1").await;
        h.executor()
            .execute(h.context(&claimed, CancellationToken::new(), CancellationToken::new()))
            .await
            .expect("the turn completes normally");
        let completed = h
            .store
            .events("t1", None, 4096)
            .await
            .unwrap()
            .into_iter()
            .find(|record| {
                record.event["type"]
                    .as_str()
                    .is_some_and(|t| t.ends_with("native.completed"))
            })
            .expect("a completed attempt record");
        assert_eq!(
            completed.event["data"]["cleanupUncertain"],
            json!(false),
            "no report and no fault must stay certain: {:?}",
            completed.event
        );
    }

    #[tokio::test]
    async fn exit_without_result_is_uncertain_never_completed() {
        let h = harness();
        // No frames, no scripted outcome: EOF without a root result.
        h.submit(
            "t1",
            &payload(
                &native_target(NativeProfile::PlainChat, false),
                &fresh_input(),
                None,
            ),
        )
        .await;
        let claimed = h.claim("t1").await;
        let err = h
            .executor()
            .execute(h.context(&claimed, CancellationToken::new(), CancellationToken::new()))
            .await
            .unwrap_err();
        let (code, retryable) = Harness::failure(err);
        assert_eq!(code.as_deref(), Some("native_exit_without_result"));
        assert_eq!(retryable, Some(true));
        assert!(
            h.event_types("t1")
                .await
                .iter()
                .any(|t| t == "native.uncertain")
        );
        assert!(
            h.store
                .current_native_binding("t1")
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn model_loop_target_is_refused_by_native_executor() {
        let h = harness();
        let value = json!({
            EXECUTION_TARGET_PAYLOAD_KEY: {"modelLoop": {
                "schemaVersion": agentkit_native::EXECUTION_TARGET_SCHEMA_VERSION,
                "providerId": "p", "model": "m"
            }},
            TURN_INPUT_PAYLOAD_KEY: serde_json::to_value(fresh_input()).unwrap(),
        });
        h.submit("t1", &value).await;
        let claimed = h.claim("t1").await;
        let (code, retryable) = Harness::failure(
            h.executor()
                .execute(h.context(&claimed, CancellationToken::new(), CancellationToken::new()))
                .await
                .unwrap_err(),
        );
        assert_eq!(code.as_deref(), Some("wrong_dispatch"));
        assert_eq!(retryable, Some(false));
        assert_eq!(h.shared.spawn_count.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn unknown_capability_is_transient_structural_is_terminal() {
        let h = harness();
        h.submit(
            "t1",
            &payload(
                &native_target(NativeProfile::PlainChat, true),
                &fresh_input(),
                None,
            ),
        )
        .await;
        let claimed = h.claim("t1").await;
        let (code, retryable) = Harness::failure(
            h.executor()
                .execute(h.context(&claimed, CancellationToken::new(), CancellationToken::new()))
                .await
                .unwrap_err(),
        );
        assert_eq!(code.as_deref(), Some("capability_violation"));
        assert_eq!(
            retryable,
            Some(true),
            "unknown-only evidence must be transient"
        );
        assert_eq!(h.shared.spawn_count.load(Ordering::SeqCst), 0);

        // Structural: images enabled without capability.
        let mut target = native_target(NativeProfile::PlainChat, false);
        target.content.images = true;
        let h = harness();
        h.submit("t2", &payload(&target, &fresh_input(), None))
            .await;
        let claimed = h.claim("t2").await;
        let (code, retryable) = Harness::failure(
            h.executor()
                .execute(h.context(&claimed, CancellationToken::new(), CancellationToken::new()))
                .await
                .unwrap_err(),
        );
        assert_eq!(code.as_deref(), Some("capability_violation"));
        assert_eq!(retryable, Some(false));
    }

    #[tokio::test]
    async fn unscoped_exact_resume_is_refused() {
        // AGENTKIT-113: the host-global "observed somewhere" fallback is
        // retired. Without a frozen scope there is no conversation,
        // configuration or position to authorise the resume.
        let h = harness();
        let input = turn_input(TurnResume::Exact {
            vendor_session_id: "v-old".into(),
        });
        h.submit(
            "t1",
            &payload(
                &native_target(NativeProfile::PlainChat, false),
                &input,
                None,
            ),
        )
        .await;
        let claimed = h.claim("t1").await;
        let (code, _) = Harness::failure(
            h.executor()
                .execute(h.context(&claimed, CancellationToken::new(), CancellationToken::new()))
                .await
                .unwrap_err(),
        );
        assert_eq!(code.as_deref(), Some("continuation_scope_missing"));
        assert_eq!(h.shared.spawn_count.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn exact_resume_with_observed_lineage_runs_and_restates_binding() {
        let h = harness();
        let input = turn_input(TurnResume::Exact {
            vendor_session_id: "v-old".into(),
        });
        h.submit(
            "t1",
            &payload(
                &native_target(NativeProfile::PlainChat, false),
                &input,
                None,
            ),
        )
        .await;
        // A previous attempt observed the lineage.
        h.store
            .bind_native_session(BindNativeSession {
                attempt_id: "att-0".into(),
                task_id: "t1".into(),
                backend: BACKEND.into(),
                vendor_session_id: "v-old".into(),
                parent_vendor_session_id: None,
                assigned_at: h.runtime.now_ms(),
            })
            .await
            .unwrap();
        h.store
            .observe_native_session("att-0", "v-old", h.runtime.now_ms())
            .await
            .unwrap();
        h.shared
            .set_outcome(Shared::completed("v-old", "continued"));
        let claimed = h.claim("t1").await;
        h.executor()
            .execute(h.context(&claimed, CancellationToken::new(), CancellationToken::new()))
            .await
            .expect("exact resume runs");
        let binding = h
            .store
            .current_native_binding("t1")
            .await
            .unwrap()
            .expect("observed lineage present");
        assert_eq!(binding.vendor_session_id, "v-old");
        assert_eq!(h.shared.spawn_count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn informational_preflight_notes_never_block_the_turn() {
        // Reviewer P1-1 regression: the driver's happy-path preflight is
        // informational (binary observed, sign-in observed). The executor
        // must fail open on notes and only refuse on blocking issues.
        let h = harness();
        h.shared.preflight_notes.lock().unwrap().extend([
            "official binary observed (version 2.1.310)".to_string(),
            "subscription sign-in observed (scope: account)".to_string(),
        ]);
        h.shared
            .queue(&[r#"{"e":"init","s":"v-note","m":"sonnet-x"}"#]);
        h.shared.set_outcome(Shared::completed("v-note", "noted"));
        h.submit(
            "t1",
            &payload(
                &native_target(NativeProfile::PlainChat, false),
                &fresh_input(),
                None,
            ),
        )
        .await;
        let claimed = h.claim("t1").await;
        h.executor()
            .execute(h.context(&claimed, CancellationToken::new(), CancellationToken::new()))
            .await
            .expect("informational notes never block the turn");
        assert_eq!(h.shared.spawn_count.load(Ordering::SeqCst), 1);
        assert_eq!(
            h.store
                .current_native_binding("t1")
                .await
                .unwrap()
                .unwrap()
                .vendor_session_id,
            "v-note"
        );
    }

    #[tokio::test]
    async fn blocking_preflight_issue_refuses_without_spawning() {
        // The mirror case: a genuine blocking issue is still a terminal
        // refusal with zero OS contact.
        let h = harness();
        h.shared
            .preflight_blocking
            .lock()
            .unwrap()
            .push("no official Claude Code binary found".into());
        h.submit(
            "t1",
            &payload(
                &native_target(NativeProfile::PlainChat, false),
                &fresh_input(),
                None,
            ),
        )
        .await;
        let claimed = h.claim("t1").await;
        let (code, retryable) = Harness::failure(
            h.executor()
                .execute(h.context(&claimed, CancellationToken::new(), CancellationToken::new()))
                .await
                .unwrap_err(),
        );
        assert_eq!(code.as_deref(), Some("preflight_not_ready"));
        assert_eq!(
            retryable,
            Some(false),
            "a blocking issue is an operator fix, not an automatic requeue"
        );
        assert_eq!(h.shared.spawn_count.load(Ordering::SeqCst), 0);
        assert!(
            h.store
                .current_native_binding("t1")
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn cross_task_exact_resume_uses_previous_tasks_lineage() {
        // Reviewer P1-2 / ONEMIND-341 regression: a follow-up turn asks to
        // resume the vendor session a *previous* task observed. The
        // lineage lives on the earlier task's committed binding, not this
        // task; refusing it (the pre-fix behavior) killed every second
        // turn. AGENTKIT-113: the resume is authorised by the scoped
        // binding — this turn is submitted from the position the previous
        // answer completed at.
        let h = harness();
        // Task one starts fresh and the vendor mints the session.
        h.shared
            .queue(&[r#"{"e":"init","s":"v-shared","m":"sonnet-x"}"#]);
        h.shared.set_outcome(Shared::completed("v-shared", "first"));
        h.submit(
            "t-first",
            &scoped_payload(
                &native_target(NativeProfile::PlainChat, false),
                &fresh_input(),
                None,
                Some(&scope("convo-a", "parent-0", 0)),
            ),
        )
        .await;
        let claimed = h.claim("t-first").await;
        h.executor()
            .execute(h.context(&claimed, CancellationToken::new(), CancellationToken::new()))
            .await
            .expect("first turn completes");

        // Task two is a brand-new task record resuming that exact session,
        // submitted from the position task one's answer completed at.
        let input = turn_input(TurnResume::Exact {
            vendor_session_id: "v-shared".into(),
        });
        h.shared
            .set_outcome(Shared::completed("v-shared", "second"));
        h.submit(
            "t-second",
            &scoped_payload(
                &native_target(NativeProfile::PlainChat, false),
                &input,
                None,
                Some(&scope("convo-a", "parent-0:answer", 1)),
            ),
        )
        .await;
        let claimed = h.claim("t-second").await;
        h.executor()
            .execute(h.context(&claimed, CancellationToken::new(), CancellationToken::new()))
            .await
            .expect("cross-task exact resume runs, not lineage_missing");
        assert_eq!(
            h.shared.spawn_count.load(Ordering::SeqCst),
            2,
            "the resumed turn actually spawned"
        );
        let binding = h
            .store
            .current_native_binding("t-second")
            .await
            .unwrap()
            .expect("restated lineage");
        assert_eq!(binding.vendor_session_id, "v-shared");
        assert_eq!(binding.state, "observed");
    }

    #[tokio::test]
    async fn unobserved_assigned_session_is_never_resumable() {
        // Reviewer F3 regression: a fresh turn pre-assigns our own session
        // id before spawn; if the vendor dies before `init` that id is a
        // phantom. It must stay `assigned` (never `observed`) and an Exact
        // resume of it must be refused as lineage_missing.
        let h = harness();
        *h.shared.preassigned.lock().unwrap() = Some("s-phantom".into());
        // No frames, no outcome: EOF without init or result.
        h.submit(
            "t1",
            &payload(
                &native_target(NativeProfile::PlainChat, false),
                &fresh_input(),
                None,
            ),
        )
        .await;
        let claimed = h.claim("t1").await;
        let (code, _) = Harness::failure(
            h.executor()
                .execute(h.context(&claimed, CancellationToken::new(), CancellationToken::new()))
                .await
                .unwrap_err(),
        );
        assert_eq!(code.as_deref(), Some("native_exit_without_result"));
        assert!(
            h.store
                .current_native_binding("t1")
                .await
                .unwrap()
                .is_none(),
            "an unobserved id is never a current binding"
        );
        assert!(
            !h.store
                .is_observed_native_session("s-phantom")
                .await
                .unwrap(),
            "never-echoed ids must not become observed lineage"
        );

        // A later turn asking to resume the phantom is honestly refused.
        let input = turn_input(TurnResume::Exact {
            vendor_session_id: "s-phantom".into(),
        });
        h.submit(
            "t2",
            &payload(
                &native_target(NativeProfile::PlainChat, false),
                &input,
                None,
            ),
        )
        .await;
        let claimed = h.claim("t2").await;
        let (code, _) = Harness::failure(
            h.executor()
                .execute(h.context(&claimed, CancellationToken::new(), CancellationToken::new()))
                .await
                .unwrap_err(),
        );
        assert_eq!(code.as_deref(), Some("continuation_scope_missing"));
        // The scoped form is refused too — the ledger holds no binding.
        h.submit(
            "t3",
            &scoped_payload(
                &native_target(NativeProfile::PlainChat, false),
                &input,
                None,
                Some(&scope("convo-x", "parent-9", 3)),
            ),
        )
        .await;
        let claimed = h.claim("t3").await;
        let (code, _) = Harness::failure(
            h.executor()
                .execute(h.context(&claimed, CancellationToken::new(), CancellationToken::new()))
                .await
                .unwrap_err(),
        );
        assert_eq!(code.as_deref(), Some("continuation_mismatch"));
        assert_eq!(
            h.shared.spawn_count.load(Ordering::SeqCst),
            1,
            "the phantom resume never spawned"
        );
    }

    fn scope(conversation: &str, parent: &str, position: i64) -> ContinuationScope {
        ContinuationScope {
            conversation: conversation.into(),
            parent: parent.into(),
            history_position: position,
            // The answer this turn produces becomes its own message at
            // the next position; the commit binds there (AGENTKIT-113).
            commit_parent: format!("{parent}:answer"),
            commit_history_position: position + 1,
            backend: BACKEND.into(),
            executable: "/usr/bin/claude".into(),
            config_identity: "cfg-digest".into(),
            system_prompt_revision: None,
        }
    }

    fn scoped_payload(
        target: &NativeExecutionTarget,
        input: &TurnInput,
        broker: Option<&BrokerSpec>,
        scope: Option<&ContinuationScope>,
    ) -> Value {
        let mut value = payload(target, input, broker);
        if let Some(scope) = scope {
            value[CONTINUATION_SCOPE_PAYLOAD_KEY] = serde_json::to_value(scope).unwrap();
        }
        value
    }

    // ----- review §2.3: scoped exact resume -------------------------------

    #[tokio::test]
    async fn completed_turn_commits_scoped_continuation_binding() {
        let h = harness();
        h.shared
            .queue(&[r#"{"e":"init","s":"vendor-1","m":"sonnet-x"}"#]);
        h.shared.set_outcome(Shared::completed("vendor-1", "hello"));
        h.submit(
            "t1",
            &scoped_payload(
                &native_target(NativeProfile::PlainChat, false),
                &fresh_input(),
                None,
                Some(&scope("convo-a", "parent-1", 1)),
            ),
        )
        .await;
        let claimed = h.claim("t1").await;
        h.executor()
            .execute(h.context(&claimed, CancellationToken::new(), CancellationToken::new()))
            .await
            .expect("turn completes");
        let binding = h
            .store
            .active_continuation(
                "convo-a",
                "parent-1:answer",
                2,
                BACKEND,
                "/usr/bin/claude",
                "cfg-digest",
                None,
            )
            .await
            .unwrap()
            .expect("completion commits the durable binding at the COMPLETED position");
        assert_eq!(binding.vendor_session_id, "vendor-1");
        assert_eq!(binding.owning_task_id, "t1");
        assert!(
            h.store
                .active_continuation(
                    "convo-a",
                    "parent-1",
                    1,
                    BACKEND,
                    "/usr/bin/claude",
                    "cfg-digest",
                    None
                )
                .await
                .unwrap()
                .is_none(),
            "the INPUT position must carry no binding — regenerating from it must miss"
        );
        assert!(
            h.event_types("t1")
                .await
                .iter()
                .any(|t| t == "native.continuation.bound"),
            "the binding event must be visible"
        );
    }

    #[tokio::test]
    async fn exact_resume_rejects_foreign_scope_binding() {
        let h = harness();
        // Establish vendor-1 under conversation A.
        h.shared
            .queue(&[r#"{"e":"init","s":"vendor-1","m":"sonnet-x"}"#]);
        h.shared.set_outcome(Shared::completed("vendor-1", "hello"));
        h.submit(
            "t1",
            &scoped_payload(
                &native_target(NativeProfile::PlainChat, false),
                &fresh_input(),
                None,
                Some(&scope("convo-a", "parent-1", 1)),
            ),
        )
        .await;
        let claimed = h.claim("t1").await;
        h.executor()
            .execute(h.context(&claimed, CancellationToken::new(), CancellationToken::new()))
            .await
            .unwrap();
        assert_eq!(h.shared.spawn_count.load(Ordering::SeqCst), 1);

        // Another conversation may NOT exact-resume the id even knowing it:
        // a vendor session is not global authority (review §2.3).
        let input = turn_input(TurnResume::Exact {
            vendor_session_id: "vendor-1".into(),
        });
        h.submit(
            "t2",
            &scoped_payload(
                &native_target(NativeProfile::PlainChat, false),
                &input,
                None,
                Some(&scope("convo-b", "parent-9", 7)),
            ),
        )
        .await;
        let claimed = h.claim("t2").await;
        let err = h
            .executor()
            .execute(h.context(&claimed, CancellationToken::new(), CancellationToken::new()))
            .await
            .unwrap_err();
        let (code, retryable) = Harness::failure(err);
        assert_eq!(code.as_deref(), Some("continuation_mismatch"));
        assert_eq!(retryable, Some(false));
        assert_eq!(
            h.shared.spawn_count.load(Ordering::SeqCst),
            1,
            "a refused resume must never reach the vendor"
        );
    }

    #[tokio::test]
    async fn exact_resume_follows_selected_history_position() {
        let h = harness();
        h.shared
            .queue(&[r#"{"e":"init","s":"vendor-1","m":"sonnet-x"}"#]);
        h.shared.set_outcome(Shared::completed("vendor-1", "hello"));
        let target = native_target(NativeProfile::PlainChat, false);
        h.submit(
            "t1",
            &scoped_payload(
                &target,
                &fresh_input(),
                None,
                Some(&scope("convo-a", "parent-1", 1)),
            ),
        )
        .await;
        let claimed = h.claim("t1").await;
        h.executor()
            .execute(h.context(&claimed, CancellationToken::new(), CancellationToken::new()))
            .await
            .unwrap();

        // The follow-up is submitted from the position the first answer
        // completed at — the binding committed there is the attestation.
        h.shared
            .queue(&[r#"{"e":"init","s":"vendor-1","m":"sonnet-x"}"#]);
        h.shared.set_outcome(Shared::completed("vendor-1", "again"));
        let input = turn_input(TurnResume::Exact {
            vendor_session_id: "vendor-1".into(),
        });
        h.submit(
            "t2",
            &scoped_payload(
                &target,
                &input,
                None,
                Some(&scope("convo-a", "parent-1:answer", 2)),
            ),
        )
        .await;
        let claimed = h.claim("t2").await;
        h.executor()
            .execute(h.context(&claimed, CancellationToken::new(), CancellationToken::new()))
            .await
            .expect("matching scope resumes exactly");
        assert_eq!(h.shared.spawn_count.load(Ordering::SeqCst), 2);
        assert!(
            h.event_types("t2")
                .await
                .iter()
                .any(|t| t == "native.completed")
        );

        // A rewind to an older selection must NOT inherit the future the
        // conversation already moved past: no active binding at the older
        // position means no exact continuation — fail closed.
        h.submit(
            "t3",
            &scoped_payload(
                &target,
                &input,
                None,
                Some(&scope("convo-a", "parent-0", 0)),
            ),
        )
        .await;
        let claimed = h.claim("t3").await;
        let err = h
            .executor()
            .execute(h.context(&claimed, CancellationToken::new(), CancellationToken::new()))
            .await
            .unwrap_err();
        let (code, _) = Harness::failure(err);
        assert_eq!(code.as_deref(), Some("continuation_mismatch"));
        assert_eq!(
            h.shared.spawn_count.load(Ordering::SeqCst),
            2,
            "a rewound selection must not resurrect the later session"
        );
    }

    #[tokio::test]
    async fn changed_configuration_refuses_exact_resume() {
        let h = harness();
        h.shared
            .queue(&[r#"{"e":"init","s":"vendor-1","m":"sonnet-x"}"#]);
        h.shared.set_outcome(Shared::completed("vendor-1", "hello"));
        let target = native_target(NativeProfile::PlainChat, false);
        h.submit(
            "t1",
            &scoped_payload(
                &target,
                &fresh_input(),
                None,
                Some(&scope("convo-a", "parent-1", 1)),
            ),
        )
        .await;
        let claimed = h.claim("t1").await;
        h.executor()
            .execute(h.context(&claimed, CancellationToken::new(), CancellationToken::new()))
            .await
            .unwrap();

        // The binding EXISTS at the position the first answer completed
        // at; only the changed config identity differs. The store's exact
        // lookup must filter it out on config identity, and a changed
        // prompt revision must break the match just as decisively.
        let mut reconfigured = target.clone();
        reconfigured.config_identity = "cfg-changed".into();
        reconfigured.system_prompt_revision = Some("prompt-v2".into());
        let input = turn_input(TurnResume::Exact {
            vendor_session_id: "vendor-1".into(),
        });
        h.submit(
            "t2",
            &scoped_payload(
                &reconfigured,
                &input,
                None,
                Some(&ContinuationScope {
                    config_identity: "cfg-changed".into(),
                    system_prompt_revision: Some("prompt-v2".into()),
                    ..scope("convo-a", "parent-1:answer", 2)
                }),
            ),
        )
        .await;
        let claimed = h.claim("t2").await;
        let err = h
            .executor()
            .execute(h.context(&claimed, CancellationToken::new(), CancellationToken::new()))
            .await
            .unwrap_err();
        let (code, _) = Harness::failure(err);
        assert_eq!(code.as_deref(), Some("continuation_mismatch"));
    }

    // ----- review §2.5: lifetime under cancellation -----------------------

    #[tokio::test]
    async fn aborted_execution_revokes_and_shuts_down_the_broker() {
        let h = harness();
        // A child that never exits: the future stays in the stream loop
        // after the broker has started, exactly like a hung vendor CLI.
        h.shared.hang.store(true, Ordering::SeqCst);
        h.submit(
            "t1",
            &payload(
                &native_target(NativeProfile::OwnedMcpChat, false),
                &fresh_input(),
                Some(&broker_spec()),
            ),
        )
        .await;
        let claimed = h.claim("t1").await;
        let executor = h.executor();
        let context = h.context(&claimed, CancellationToken::new(), CancellationToken::new());
        let handle = tokio::spawn(async move { executor.execute(context).await });
        let broker = loop {
            if let Some(broker) = h.factory.started.lock().unwrap().last().cloned() {
                break broker;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        };
        // Whole-future cancellation — no explicit cleanup ran anywhere.
        handle.abort();
        let _ = handle.await;

        // Every ingress must refuse on its own (owner Drop revoked the
        // authority — no explicit shutdown ran anywhere). The cleanup is
        // detached, so give it a bounded window before judging.
        let scope = SessionScope {
            principal: json!({"attempt": "aborted"}),
            chat_id: None,
            actor_id: "vendor-1".into(),
            request_id: None,
            cancellation: CancellationToken::new(),
        };
        let mut refusal = None;
        for _ in 0..400 {
            let tools = broker.backend().list_tools(&scope).await.err();
            // A revoked broker refuses a call as an isError result (the
            // MCP call channel reports refusals in-band, not as Err).
            let call = match broker
                .backend()
                .call_tool("note_search", Default::default(), &scope)
                .await
            {
                Ok(result) if result.is_error == Some(true) => Some(format!("{result:?}")),
                Ok(_result) => {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                    continue;
                }
                Err(error) => Some(format!("{error:?}")),
            };
            if let Some(tools_err) = tools {
                let quarantined = h
                    .shared
                    .interrupts
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|policy| matches!(policy, InterruptPolicy::Quarantine));
                if quarantined {
                    refusal = Some((format!("{tools_err:?}"), call.unwrap_or_default()));
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let refusal = match refusal {
            Some(refusal) => refusal,
            None => {
                let tools_dbg = broker.backend().list_tools(&scope).await.is_err();
                let call_dbg = broker
                    .backend()
                    .call_tool("note_search", Default::default(), &scope)
                    .await
                    .is_err();
                let interrupts = format!("{:?}", *h.shared.interrupts.lock().unwrap());
                let live = h.shared.child_live.load(Ordering::SeqCst);
                panic!(
                    "broker serving or child un-quarantined: tools_err={tools_dbg} call_err={call_dbg} interrupts={interrupts} child_live={live}"
                )
            }
        };
        let (tools_text, call_text) = refusal;
        let text = format!("{tools_text} {call_text}");
        assert!(
            text.contains("revoked") || text.contains("not_live") || text.contains("denied"),
            "expected an explicit revocation refusal, got {text}"
        );
    }

    /// AGENTKIT-110 lifecycle hardening: the attempt destructor is scoped
    /// to the attempt's OWN process generation. Dropping attempt A must
    /// never quarantine-interrupt a child belonging to a concurrent
    /// attempt B on the shared runtime supervisor; dropping the owner that
    /// DID attach a child must terminate exactly that child.
    #[tokio::test]
    async fn dropping_an_attempt_owner_never_interrupts_a_sibling_child() {
        let shared = Shared::default();
        let sup: Arc<dyn ProcessSupervisor> = Arc::new(FakeSupervisor {
            shared: shared.clone(),
        });
        let plan = LaunchPlan {
            argv: vec!["claude".into()],
            env: vec![],
            cwd: "/tmp".into(),
            per_run_dir: "/tmp/run".into(),
            permission_policy: PermissionPromptPolicy::None,
            max_vendor_turns: None,
            native_session_id: None,
            fingerprint: "fp-scope".into(),
        };
        let mut child = sup
            .spawn(&plan, &ResourceLimits::default(), &CancellationToken::new())
            .await
            .expect("spawn sibling child");
        assert!(shared.child_live.load(Ordering::SeqCst));

        // Attempt A owns no process: dropping it must leave B untouched.
        let owner_a = AttemptOwner::new(
            SqliteStore::memory(64).unwrap(),
            "attempt-a",
            sup.clone(),
            None,
            None,
        );
        drop(owner_a);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            shared.interrupts.lock().unwrap().is_empty(),
            "dropping an owner with no attached process must not interrupt the sibling"
        );
        assert!(
            shared.child_live.load(Ordering::SeqCst),
            "the sibling child must survive another attempt's drop"
        );

        // Attempt B owns the live child: its drop terminates exactly it.
        let owner_b = AttemptOwner::new(
            SqliteStore::memory(64).unwrap(),
            "attempt-b",
            sup.clone(),
            None,
            None,
        );
        owner_b.attach_child(child.pid(), child.generation());
        drop(owner_b);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while shared.child_live.load(Ordering::SeqCst) {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the owner that attached the child must terminate its own process"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(
            &*shared.interrupts.lock().unwrap(),
            &[InterruptPolicy::Quarantine],
            "exactly one quarantine interrupt — the attached generation only"
        );
        let _ = child.wait().await;
    }

    #[tokio::test]
    async fn post_spawn_persistence_failure_reaps_the_child() {
        let h = harness();
        h.shared
            .queue(&[r#"{"e":"init","s":"vendor-1","m":"sonnet-x"}"#]);
        h.shared.set_outcome(Shared::completed("vendor-1", "hello"));
        h.submit(
            "t1",
            &payload(
                &native_target(NativeProfile::PlainChat, false),
                &fresh_input(),
                None,
            ),
        )
        .await;
        let claimed = h.claim("t1").await;
        // Surrender the lease before execution starts: the first event the
        // executor appends AFTER spawning is rejected at the fence — the
        // review §2.5 `?`-leak shape.
        h.store
            .park_interrupted(claimed.lease.fence(), h.runtime.now_ms())
            .await
            .unwrap();
        let err = h
            .executor()
            .execute(h.context(&claimed, CancellationToken::new(), CancellationToken::new()))
            .await
            .unwrap_err();
        assert!(
            matches!(err, HostError::Store(_)),
            "expected the fenced append failure to surface, got {err:?}"
        );
        assert_eq!(h.shared.spawn_count.load(Ordering::SeqCst), 1);
        assert!(
            h.shared
                .interrupts
                .lock()
                .unwrap()
                .iter()
                .any(|policy| matches!(policy, InterruptPolicy::Quarantine)),
            "a persistence failure after spawn must quarantine-interrupt and reap the child"
        );
        assert!(
            h.shared
                .interrupts
                .lock()
                .unwrap()
                .iter()
                .any(|policy| matches!(policy, InterruptPolicy::HostShutdown)
                    || matches!(policy, InterruptPolicy::Quarantine))
                && !h
                    .event_types("t1")
                    .await
                    .iter()
                    .any(|t| t == "native.completed"),
            "no completion may be claimed for the failed attempt"
        );
    }

    #[tokio::test]
    async fn preflight_refusal_never_spawns() {
        let h = harness();
        *h.shared.preflight_refusal.lock().unwrap() =
            Some(NativeRefusal::new("not_signed_in", "sign in first", true));
        h.submit(
            "t1",
            &payload(
                &native_target(NativeProfile::PlainChat, false),
                &fresh_input(),
                None,
            ),
        )
        .await;
        let claimed = h.claim("t1").await;
        let (code, retryable) = Harness::failure(
            h.executor()
                .execute(h.context(&claimed, CancellationToken::new(), CancellationToken::new()))
                .await
                .unwrap_err(),
        );
        assert_eq!(code.as_deref(), Some("not_signed_in"));
        assert_eq!(retryable, Some(true), "pre-spawn refusal is replay-safe");
        assert_eq!(h.shared.spawn_count.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn host_cancellation_interrupts_child_and_reports_cancelled() {
        let h = harness();
        h.shared.hang.store(true, Ordering::SeqCst);
        h.shared
            .set_outcome(NativeTerminalOutcome::Interrupted { partial_text: None });
        h.submit(
            "t1",
            &payload(
                &native_target(NativeProfile::PlainChat, false),
                &fresh_input(),
                None,
            ),
        )
        .await;
        let claimed = h.claim("t1").await;
        let cancellation = CancellationToken::new();
        let context = h.context(&claimed, cancellation.clone(), CancellationToken::new());
        cancellation.cancel();
        let err = h.executor().execute(context).await.unwrap_err();
        assert!(matches!(err, HostError::Cancelled), "{err:?}");
        assert_eq!(
            *h.shared.interrupts.lock().unwrap(),
            [InterruptPolicy::HostShutdown]
        );
    }

    #[tokio::test]
    async fn durable_interruption_is_terminal_not_cancelled() {
        let h = harness();
        h.shared.hang.store(true, Ordering::SeqCst);
        h.shared
            .set_outcome(NativeTerminalOutcome::Interrupted { partial_text: None });
        h.submit(
            "t1",
            &payload(
                &native_target(NativeProfile::PlainChat, false),
                &fresh_input(),
                None,
            ),
        )
        .await;
        let claimed = h.claim("t1").await;
        let interruption = CancellationToken::new();
        let context = h.context(&claimed, CancellationToken::new(), interruption.clone());
        interruption.cancel();
        let (code, retryable) = Harness::failure(h.executor().execute(context).await.unwrap_err());
        assert_eq!(code.as_deref(), Some("native_interrupted"));
        assert_eq!(
            retryable,
            Some(false),
            "a stopped turn is never auto-retried"
        );
        assert_eq!(
            *h.shared.interrupts.lock().unwrap(),
            [InterruptPolicy::UserStop]
        );
    }

    #[tokio::test]
    async fn protocol_violation_quarantines_the_child() {
        let h = harness();
        h.shared.queue(&["VIOLATION junk"]);
        h.submit(
            "t1",
            &payload(
                &native_target(NativeProfile::PlainChat, false),
                &fresh_input(),
                None,
            ),
        )
        .await;
        let claimed = h.claim("t1").await;
        let (code, retryable) = Harness::failure(
            h.executor()
                .execute(h.context(&claimed, CancellationToken::new(), CancellationToken::new()))
                .await
                .unwrap_err(),
        );
        assert_eq!(code.as_deref(), Some("native_protocol_violation"));
        assert_eq!(retryable, Some(false));
        assert_eq!(
            *h.shared.interrupts.lock().unwrap(),
            [InterruptPolicy::Quarantine]
        );
    }

    #[tokio::test]
    async fn idle_timeout_stops_silent_child() {
        let h = harness();
        // One event, then silence: the idle bound must fire. Pop-first
        // guarantees the init frame is consumed before the hang engages.
        h.shared.queue(&[r#"{"e":"init","s":"vendor-1"}"#]);
        h.shared.hang.store(true, Ordering::SeqCst);
        h.submit(
            "t1",
            &payload(
                &native_target(NativeProfile::PlainChat, false),
                &fresh_input(),
                None,
            ),
        )
        .await;
        let claimed = h.claim("t1").await;
        let (code, retryable) = Harness::failure(
            h.executor()
                .execute(h.context(&claimed, CancellationToken::new(), CancellationToken::new()))
                .await
                .unwrap_err(),
        );
        assert_eq!(code.as_deref(), Some("native_idle_timeout"));
        assert_eq!(
            retryable,
            Some(false),
            "an idle child may have acted; no blind retry"
        );
        assert!(!h.shared.interrupts.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn init_identity_drift_fails_closed() {
        let h = harness();
        *h.shared.preassigned.lock().unwrap() = Some("host-1".into());
        h.shared.queue(&[r#"{"e":"init","s":"vendor-2"}"#]);
        h.shared.set_outcome(Shared::completed("vendor-2", "late"));
        h.submit(
            "t1",
            &payload(
                &native_target(NativeProfile::PlainChat, false),
                &fresh_input(),
                None,
            ),
        )
        .await;
        let claimed = h.claim("t1").await;
        let (code, _) = Harness::failure(
            h.executor()
                .execute(h.context(&claimed, CancellationToken::new(), CancellationToken::new()))
                .await
                .unwrap_err(),
        );
        assert_eq!(code.as_deref(), Some("session_identity_mismatch"));
        assert_eq!(
            *h.shared.interrupts.lock().unwrap(),
            [InterruptPolicy::Quarantine]
        );
    }

    #[tokio::test]
    async fn owned_mcp_chat_wires_broker_then_revokes() {
        let h = harness();
        h.shared
            .set_outcome(Shared::completed("vendor-1", "tool answer"));
        h.shared
            .queue(&[r#"{"e":"init","s":"vendor-1","mcp":{"onemind":"connected"}}"#]);
        h.submit(
            "t1",
            &payload(
                &native_target(NativeProfile::OwnedMcpChat, false),
                &fresh_input(),
                Some(&broker_spec()),
            ),
        )
        .await;
        let claimed = h.claim("t1").await;
        h.executor()
            .execute(h.context(&claimed, CancellationToken::new(), CancellationToken::new()))
            .await
            .expect("governed turn completes");
        // Exactly one broker for this attempt, spec handed to the driver.
        assert_eq!(h.factory.contexts.lock().unwrap().len(), 1);
        let recorded = h.factory.contexts.lock().unwrap()[0].clone();
        let (task_id, attempt_id, server, tools) = &recorded;
        assert_eq!(task_id, "t1");
        assert_eq!(attempt_id, &claimed.lease.attempt_id);
        assert_eq!(server, "onemind");
        assert_eq!(*tools, 1);
        let seen = h.shared.seen_mcp.lock().unwrap().clone().unwrap();
        assert!(seen.is_some_and(|spec| spec.http_url.is_some() && spec.auth_token.is_some()));
        // Settlement revoked the endpoint immediately.
        let broker = h.factory.started.lock().unwrap().remove(0);
        let scope = agentkit_mcp::SessionScope {
            principal: json!({"attempt": claimed.lease.attempt_id}),
            chat_id: None,
            actor_id: "vendor-1".into(),
            request_id: None,
            cancellation: CancellationToken::new(),
        };
        assert!(
            broker.backend().list_tools(&scope).await.is_err(),
            "settled attempts must not keep authenticating"
        );
    }

    // ----- Fix B: fail closed when the owned MCP server is not connected ---

    /// Runs one OwnedMcpChat turn whose scripted init frame is `init_line`.
    async fn run_owned_mcp(h: &Harness, init_line: &str) -> HostResult<()> {
        h.shared.queue(&[init_line]);
        h.submit(
            "t1",
            &payload(
                &native_target(NativeProfile::OwnedMcpChat, false),
                &fresh_input(),
                Some(&broker_spec()),
            ),
        )
        .await;
        let claimed = h.claim("t1").await;
        h.executor()
            .execute(h.context(&claimed, CancellationToken::new(), CancellationToken::new()))
            .await
    }

    async fn assert_bridge_failure(h: &Harness, err: HostError, reported: &str) {
        let message = Harness::failure_message(&err);
        let (code, _) = Harness::failure(err);
        assert_eq!(code.as_deref(), Some("native_mcp_bridge_unavailable"));
        assert!(
            message.contains("onemind") && message.contains(reported),
            "{message}"
        );
        let types = h.event_types("t1").await;
        assert!(
            types.iter().any(|t| t == "native.event"),
            "evidence is durable: {types:?}"
        );
        assert!(types.iter().any(|t| t == "native.failed"), "{types:?}");
        assert!(!types.iter().any(|t| t == "native.completed"), "{types:?}");
        let failed = h.event_data("t1", "native.failed").await.unwrap();
        assert_eq!(failed["class"], "mcp_bridge_unavailable");
        assert_eq!(
            *h.shared.interrupts.lock().unwrap(),
            [InterruptPolicy::Quarantine]
        );
    }

    #[tokio::test]
    async fn owned_mcp_fails_closed_when_the_server_reports_failed() {
        let h = harness();
        // Even a scripted `Completed` must not win over the bridge failure.
        h.shared
            .set_outcome(Shared::completed("vendor-1", "fabricated"));
        let err = run_owned_mcp(
            &h,
            r#"{"e":"init","s":"vendor-1","mcp":{"onemind":"failed"}}"#,
        )
        .await
        .unwrap_err();
        assert_bridge_failure(&h, err, "failed").await;
    }

    #[tokio::test]
    async fn owned_mcp_fails_closed_when_the_server_is_not_reported() {
        let h = harness();
        h.shared
            .set_outcome(Shared::completed("vendor-1", "fabricated"));
        let err = run_owned_mcp(
            &h,
            r#"{"e":"init","s":"vendor-1","mcp":{"someone-else":"connected"}}"#,
        )
        .await
        .unwrap_err();
        assert_bridge_failure(&h, err, "not reported").await;
    }

    #[tokio::test]
    async fn owned_mcp_completes_when_the_server_is_connected() {
        let h = harness();
        h.shared
            .set_outcome(Shared::completed("vendor-1", "real answer"));
        run_owned_mcp(
            &h,
            r#"{"e":"init","s":"vendor-1","mcp":{"onemind":"connected"}}"#,
        )
        .await
        .expect("a connected bridge completes as before");
        let types = h.event_types("t1").await;
        assert!(types.iter().any(|t| t == "native.completed"), "{types:?}");
        assert!(h.shared.interrupts.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn plain_chat_without_mcp_servers_completes_as_before() {
        let h = harness();
        h.shared.queue(&[r#"{"e":"init","s":"vendor-1"}"#]);
        h.shared.set_outcome(Shared::completed("vendor-1", "plain"));
        h.submit(
            "t1",
            &payload(
                &native_target(NativeProfile::PlainChat, false),
                &fresh_input(),
                None,
            ),
        )
        .await;
        let claimed = h.claim("t1").await;
        h.executor()
            .execute(h.context(&claimed, CancellationToken::new(), CancellationToken::new()))
            .await
            .expect("plain chat is unaffected by the MCP gate");
        assert!(h.shared.interrupts.lock().unwrap().is_empty());
    }

    // ----- Fix C: stderr surfaces in exit-derived failures ------------------

    const INVALID_SESSION: &str = "Error: Invalid session ID. Must be a valid UUID.";

    #[tokio::test]
    async fn early_exit_failure_carries_the_stderr_tail() {
        let h = harness();
        h.shared.exit_code.store(1, Ordering::SeqCst);
        *h.shared.stderr.lock().unwrap() =
            format!("\x1b[31m{INVALID_SESSION}\x1b[0m\n").into_bytes();
        h.shared.set_outcome(NativeTerminalOutcome::Failed {
            class: FailureClass::ExitWithoutResult,
            message: "vendor process exited without a root result (exit_code=Some(1), signal=None)"
                .into(),
            partial_text: None,
        });
        h.submit(
            "t1",
            &payload(
                &native_target(NativeProfile::PlainChat, false),
                &fresh_input(),
                None,
            ),
        )
        .await;
        let claimed = h.claim("t1").await;
        let err = h
            .executor()
            .execute(h.context(&claimed, CancellationToken::new(), CancellationToken::new()))
            .await
            .unwrap_err();
        let message = Harness::failure_message(&err);
        assert!(message.contains("exit_code=Some(1)"), "{message}");
        assert!(
            message.contains(&format!("; stderr: [31m{INVALID_SESSION}[0m")),
            "{message}"
        );
        let failed = h.event_data("t1", "native.failed").await.unwrap();
        let tail = failed["stderrTail"].as_str().unwrap();
        assert!(
            tail.contains(INVALID_SESSION) && !tail.contains('\u{1b}'),
            "{tail}"
        );
    }

    #[tokio::test]
    async fn uncertain_exit_carries_the_stderr_tail_and_completed_does_not() {
        let h = harness();
        *h.shared.stderr.lock().unwrap() = INVALID_SESSION.as_bytes().to_vec();
        // No scripted outcome: the fake adapter reports Uncertain/ExitWithoutResult.
        h.submit(
            "t1",
            &payload(
                &native_target(NativeProfile::PlainChat, false),
                &fresh_input(),
                None,
            ),
        )
        .await;
        let claimed = h.claim("t1").await;
        let err = h
            .executor()
            .execute(h.context(&claimed, CancellationToken::new(), CancellationToken::new()))
            .await
            .unwrap_err();
        assert!(Harness::failure_message(&err).ends_with(&format!("; stderr: {INVALID_SESSION}")));
        let uncertain = h.event_data("t1", "native.uncertain").await.unwrap();
        assert_eq!(uncertain["stderrTail"], INVALID_SESSION);
        assert!(
            uncertain["detail"]
                .as_str()
                .unwrap()
                .contains(INVALID_SESSION)
        );

        // A completed turn never gets the tail, even if stderr is noisy.
        let h = harness();
        *h.shared.stderr.lock().unwrap() = b"warning noise".to_vec();
        h.shared.queue(&[r#"{"e":"init","s":"vendor-1"}"#]);
        h.shared.set_outcome(Shared::completed("vendor-1", "ok"));
        h.submit(
            "t1",
            &payload(
                &native_target(NativeProfile::PlainChat, false),
                &fresh_input(),
                None,
            ),
        )
        .await;
        let claimed = h.claim("t1").await;
        h.executor()
            .execute(h.context(&claimed, CancellationToken::new(), CancellationToken::new()))
            .await
            .unwrap();
        let completed = h.event_data("t1", "native.completed").await.unwrap();
        assert!(completed.get("stderrTail").is_none());
    }

    #[tokio::test]
    async fn plain_chat_with_broker_payload_is_refused() {
        let h = harness();
        h.submit(
            "t1",
            &payload(
                &native_target(NativeProfile::PlainChat, false),
                &fresh_input(),
                Some(&broker_spec()),
            ),
        )
        .await;
        let claimed = h.claim("t1").await;
        let (code, _) = Harness::failure(
            h.executor()
                .execute(h.context(&claimed, CancellationToken::new(), CancellationToken::new()))
                .await
                .unwrap_err(),
        );
        assert_eq!(code.as_deref(), Some("broker_not_allowed"));
    }

    struct CountingExecutor {
        hits: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl TaskExecutor<SqliteStore> for CountingExecutor {
        async fn execute(&self, _context: ExecutionContext) -> HostResult<()> {
            self.hits.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    #[tokio::test]
    async fn dispatch_executor_routes_by_frozen_target() {
        let h = harness();
        let native_hits = Arc::new(AtomicUsize::new(0));
        let fallback_hits = Arc::new(AtomicUsize::new(0));
        let dispatcher = NativeDispatchExecutor {
            native: Arc::new(CountingExecutor {
                hits: native_hits.clone(),
            }),
            fallback: Arc::new(CountingExecutor {
                hits: fallback_hits.clone(),
            }),
        };
        let cases: Vec<(Value, usize, usize)> = vec![
            (
                payload(
                    &native_target(NativeProfile::PlainChat, false),
                    &fresh_input(),
                    None,
                ),
                1,
                0,
            ),
            (json!({"prompt": "legacy chat task"}), 0, 1),
            (
                json!({
                    EXECUTION_TARGET_PAYLOAD_KEY: {
                        "modelLoop": {
                            "schemaVersion": 1,
                            "providerId": "p",
                            "model": "m"
                        }
                    }
                }),
                0,
                1,
            ),
        ];
        for (index, (value, native_expected, fallback_expected)) in cases.into_iter().enumerate() {
            let task_id = format!("d{index}");
            h.submit(&task_id, &value).await;
            let claimed = h.claim(&task_id).await;
            let before_native = native_hits.load(Ordering::SeqCst);
            let before_fallback = fallback_hits.load(Ordering::SeqCst);
            dispatcher
                .execute(h.context(&claimed, CancellationToken::new(), CancellationToken::new()))
                .await
                .unwrap();
            assert_eq!(
                native_hits.load(Ordering::SeqCst) - before_native,
                native_expected,
                "native route count for case {index}"
            );
            assert_eq!(
                fallback_hits.load(Ordering::SeqCst) - before_fallback,
                fallback_expected,
                "fallback route count for case {index}"
            );
        }

        // Reviewer F5: a frozen-but-malformed native target is terminal at
        // the dispatcher - it must never be silently executed (and billed)
        // by the model-loop fallback.
        let malformed = json!({
            EXECUTION_TARGET_PAYLOAD_KEY: {"native": {"schemaVersion": 1}},
        });
        h.submit("d-bad", &malformed).await;
        let claimed = h.claim("d-bad").await;
        let err = dispatcher
            .execute(h.context(&claimed, CancellationToken::new(), CancellationToken::new()))
            .await
            .unwrap_err();
        let (code, retryable) = Harness::failure(err);
        assert_eq!(code.as_deref(), Some("execution_target_invalid"));
        assert_eq!(retryable, Some(false));
        assert_eq!(native_hits.load(Ordering::SeqCst), 1);
        assert_eq!(
            fallback_hits.load(Ordering::SeqCst),
            2,
            "the malformed native target never reached the fallback"
        );
    }

    struct CapExecutor {
        verification: bool,
        snapshots: bool,
    }

    #[async_trait]
    impl TaskExecutor<SqliteStore> for CapExecutor {
        fn handles_verification(&self) -> bool {
            self.verification
        }
        fn resolves_provider_snapshots(&self) -> bool {
            self.snapshots
        }
        async fn execute(&self, _context: ExecutionContext) -> HostResult<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn dispatch_executor_preserves_declared_capabilities() {
        // Review §3.4: wrapping must not reclassify a turn. Each route's
        // declared verification / snapshot capabilities survive the wrapper.
        let dispatcher = NativeDispatchExecutor {
            native: Arc::new(CapExecutor {
                verification: true,
                snapshots: false,
            }),
            fallback: Arc::new(CapExecutor {
                verification: false,
                snapshots: true,
            }),
        };
        let native_payload = payload(
            &native_target(NativeProfile::PlainChat, false),
            &fresh_input(),
            None,
        );
        let model_payload = json!({
            EXECUTION_TARGET_PAYLOAD_KEY: {
                "modelLoop": { "schemaVersion": 1, "providerId": "p", "model": "m" }
            }
        });
        let legacy = json!({"prompt": "legacy chat task"});

        assert!(
            dispatcher.handles_verification_for(&native_payload),
            "the native route keeps its self-attested verification"
        );
        assert!(
            !dispatcher.handles_verification_for(&model_payload),
            "the model-loop route keeps its hook-verified behaviour"
        );
        assert!(!dispatcher.handles_verification_for(&legacy));
        assert!(
            !dispatcher.resolves_provider_snapshots_for(&native_payload),
            "a native turn resolves no provider snapshot"
        );
        assert!(
            dispatcher.resolves_provider_snapshots_for(&model_payload),
            "the model-loop route keeps dynamic snapshot resolution"
        );
        assert!(dispatcher.resolves_provider_snapshots_for(&legacy));

        // The kind-wide host view follows the model-loop lane — that is
        // the behaviour outside callers (HTTP admission) condition on.
        assert!(!dispatcher.handles_verification());
        assert!(dispatcher.resolves_provider_snapshots());
    }

    // ----- review round-4 F3: claim-scoped continuation ownership -----

    /// Publish one ACTIVE binding at the INPUT position
    /// (`conversation`, `parent`, `position`) of a follow-up turn: a
    /// fresh scoped turn submitted from just before it completes and
    /// commits there, then settles.
    async fn publish_input_position(
        h: &Harness,
        conversation: &str,
        parent: &str,
        position: i64,
        session: &str,
    ) -> String {
        let source_parent = format!("{parent}:before");
        let seeding = ContinuationScope {
            commit_parent: parent.into(),
            commit_history_position: position,
            ..scope(conversation, &source_parent, position - 1)
        };
        h.shared
            .queue(&[format!(r#"{{"e":"init","s":"{session}","m":"sonnet-x"}}"#).as_str()]);
        h.shared.set_outcome(Shared::completed(session, "seeded"));
        h.submit(
            "t-seed",
            &scoped_payload(
                &native_target(NativeProfile::PlainChat, false),
                &fresh_input(),
                None,
                Some(&seeding),
            ),
        )
        .await;
        let claimed = h.claim("t-seed").await;
        h.executor()
            .execute(h.context(&claimed, CancellationToken::new(), CancellationToken::new()))
            .await
            .expect("seeding turn completes");
        h.store
            .settle(agentkit_store::SettleTask {
                fence: claimed.lease.fence(),
                now: h.runtime.now_ms(),
                status: agentkit_store::TaskStatus::Completed,
                error: None,
                events: vec![],
                messages: vec![],
            })
            .await
            .unwrap();
        claimed.lease.attempt_id
    }

    #[tokio::test]
    async fn stale_selection_during_preflight_never_spawns_or_claims_replacement() {
        let h = harness();
        publish_input_position(&h, "stale-selection", "msg-1", 1, "vendor-x").await;
        h.shared.preflight_entered.store(false, Ordering::SeqCst);
        h.shared.preflight_hang.store(true, Ordering::SeqCst);
        h.submit(
            "stale",
            &scoped_payload(
                &native_target(NativeProfile::PlainChat, false),
                &turn_input(TurnResume::Exact {
                    vendor_session_id: "vendor-x".into(),
                }),
                None,
                Some(&scope("stale-selection", "msg-1", 1)),
            ),
        )
        .await;
        let claimed = h.claim("stale").await;
        let executor = h.executor();
        let context = h.context(&claimed, CancellationToken::new(), CancellationToken::new());
        let handle = tokio::spawn(async move { executor.execute(context).await });
        tokio::time::timeout(Duration::from_secs(2), async {
            while !h.shared.preflight_entered.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("selection reached preflight");
        h.submit("replacement", &json!({})).await;
        let replacement = h.claim("replacement").await;
        let y = h
            .store
            .commit_continuation(CommitContinuation {
                conversation: "stale-selection".into(),
                parent: "msg-1".into(),
                history_position: 1,
                backend: BACKEND.into(),
                executable: "/usr/bin/claude".into(),
                config_identity: "cfg-digest".into(),
                system_prompt_revision: None,
                vendor_session_id: "vendor-y".into(),
                owning_attempt_id: replacement.lease.attempt_id.clone(),
                owning_task_id: "replacement".into(),
                committed_at: h.runtime.now_ms(),
            })
            .await
            .unwrap();
        h.shared.preflight_hang.store(false, Ordering::SeqCst);
        let (code, _) = Harness::failure(handle.await.unwrap().unwrap_err());
        assert_eq!(code.as_deref(), Some("continuation_mismatch"));
        assert_eq!(
            h.shared.spawn_count.load(Ordering::SeqCst),
            1,
            "only seeding turn spawned"
        );
        let current = h
            .store
            .continuation_for_attempt(&replacement.lease.attempt_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(current.binding_id, y.binding_id);
        assert_eq!(current.state, "active", "replacement remains unclaimed");
    }

    #[tokio::test]
    async fn abort_during_pending_delivery_keeps_continuation_uncertain() {
        let h = harness();
        let seeding =
            publish_input_position(&h, "pending-delivery", "msg-1", 1, "vendor-pending").await;
        h.shared.delivered.lock().unwrap().clear();
        h.shared.delivery_hang.store(true, Ordering::SeqCst);
        h.submit(
            "pending",
            &scoped_payload(
                &native_target(NativeProfile::PlainChat, false),
                &turn_input(TurnResume::Exact {
                    vendor_session_id: "vendor-pending".into(),
                }),
                None,
                Some(&scope("pending-delivery", "msg-1", 1)),
            ),
        )
        .await;
        let claimed = h.claim("pending").await;
        let executor = h.executor();
        let context = h.context(&claimed, CancellationToken::new(), CancellationToken::new());
        let handle = tokio::spawn(async move { executor.execute(context).await });
        tokio::time::timeout(Duration::from_secs(2), async {
            while h.shared.delivered.lock().unwrap().is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("delivery entered");
        handle.abort();
        assert!(handle.await.unwrap_err().is_cancelled());
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let row = h
                    .store
                    .continuation_for_attempt(&seeding)
                    .await
                    .unwrap()
                    .unwrap();
                if row.state == "uncertain" {
                    break;
                }
                assert_ne!(
                    row.state, "active",
                    "pending delivery must never release consumed input"
                );
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("aborted attempt settled uncertain");
    }

    #[tokio::test]
    async fn interrupted_after_input_attempt_cannot_exact_resume_preclaim_position() {
        // Review round-4 F3-A reproduced: complete a turn recording its
        // vendor session, resume that session for a follow-up, deliver
        // the prompt, receive PARTIAL output, then interrupt. The input
        // position the interrupted attempt claimed must never present as
        // available for an exact resume again — the vendor session
        // already consumed the prompt and part of the answer.
        let h = harness();
        let seeding_attempt =
            publish_input_position(&h, "convo-f3a", "msg-1", 1, "vendor-f3a").await;
        assert!(
            h.store
                .active_continuation(
                    "convo-f3a",
                    "msg-1",
                    1,
                    BACKEND,
                    "/usr/bin/claude",
                    "cfg-digest",
                    None
                )
                .await
                .unwrap()
                .is_some(),
            "the input position is resumable before the claim"
        );

        let input = turn_input(TurnResume::Exact {
            vendor_session_id: "vendor-f3a".into(),
        });
        h.submit(
            "t1",
            &scoped_payload(
                &native_target(NativeProfile::PlainChat, false),
                &input,
                None,
                Some(&scope("convo-f3a", "msg-1", 1)),
            ),
        )
        .await;
        let claimed = h.claim("t1").await;
        // Partial output, no root result, then a durable user stop.
        h.shared.queue(&[r#"{"e":"delta","t":"half"}"#]);
        h.shared.hang.store(true, Ordering::SeqCst);
        h.shared.set_outcome(NativeTerminalOutcome::Interrupted {
            partial_text: Some("half".into()),
        });
        let interruption = CancellationToken::new();
        interruption.cancel();
        let (code, _) = Harness::failure(
            h.executor()
                .execute(h.context(&claimed, CancellationToken::new(), interruption))
                .await
                .unwrap_err(),
        );
        assert_eq!(code.as_deref(), Some("native_interrupted"));
        assert!(
            !h.shared.delivered.lock().unwrap().is_empty(),
            "the input WAS delivered"
        );

        // The claimed position is now `uncertain`: never available, and
        // the committing attempt's audit row says so honestly.
        assert!(
            h.store
                .active_continuation(
                    "convo-f3a",
                    "msg-1",
                    1,
                    BACKEND,
                    "/usr/bin/claude",
                    "cfg-digest",
                    None
                )
                .await
                .unwrap()
                .is_none(),
            "an interrupted attempt's pre-claim position is not available"
        );
        assert!(
            h.store
                .resumable_continuation(
                    "convo-f3a",
                    "msg-1",
                    1,
                    BACKEND,
                    "/usr/bin/claude",
                    "cfg-digest",
                    None,
                    &claimed.lease.attempt_id
                )
                .await
                .unwrap()
                .is_none(),
            "not even the claimant may exact-resume it"
        );
        let row = h
            .store
            .continuation_for_attempt(&seeding_attempt)
            .await
            .unwrap()
            .expect("row kept for audit");
        assert_eq!(
            row.state, "uncertain",
            "interruption after input is uncertain"
        );

        // A follow-up asking for the exact resume is refused from the
        // ledger — no second spawn from the consumed session.
        h.submit(
            "t2",
            &scoped_payload(
                &native_target(NativeProfile::PlainChat, false),
                &input,
                None,
                Some(&scope("convo-f3a", "msg-1", 1)),
            ),
        )
        .await;
        let claimed2 = h.claim("t2").await;
        let (code, _) = Harness::failure(
            h.executor()
                .execute(h.context(
                    &claimed2,
                    CancellationToken::new(),
                    CancellationToken::new(),
                ))
                .await
                .unwrap_err(),
        );
        assert_eq!(code.as_deref(), Some("continuation_mismatch"));
        assert_eq!(
            h.shared.spawn_count.load(Ordering::SeqCst),
            2,
            "only the seeding turn and the interrupted attempt ever spawned"
        );
    }

    #[tokio::test]
    async fn retired_continuations_refuse_the_same_task_resume() {
        // Review round-4 F3-B reproduced (OneMind Resume, engine.resume
        // on the SAME task): the task's own observed session proves
        // identity only. Once the conversation's continuations are
        // retired, the same-task resume path must NOT respawn from the
        // stale vendor session — the ledger is the authority.
        let h = harness();
        publish_input_position(&h, "convo-f3b", "msg-1", 1, "vendor-f3b").await;
        let input = turn_input(TurnResume::Exact {
            vendor_session_id: "vendor-f3b".into(),
        });
        h.submit(
            "t1",
            &scoped_payload(
                &native_target(NativeProfile::PlainChat, false),
                &input,
                None,
                Some(&scope("convo-f3b", "msg-1", 1)),
            ),
        )
        .await;
        let attempt1 = h.claim("t1").await;
        // The first attempt observes the identity, delivers the prompt,
        // and ends inconclusively (uncertain — no root result, input
        // accepted: the discharge marks the claim uncertain).
        h.shared.queue(&[
            r#"{"e":"init","s":"vendor-f3b","m":"sonnet-x"}"#,
            r#"{"e":"delta","t":"partial"}"#,
        ]);
        h.shared.set_outcome(NativeTerminalOutcome::Uncertain {
            class: FailureClass::ExitWithoutResult,
            detail: "review fixture: exited without a root result".into(),
            partial_text: None,
        });
        h.executor()
            .execute(h.context(
                &attempt1,
                CancellationToken::new(),
                CancellationToken::new(),
            ))
            .await
            .expect_err("no scripted outcome is never a success");
        assert!(
            h.store
                .current_native_binding("t1")
                .await
                .unwrap()
                .is_some(),
            "the task-local observation persists for the retry"
        );

        // The runner retries the SAME task (attempt 2), then the branch
        // retirement puts every continuation of the conversation out of
        // service (OneMind / user deletes the branch).
        h.store
            .retry_task(agentkit_store::RetryTask {
                fence: attempt1.lease.fence(),
                now: h.runtime.now_ms(),
                available_at: h.runtime.now_ms(),
                max_attempts: 5,
                error: "inconclusive vendor outcome".into(),
            })
            .await
            .unwrap();
        h.store.retire_continuations("convo-f3b").await.unwrap();

        let attempt2 = h.claim("t1").await;
        assert_ne!(attempt2.lease.attempt_id, attempt1.lease.attempt_id);
        let spawned_before = h.shared.spawn_count.load(Ordering::SeqCst);
        let (code, _) = Harness::failure(
            h.executor()
                .execute(h.context(
                    &attempt2,
                    CancellationToken::new(),
                    CancellationToken::new(),
                ))
                .await
                .unwrap_err(),
        );
        assert_eq!(
            code.as_deref(),
            Some("continuation_mismatch"),
            "the observed shortcut must not outlive the ledger"
        );
        assert_eq!(
            h.shared.spawn_count.load(Ordering::SeqCst),
            spawned_before,
            "no spawn from the retired session"
        );
    }

    #[tokio::test]
    async fn two_current_tasks_one_binding_exactly_one_claim_winner() {
        // Review round-4 F3-C reproduced: two current tasks for the same
        // conversation position must never both start from the same
        // exact binding. The atomic claim (store-level CAS) has exactly
        // one winner; the loser takes the fresh-session decision.
        let h = harness();
        publish_input_position(&h, "convo-f3c", "msg-1", 1, "vendor-f3c").await;
        let input = turn_input(TurnResume::Exact {
            vendor_session_id: "vendor-f3c".into(),
        });
        let scope = scope("convo-f3c", "msg-1", 1);
        h.submit(
            "t-a",
            &scoped_payload(
                &native_target(NativeProfile::PlainChat, false),
                &input,
                None,
                Some(&scope),
            ),
        )
        .await;
        h.submit(
            "t-b",
            &scoped_payload(
                &native_target(NativeProfile::PlainChat, false),
                &input,
                None,
                Some(&scope),
            ),
        )
        .await;
        let claim_a = h.claim("t-a").await;
        let claim_b = h.claim("t-b").await;

        // Task A takes the claim first (the concurrent winner).
        let won = h
            .store
            .claim_continuation(
                "convo-f3c",
                "msg-1",
                1,
                BACKEND,
                "/usr/bin/claude",
                "cfg-digest",
                None,
                "t-a",
                &claim_a.lease.attempt_id,
                h.runtime.now_ms(),
            )
            .await
            .unwrap()
            .expect("the first claim wins");
        assert_eq!(won.state, "owned");

        // Task B's executor must not consume it: refused from the ledger,
        // zero OS contact.
        let (code, _) = Harness::failure(
            h.executor()
                .execute(h.context(&claim_b, CancellationToken::new(), CancellationToken::new()))
                .await
                .unwrap_err(),
        );
        assert_eq!(code.as_deref(), Some("continuation_mismatch"));
        assert_eq!(
            h.shared.spawn_count.load(Ordering::SeqCst),
            1,
            "the loser never spawns (the only spawn so far is the seeding turn)"
        );

        // The winner runs its turn normally (claim re-entry is
        // idempotent) and completes.
        h.shared
            .queue(&[r#"{"e":"init","s":"vendor-f3c","m":"sonnet-x"}"#]);
        h.shared
            .set_outcome(Shared::completed("vendor-f3c", "continued"));
        h.executor()
            .execute(h.context(&claim_a, CancellationToken::new(), CancellationToken::new()))
            .await
            .expect("the claim winner resumes normally");
        assert_eq!(
            h.shared.spawn_count.load(Ordering::SeqCst),
            2,
            "seed + winner"
        );
        assert!(
            h.store
                .active_continuation(
                    "convo-f3c",
                    "msg-1:answer",
                    2,
                    BACKEND,
                    "/usr/bin/claude",
                    "cfg-digest",
                    None
                )
                .await
                .unwrap()
                .is_some(),
            "completion publishes the new active position"
        );
    }

    #[tokio::test]
    async fn contradictory_continuation_identity_is_refused_before_spawn() {
        // Review round-4 F8-b at the executor boundary: a frozen scope
        // whose configuration identity contradicts the frozen target is
        // refused at claim, before any OS contact.
        let h = harness();
        let input = turn_input(TurnResume::Exact {
            vendor_session_id: "vendor-x".into(),
        });
        let drifted = ContinuationScope {
            config_identity: "cfg-other".into(),
            ..scope("convo-f8b", "msg-1", 1)
        };
        h.submit(
            "t1",
            &scoped_payload(
                &native_target(NativeProfile::PlainChat, false),
                &input,
                None,
                Some(&drifted),
            ),
        )
        .await;
        let claimed = h.claim("t1").await;
        let (code, _) = Harness::failure(
            h.executor()
                .execute(h.context(&claimed, CancellationToken::new(), CancellationToken::new()))
                .await
                .unwrap_err(),
        );
        assert_eq!(code.as_deref(), Some("continuation_scope_mismatch"));
        assert_eq!(h.shared.spawn_count.load(Ordering::SeqCst), 0);
    }
}
