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
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use agentkit_native::{
    CapabilityViolation, ExecutableLocator, ExecutionTarget, FailureClass, FinishDisposition,
    FrameLine, InterruptPolicy, NativeBackend, NativeError, NativeEvent, NativeExecutionTarget,
    NativeProfile, NativeTerminalOutcome, PreflightReport, ProcessExit, ProcessSupervisor,
    ResourceLimits, TransportFault, TurnInput, TurnResume, validate_execution_target,
};
use agentkit_store::{BindNativeSession, SqliteStore};

use crate::native_bridge::{AttemptBroker, BrokerLimits, BrokerToolDefinition};
use crate::{ExecutionContext, ExecutionFailure, HostError, HostResult, RetrySafety, TaskExecutor};

/// Payload key carrying the frozen `ExecutionTarget` JSON.
pub const EXECUTION_TARGET_PAYLOAD_KEY: &str = "executionTarget";
/// Payload key carrying the frozen `TurnInput` JSON.
pub const TURN_INPUT_PAYLOAD_KEY: &str = "turnInput";
/// Payload key carrying the frozen broker stanza (OwnedMcpChat only).
pub const BROKER_PAYLOAD_KEY: &str = "broker";

/// One catalog entry in the serialized payload form (`broker.tools[]`).
/// Names become `mcp__<server>__<name>` at the vendor.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BrokerToolSpec {
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub input_schema: Value,
    #[serde(default)]
    pub writes: bool,
}

/// The frozen broker stanza of an OwnedMcpChat turn.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BrokerSpec {
    pub server_name: String,
    pub tools: Vec<BrokerToolSpec>,
    #[serde(default)]
    pub limits: Option<BrokerLimitsSpec>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BrokerLimitsSpec {
    pub max_calls: usize,
    pub max_args_bytes: usize,
    pub max_result_bytes: usize,
    pub call_timeout_ms: i64,
}

/// Everything a factory needs to build the per-attempt broker authority.
pub struct AttemptBrokerContext<'a> {
    pub task_id: &'a str,
    pub attempt_id: &'a str,
    pub server_name: &'a str,
    pub target: &'a NativeExecutionTarget,
    pub catalog: Vec<BrokerToolDefinition>,
    pub limits: BrokerLimits,
    pub cancellation: CancellationToken,
}

