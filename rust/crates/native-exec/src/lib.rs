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
mod tests;