/// Consumer-supplied authority source: the factory owns the
/// `BrokerAuthority` implementation (approvals, effects, argument
/// validation — AGENTKIT-115) and the attempt's liveness view.
#[async_trait]
pub trait AttemptBrokerFactory: Send + Sync {
    async fn start(&self, ctx: &AttemptBrokerContext<'_>) -> HostResult<Arc<AttemptBroker>>;
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

/// The generic durable executor. Stateless across attempts; all per-attempt
/// state lives in the store, the child handle and the adapter.
pub struct NativeAgentExecutor {
    store: SqliteStore,
    backend: Arc<dyn NativeBackend>,
    supervisor: Arc<dyn ProcessSupervisor>,
    locator: Arc<dyn ExecutableLocator>,
    broker_factory: Option<Arc<dyn AttemptBrokerFactory>>,
    options: NativeExecutorOptions,
}

impl NativeAgentExecutor {
    pub fn new(
        store: SqliteStore,
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

    /// Parse a payload target without executing. Admission and execution
    /// must apply the same parse (AGENTKIT-112/113).
    pub fn parse_target(payload: &Value) -> Result<ExecutionTarget, String> {
        let raw = payload
            .get(EXECUTION_TARGET_PAYLOAD_KEY)
            .ok_or_else(|| format!("payload is missing '{EXECUTION_TARGET_PAYLOAD_KEY}'"))?;
        serde_json::from_value::<ExecutionTarget>(raw.clone())
            .map_err(|e| format!("invalid execution target: {e}"))
    }

    /// Capability verdict for any target; `Ok(())` means admissible
    /// (model-loop targets always pass — behavior unchanged).
    pub fn capability_verdict(target: &ExecutionTarget) -> Result<(), Vec<CapabilityViolation>> {
        validate_execution_target(target)
    }

    fn terminal(&self, code: &str, message: impl Into<String>) -> HostError {
        HostError::Failure(ExecutionFailure {
            code: Some(code.to_string()),
            retryable: Some(false),
            retry_safety: RetrySafety::UnknownOutcome,
            ..ExecutionFailure::new(message)
        })
    }

    fn transient(&self, code: &str, message: impl Into<String>) -> HostError {
        HostError::Failure(ExecutionFailure {
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
        context: &ExecutionContext,
        backend: &str,
        vendor_session_id: &str,
        parent: Option<String>,
    ) -> HostResult<()> {
        self.store
            .bind_native_session(BindNativeSession {
                attempt_id: context.attempt_id.clone(),
                task_id: context.task.task_id.clone(),
                backend: backend.to_string(),
                vendor_session_id: vendor_session_id.to_string(),
                parent_vendor_session_id: parent,
                assigned_at: context.runtime.now_ms(),
            })
            .await?;
        self.store
            .observe_native_session(
                &context.attempt_id,
                vendor_session_id,
                context.runtime.now_ms(),
            )
            .await?;
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
        context: &ExecutionContext,
        broker: Option<&Arc<AttemptBroker>>,
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

    async fn emit(&self, context: &ExecutionContext, kind: &str, payload: Value) -> HostResult<()> {
        context.emit(kind, payload).await
    }

    async fn emit_session(
        &self,
        context: &ExecutionContext,
        kind: &str,
        session_id: &str,
        fingerprint: &str,
    ) -> HostResult<()> {
        context
            .emit(
                kind,
                json!({"sessionId": session_id, "fingerprint": fingerprint}),
            )
            .await
    }
}

#[async_trait]
impl TaskExecutor for NativeAgentExecutor {
    async fn execute(&self, context: ExecutionContext) -> HostResult<()> {
        // 1. Typed dispatch: only native targets run here.
        let target = match Self::parse_target(&context.task.payload) {
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
        if let Err(violations) = Self::capability_verdict(&ExecutionTarget::Native(target.clone()))
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

        // 4. Lineage preconditions for exact resume (AGENTKIT-113).
        // A continuation is a NEW task: the conversation's observed
        // identity lives on the previous task's binding, so the gate
        // accepts either this task's own observed binding or the identity
        // being currently OBSERVED anywhere on this host (vendor-attested
        // by an init frame). Assigned-only and retired identities never
        // qualify.
        let lineage_parent: Option<String> = match &input.resume {
            TurnResume::Exact { vendor_session_id } => {
                let binding = self
                    .store
                    .current_native_binding(&context.task.task_id)
                    .await?;
                match &binding {
                    Some(binding) if binding.vendor_session_id == *vendor_session_id => {
                        Some(binding.vendor_session_id.clone())
                    }
                    _ => {
                        if self
                            .store
                            .is_observed_native_session(vendor_session_id)
                            .await?
                        {
                            Some(vendor_session_id.clone())
                        } else if let Some(binding) = binding {
                            return Err(self.terminal(
                                "lineage_mismatch",
                                format!(
                                    "exact resume asks '{vendor_session_id}' but the observed lineage is '{}'",
                                    binding.vendor_session_id
                                ),
                            ));
                        } else {
                            return Err(self.terminal(
                                "lineage_missing",
                                format!(
                                    "exact resume of '{vendor_session_id}' has no observed lineage"
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

        // 7. Frozen plan.
        let plan = match self.backend.plan_turn(
            &target,
            &input,
            broker.as_ref().map(|b| b.spec()).as_ref(),
        ) {
            Ok(plan) => plan,
            Err(refusal) if refusal.retryable => {
                self.settle_cleanup(&context, broker.as_ref(), None).await;
                return Err(self.transient(&refusal.code, refusal.message));
            }
            Err(refusal) => {
                self.settle_cleanup(&context, broker.as_ref(), None).await;
                return Err(self.terminal(&refusal.code, refusal.message));
            }
        };

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
                        .await?;
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

        // 9. Spawn the owned process (readers start immediately inside).
        let mut child = match self
            .supervisor
            .spawn(&plan, &self.options.limits, &context.cancellation)
            .await
        {
            Ok(child) => child,
            Err(err) => {
                self.settle_cleanup(&context, broker.as_ref(), Some(&plan.per_run_dir))
                    .await;
                let (code, message) = native_error_code(&err);
                // A spawn failure may have touched the OS; uncertain-safe.
                return Err(HostError::Failure(ExecutionFailure {
                    code: Some(code),
                    retryable: Some(true),
                    retry_safety: RetrySafety::UnknownOutcome,
                    ..ExecutionFailure::new(message)
                }));
            }
        };
        self.emit(
            &context,
            "native.spawn",
            json!({
                "pid": child.pid(),
                "generation": child.generation(),
                "fingerprint": plan.fingerprint,
                "nativeSessionId": bound_session,
            }),
        )
        .await?;

        // 10. Deliver the admitted input. A transport fault here (early exit
        // before stdin drained) is recorded, not fatal: the stream loop and
        // the adapter decide the terminal outcome honestly.
        let input_bytes = match self.backend.encode_input(&input) {
            Ok(bytes) => bytes,
            Err(refusal) => {
                let _ = child.interrupt(InterruptPolicy::Quarantine).await;
                let _ = child.wait().await;
                self.settle_cleanup(&context, broker.as_ref(), Some(&plan.per_run_dir))
                    .await;
                return Err(self.terminal(&refusal.code, refusal.message));
            }
        };
        if let Some(fault) = child.deliver_input(&input_bytes, true).await.err() {
            self.emit(
                &context,
                "native.input.fault",
                json!({"code": fault.code, "message": fault.message}),
            )
            .await?;
        }

        // 11. Stream loop with startup/idle/overall bounds and dual cancel.
        let mut adapter = self.backend.new_frame_adapter();
        let mut saw_event = false;
        let mut protocol_violation: Option<(String, String)> = None;
        let mut identity_mismatch: Option<String> = None;
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
            let overall_left = overall.saturating_sub(started.elapsed());
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
                                            self.store
                                                .observe_native_session(
                                                    &context.attempt_id,
                                                    session_id,
                                                    context.runtime.now_ms(),
                                                )
                                                .await?;
                                            observed_session = Some(session_id.clone());
                                            self.emit_session(
                                                &context,
                                                "native.session.observed",
                                                session_id,
                                                &plan.fingerprint,
                                            )
                                            .await?;
                                        }
                                    } else {
                                        self.bind_and_observe(
                                            &context,
                                            &target.backend,
                                            session_id,
                                            lineage_parent.clone(),
                                        )
                                        .await?;
                                        bound_session = Some(session_id.clone());
                                        observed_session = Some(session_id.clone());
                                        self.emit_session(
                                            &context,
                                            "native.session.observed",
                                            session_id,
                                            &plan.fingerprint,
                                        )
                                        .await?;
                                    }
                                }
                                self.emit(
                                    &context,
                                    "native.event",
                                    json!({
                                        "event": event,
                                        "fingerprint": plan.fingerprint,
                                    }),
                                )
                                .await?;
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

        // 13. Authoritative terminal outcome: the adapter decides (the only
        // component that saw the vendor's root result facts).
        if let Some(detail) = &identity_mismatch {
            self.settle_cleanup(&context, broker.as_ref(), Some(&plan.per_run_dir))
                .await;
            return Err(self.terminal("session_identity_mismatch", detail.clone()));
        }
        let outcome = if let Some((code, message)) = &protocol_violation {
            NativeTerminalOutcome::Failed {
                class: FailureClass::ProtocolViolation,
                message: format!("{code}: {message}"),
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
        let cleanup_uncertain = interrupt_receipt
            .as_ref()
            .is_some_and(|receipt| receipt.cleanup_uncertain)
            || transport_faults
                .iter()
                .any(|f| f.code == "cleanup_uncertain");
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
                    return Err(HostError::Cancelled);
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
                self.emit(
                    &context,
                    "native.failed",
                    json!({
                        "class": class,
                        "message": message,
                        "partialText": partial_text,
                        "transportFaults": transport_faults,
                        "cleanupUncertain": cleanup_uncertain,
                        "fingerprint": plan.fingerprint,
                    }),
                )
                .await?;
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
                    Err(HostError::Failure(ExecutionFailure {
                        code: Some(code),
                        retryable: Some(true),
                        retry_safety: RetrySafety::SafeToRetry,
                        ..ExecutionFailure::new(message)
                    }))
                } else {
                    Err(HostError::Failure(ExecutionFailure {
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
                self.emit(
                    &context,
                    "native.uncertain",
                    json!({
                        "class": class,
                        "detail": detail,
                        "partialText": partial_text,
                        "cleanupUncertain": cleanup_uncertain,
                        "fingerprint": plan.fingerprint,
                    }),
                )
                .await?;
                Err(HostError::Failure(ExecutionFailure {
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
pub struct NativeDispatchExecutor {
    pub native: Arc<dyn TaskExecutor>,
    pub fallback: Arc<dyn TaskExecutor>,
}

#[async_trait]
impl TaskExecutor for NativeDispatchExecutor {
    async fn execute(&self, context: ExecutionContext) -> HostResult<()> {
        match NativeAgentExecutor::parse_target(&context.task.payload) {
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
                    return Err(HostError::Failure(ExecutionFailure {
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
    use crate::native_bridge::BrokerAuthority;
    use crate::{SharedRuntime, SystemRuntime};
    use agentkit_mcp::Backend as _;
    use agentkit_native::{
        AuthEvidence, AuthMode, CapabilitySnapshot, CapabilityState, ExecutableEvidence,
        LaunchPlan, McpEndpointSpec, NativeRefusal, OwnedChildHandle, PermissionPromptPolicy,
        PolicyState, StreamFrameAdapter,
    };
    use agentkit_store::{ClaimInput, SubmitTask};
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
        }
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
        outcome: Arc<Mutex<Option<NativeTerminalOutcome>>>,
        preflight_refusal: Arc<Mutex<Option<NativeRefusal>>>,
        preflight_notes: Arc<Mutex<Vec<String>>>,
        preflight_blocking: Arc<Mutex<Vec<String>>>,
        plan_refusal: Arc<Mutex<Option<NativeRefusal>>>,
        preassigned: Arc<Mutex<Option<String>>>,
        seen_mcp: Arc<Mutex<Option<Option<McpEndpointSpec>>>>,
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
            vec![]
        }
        async fn deliver_input(
            &mut self,
            bytes: &[u8],
            _close: bool,
        ) -> Result<(), TransportFault> {
            self.shared.delivered.lock().unwrap().push(bytes.to_vec());
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
            ProcessExit {
                pid: 4242,
                generation: 7,
                exit_code: Some(0),
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
            Ok(Box::new(FakeChild {
                shared: self.shared.clone(),
            }))
        }
        async fn drain_owned(
            &self,
            _policy: InterruptPolicy,
        ) -> Vec<agentkit_native::InterruptReceipt> {
            vec![]
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
        async fn start(&self, ctx: &AttemptBrokerContext<'_>) -> HostResult<Arc<AttemptBroker>> {
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
            .map_err(|e| HostError::Failure(ExecutionFailure::new(e)))?;
            self.started.lock().unwrap().push(broker.clone());
            Ok(broker)
        }
    }

    struct AllowAll;

    #[async_trait]
    impl BrokerAuthority for AllowAll {
        fn attempt_generation(&self) -> Option<u64> {
            Some(7)
        }
        async fn authorize(
            &self,
            _invocation: &crate::native_bridge::BrokerInvocation,
            _arguments: &Value,
        ) -> Result<(), crate::native_bridge::BrokerDenial> {
            Ok(())
        }
        async fn execute(
            &self,
            _invocation: &crate::native_bridge::BrokerInvocation,
            _arguments: Value,
        ) -> Result<crate::native_bridge::BrokerExecuted, crate::native_bridge::BrokerDenial>
        {
            Ok(crate::native_bridge::BrokerExecuted {
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

        fn executor(&self) -> NativeAgentExecutor {
            let backend = Arc::new(FakeBackend {
                shared: self.shared.clone(),
            });
            let supervisor = Arc::new(FakeSupervisor {
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
    async fn exact_resume_requires_observed_lineage() {
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
        assert_eq!(code.as_deref(), Some("lineage_missing"));
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
        // Reviewer P1-2 / ONEMIND-341 regression: a brand-new OneMind
        // conversation asks to resume the vendor session a *previous* task
        // observed. The lineage lives on the earlier task, not this one;
        // refusing it (the pre-fix behavior) killed every second turn.
        let h = harness();
        // Task one starts fresh and the vendor mints the session.
        h.shared
            .queue(&[r#"{"e":"init","s":"v-shared","m":"sonnet-x"}"#]);
        h.shared.set_outcome(Shared::completed("v-shared", "first"));
        h.submit(
            "t-first",
            &payload(
                &native_target(NativeProfile::PlainChat, false),
                &fresh_input(),
                None,
            ),
        )
        .await;
        let claimed = h.claim("t-first").await;
        h.executor()
            .execute(h.context(&claimed, CancellationToken::new(), CancellationToken::new()))
            .await
            .expect("first turn completes");

        // Task two is a brand-new task record resuming that exact session.
        let input = turn_input(TurnResume::Exact {
            vendor_session_id: "v-shared".into(),
        });
        h.shared
            .set_outcome(Shared::completed("v-shared", "second"));
        h.submit(
            "t-second",
            &payload(
                &native_target(NativeProfile::PlainChat, false),
                &input,
                None,
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
        assert_eq!(code.as_deref(), Some("lineage_missing"));
        assert_eq!(
            h.shared.spawn_count.load(Ordering::SeqCst),
            1,
            "the phantom resume never spawned"
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
        h.shared.queue(&[r#"{"e":"init","s":"vendor-1"}"#]);
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
            cancellation: CancellationToken::new(),
        };
        assert!(
            broker.backend().list_tools(&scope).await.is_err(),
            "settled attempts must not keep authenticating"
        );
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
    impl TaskExecutor for CountingExecutor {
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
}
