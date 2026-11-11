//! OPTIONAL standalone facade for driving one native Chat turn (AGENTKIT-117).
//!
//! This crate is for consumers that want to drive a single native Chat turn
//! against an official vendor CLI **without** the durable host: there is NO
//! task/attempt store, NO leases, NO approval governance and NO MCP broker
//! here. Those authorities live in `agentkit-host` / `agentkit-store` and
//! nothing in this facade substitutes for them.
//!
//! # Loud limitations
//! * **PlainChat only.** [`NativeProfile::OwnedMcpChat`] targets are refused
//!   before any spawn with the stable refusal code
//!   `"facade_profile_unsupported"`: the scoped broker endpoint is staged by
//!   the host and cannot be synthesized here.
//! * No durable lineage: the vendor session identity is verified against the
//!   frozen plan but never persisted. A crash mid-turn leaves no record.
//! * Retries are the caller's decision; [`FacadeError::retryable`] is a
//!   hint, never an authorization for automatic retry loops.
//!
//! The stream loop mirrors `agentkit-host`'s generic executor: startup /
//! idle / overall bounds, interrupt-on-timeout, quarantine on protocol
//! violation, bounded reap, and the adapter (not the exit) decides the
//! authoritative terminal outcome — a clean exit without a root result can
//! never produce `Completed` (AGENTKIT-107 §7).
//!
//! [`conformance`] hosts the driver conformance harness: stable case ids,
//! a runner that exercises the [`NativeBackend`] / [`ExecutableLocator`]
//! ports generically, and reports consumed by qualification.

pub mod conformance;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use agentkit_claude::ClaudeCodeBackend;
use agentkit_claude::backend::default_backend;
use agentkit_claude::discovery::ClaudeSupportMatrix;
use agentkit_claude::transport::OwnedProcessSupervisor;

/// Curated driver surface: the concrete locator/matrix behind
/// [`NativeChat::new`]'s default wiring.
pub use agentkit_claude::{ClaudeLocator, ClaudeSupportMatrix as SupportMatrix};
/// Curated contract surface: everything a standalone consumer needs to
/// build a target and interpret a turn, without importing internals.
pub use agentkit_native::{
    AuthEvidence, AuthExpectation, AuthMode, CapabilitySnapshot, CapabilityState, ContentPolicy,
    ContextBlock, EXECUTION_TARGET_SCHEMA_VERSION, ExecutableEvidence, ExecutableLocator,
    FailureClass, FinishDisposition, FrameLine, InterruptPolicy, InterruptReceipt, LaunchPlan,
    McpServerStatus, NativeBackend, NativeError, NativeEvent, NativeExecutionTarget, NativeProfile,
    NativeRefusal, NativeResult, NativeTerminalOutcome, OwnedChildHandle, PermissionPromptPolicy,
    PolicyState, PreflightReport, ProcessExit, ProcessSupervisor, ProtocolViolation,
    ResourceLimits, StreamFrameAdapter, TransportFault, TurnInput, TurnResume,
    validate_execution_target,
};
/// Cancellation handle used by [`NativeChatOptions::cancellation`].
pub use tokio_util::sync::CancellationToken;

/// Tunables for [`NativeChat`]. Defaults are the contract's bounded
/// resources; a standalone caller normally only adjusts `limits`.
#[derive(Clone, Debug)]
pub struct NativeChatOptions {
    /// Bounded resources for each turn (timeouts, byte caps).
    pub limits: ResourceLimits,
    /// Cancellation shared across turns: cancelling it interrupts the live
    /// child with [`InterruptPolicy::HostShutdown`] and maps the turn to
    /// [`FacadeError::Cancelled`].
    pub cancellation: CancellationToken,
}

impl Default for NativeChatOptions {
    fn default() -> Self {
        NativeChatOptions {
            limits: ResourceLimits::default(),
            cancellation: CancellationToken::new(),
        }
    }
}

/// Facade error vocabulary. Never loses retryability: a
/// [`NativeRefusal`] is carried whole inside [`FacadeError::Refusal`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[serde(tag = "kind", rename_all = "camelCase", deny_unknown_fields)]
pub enum FacadeError {
    /// Deterministic pre-spawn refusal; `refusal.retryable` is preserved.
    #[error("refused: {refusal}")]
    Refusal { refusal: NativeRefusal },
    /// Transport-level fault while an owned child was live.
    #[error("transport fault: {code}: {message}")]
    Transport { code: String, message: String },
    /// The vendor protocol produced a shape the qualified adapter cannot
    /// interpret, or a frozen-identity mismatch (code
    /// `"session_identity_mismatch"`).
    #[error("protocol violation: {code}: {message}")]
    Protocol { code: String, message: String },
    /// Host/user cancellation via [`NativeChatOptions::cancellation`].
    #[error("cancelled")]
    Cancelled,
}

impl FacadeError {
    /// Whether retrying the same frozen target could plausibly succeed.
    /// A hint for UX, never an authorization for automatic retries.
    pub fn retryable(&self) -> bool {
        match self {
            FacadeError::Refusal { refusal } => refusal.retryable,
            FacadeError::Transport { .. } => true,
            FacadeError::Protocol { .. } => false,
            FacadeError::Cancelled => true,
        }
    }

    /// Stable machine code for logs/UX mapping.
    pub fn code(&self) -> &str {
        match self {
            FacadeError::Refusal { refusal } => &refusal.code,
            FacadeError::Transport { code, .. } => code,
            FacadeError::Protocol { code, .. } => code,
            FacadeError::Cancelled => "cancelled",
        }
    }
}

impl From<NativeError> for FacadeError {
    fn from(value: NativeError) -> Self {
        match value {
            NativeError::Refusal { refusal } => FacadeError::Refusal { refusal },
            NativeError::Transport { code, message } => FacadeError::Transport { code, message },
            NativeError::Protocol { code, message } => FacadeError::Protocol { code, message },
            // The facade holds no attempt/lease authority: a stale-attempt
            // error can only come from a host-bound injected supervisor and
            // is surfaced as an explicit protocol-level fault.
            NativeError::StaleAttempt { attempt_id } => FacadeError::Protocol {
                code: "stale_attempt".to_owned(),
                message: format!("facade has no attempt authority ({attempt_id})"),
            },
            NativeError::Cancelled => FacadeError::Cancelled,
        }
    }
}

/// Result alias for facade operations.
pub type FacadeResult<T> = Result<T, FacadeError>;

/// High-level one-turn facade over the optional Claude Code driver.
///
/// Constructed with the driver's default backend/locator and the claude
/// transport supervisor; alternative supervisors are injected via
/// [`NativeChat::with_supervisor`] (the primary seam for tests and for
/// consumers owning their own process supervision).
pub struct NativeChat {
    backend: ClaudeCodeBackend,
    locator: Arc<dyn ExecutableLocator>,
    supervisor: Arc<dyn ProcessSupervisor>,
    options: NativeChatOptions,
}

impl NativeChat {
    /// Default wiring: `agentkit_claude::default_backend` (explicit
    /// executable or PATH discovery) plus the claude transport
    /// [`OwnedProcessSupervisor`]. `run_root` parents the private per-run
    /// directories this facade owns and removes after each turn.
    pub fn new(explicit_executable: Option<PathBuf>, run_root: impl Into<PathBuf>) -> Self {
        let (backend, locator) = default_backend(explicit_executable, run_root);
        NativeChat {
            backend,
            locator,
            supervisor: Arc::new(OwnedProcessSupervisor::new()),
            options: NativeChatOptions::default(),
        }
    }

    /// Override the turn options (limits, cancellation).
    pub fn with_options(mut self, options: NativeChatOptions) -> Self {
        self.options = options;
        self
    }

    /// Inject a different process supervisor (tests, custom ownership).
    pub fn with_supervisor(mut self, supervisor: Arc<dyn ProcessSupervisor>) -> Self {
        self.supervisor = supervisor;
        self
    }

    /// Inject a different executable locator (tests, custom discovery).
    pub fn with_locator(mut self, locator: Arc<dyn ExecutableLocator>) -> Self {
        self.locator = locator;
        self
    }

    /// The underlying driver's stable identity, e.g. `"claude-code"`.
    pub fn backend_id(&self) -> &str {
        self.backend.backend_id()
    }

    pub fn options(&self) -> &NativeChatOptions {
        &self.options
    }

    /// Readiness preflight without a caller-built target: resolves the
    /// canonical executable through the locator, freezes a minimal
    /// PlainChat / `AsConfigured` admission snapshot for the observed
    /// version, and runs the driver's preflight. Not live-verified
    /// (AGENTKIT-109): readiness notes are surfaced, never swallowed.
    pub async fn preflight(&self) -> FacadeResult<PreflightReport> {
        let evidence = self
            .locator
            .locate()
            .await
            .map_err(|refusal| FacadeError::Refusal { refusal })?;
        let target = self.preflight_target(&evidence);
        self.backend
            .preflight(&target, self.locator.as_ref(), &self.options.cancellation)
            .await
            .map_err(|refusal| FacadeError::Refusal { refusal })
    }

    fn preflight_target(&self, evidence: &ExecutableEvidence) -> NativeExecutionTarget {
        NativeExecutionTarget {
            schema_version: EXECUTION_TARGET_SCHEMA_VERSION,
            backend: self.backend.backend_id().to_owned(),
            backend_version: Some(evidence.version.clone()),
            profile: NativeProfile::PlainChat,
            executable: evidence.path.display().to_string(),
            requested_model: None,
            requested_effort: None,
            auth_expectation: AuthExpectation::AsConfigured,
            capabilities: ClaudeSupportMatrix::snapshot(Some(&evidence.version)),
            config_identity: "agentkit-facade-preflight".to_owned(),
            system_prompt_revision: None,
            content: ContentPolicy::default(),
        }
    }

    /// Drive exactly one plain Chat turn end to end: preflight → frozen
    /// plan (no MCP endpoint: this facade has no broker) → spawn → input
    /// delivery → bounded streaming with interrupt-on-timeout → reap →
    /// adapter-decided terminal outcome.
    ///
    /// Refuses [`NativeProfile::OwnedMcpChat`] targets with the stable code
    /// `"facade_profile_unsupported"`; there is no durable store to bind
    /// lineage to, so caller-side persistence of the returned session id is
    /// required for any later [`TurnResume::Exact`].
    pub async fn chat_turn(
        &self,
        target: &NativeExecutionTarget,
        input: &TurnInput,
    ) -> FacadeResult<NativeTerminalOutcome> {
        if target.profile != NativeProfile::PlainChat {
            return Err(FacadeError::Refusal {
                refusal: NativeRefusal::new(
                    "facade_profile_unsupported",
                    "the standalone facade serves PlainChat only; OwnedMcpChat needs the \
                     host-staged broker endpoint",
                    false,
                ),
            });
        }

        // 1. Driver preflight against the frozen target.
        self.backend
            .preflight(target, self.locator.as_ref(), &self.options.cancellation)
            .await
            .map_err(|refusal| FacadeError::Refusal { refusal })?;

        // 2. Frozen plan. No MCP endpoint exists without the host broker.
        let plan = self
            .backend
            .plan_turn(target, input, None)
            .map_err(|refusal| FacadeError::Refusal { refusal })?;

        // 3. Spawn the owned process (readers start immediately inside).
        let mut child = self
            .supervisor
            .spawn(&plan, &self.options.limits, &self.options.cancellation)
            .await
            .map_err(FacadeError::from)?;

        // 4. Deliver the admitted input. A stdin fault is recorded, not
        //    fatal: the stream loop and the adapter decide honestly.
        let input_bytes = self
            .backend
            .encode_input(input)
            .map_err(|refusal| FacadeError::Refusal { refusal })?;
        let mut transport_faults: Vec<TransportFault> = Vec::new();
        if let Err(fault) = child.deliver_input(&input_bytes, true).await {
            transport_faults.push(fault);
        }

        self.drive_turn(
            child.as_mut(),
            &plan.native_session_id,
            &plan.per_run_dir,
            input,
            &mut transport_faults,
        )
        .await
    }
}

impl NativeChat {
    /// Bounded stream drive mirroring `agentkit-host`'s executor loop:
    /// startup/idle/overall budgets, interrupt-on-timeout, quarantine on
    /// protocol violation or identity mismatch, bounded reap, then the
    /// adapter maps the exit to the single authoritative outcome.
    async fn drive_turn(
        &self,
        child: &mut dyn OwnedChildHandle,
        frozen_session: &Option<String>,
        per_run_dir: &Path,
        input: &TurnInput,
        transport_faults: &mut Vec<TransportFault>,
    ) -> FacadeResult<NativeTerminalOutcome> {
        let bound_session: Option<String> = match &input.resume {
            TurnResume::Exact { vendor_session_id } => Some(vendor_session_id.clone()),
            _ => frozen_session.clone(),
        };
        let mut adapter = self.backend.new_frame_adapter();
        let mut saw_event = false;
        let mut protocol_violation: Option<(String, String)> = None;
        let mut identity_mismatch: Option<String> = None;
        let mut timed_out: Option<FailureClass> = None;
        let mut interrupt_receipt: Option<InterruptReceipt> = None;
        let mut cancel_observed = false;

        let (tx, mut rx) = mpsc::unbounded_channel::<InterruptPolicy>();
        let watcher = {
            let cancel = self.options.cancellation.clone();
            let tx1 = tx.clone();
            tokio::spawn(async move {
                cancel.cancelled().await;
                let _ = tx1.send(InterruptPolicy::HostShutdown);
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
                // Already interrupted: keep draining frames until EOF or
                // the reap window closes, then stop waiting on the stream.
                tokio::select! {
                    frame = child.next_frame() => match frame {
                        None => break,
                        Some(FrameLine::Line(line)) => {
                            if let Err(violation) = adapter.push_line(&line) {
                                protocol_violation
                                    .get_or_insert((violation.code, violation.message));
                            }
                        }
                        Some(FrameLine::Fault(fault)) => transport_faults.push(fault),
                    },
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
                                if let NativeEvent::Init { session_id, .. } = &event
                                    && let Some(bound) = &bound_session
                                    && bound != session_id
                                {
                                    identity_mismatch = Some(format!(
                                        "init reports '{session_id}' but the turn is bound to '{bound}'"
                                    ));
                                    interrupt_receipt = Some(
                                        child.interrupt(InterruptPolicy::Quarantine).await,
                                    );
                                    break;
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
                        cancel_observed = true;
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

        // Reap to completion, bounded. A stalled child is reported with
        // `after_interrupt` honesty, never as success.
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

        // The CLI explains early deaths on stderr; keep a bounded,
        // sanitised tail for exit-derived failures (read after the reap).
        let stderr_tail = agentkit_native::bounded_stderr_tail(&child.stderr_tail());

        // The facade owns the private per-run directory the host would
        // otherwise settle: best-effort bounded removal, residue is visible
        // through `cleanup_uncertain` below, never silently ignored.
        let dir_removed = tokio::time::timeout(
            Duration::from_millis(self.options.limits.cleanup_timeout_ms.max(1) as u64),
            tokio::fs::remove_dir_all(per_run_dir),
        )
        .await
        .map(|r| match r {
            Ok(()) => true,
            // A supervisor that never materialized the directory left no residue.
            Err(e) => e.kind() == std::io::ErrorKind::NotFound,
        })
        .unwrap_or(false);

        if let Some(detail) = &identity_mismatch {
            return Err(FacadeError::Protocol {
                code: "session_identity_mismatch".to_owned(),
                message: detail.clone(),
            });
        }
        if cancel_observed {
            return Err(FacadeError::Cancelled);
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
                message: format!("turn exceeded its {class:?} budget"),
                partial_text: partial,
            }
        } else {
            adapter.terminal_outcome(&exit)
        };

        // Cleanup uncertainty stays visible: receipts, faults and staged
        // directories that survived are appended to the failure detail
        // rather than swallowed.
        let cleanup_uncertain = interrupt_receipt
            .as_ref()
            .is_some_and(|receipt| receipt.cleanup_uncertain)
            || transport_faults
                .iter()
                .any(|fault| fault.code == "cleanup_uncertain")
            || self
                .supervisor
                .descendant_cleanup(exit.pid, exit.generation)
                .is_some_and(|receipt| receipt.cleanup_uncertain)
            || !dir_removed;
        // Only outcomes decided by the exit rows (no root result) get the
        // stderr suffix; the rule lives next to the sanitiser in `native`.
        let outcome = match outcome {
            NativeTerminalOutcome::Failed {
                class,
                message,
                partial_text,
            } => NativeTerminalOutcome::Failed {
                class,
                message: agentkit_native::with_stderr_suffix(class, message, &stderr_tail),
                partial_text,
            },
            NativeTerminalOutcome::Uncertain {
                class,
                detail,
                partial_text,
            } => NativeTerminalOutcome::Uncertain {
                class,
                detail: agentkit_native::with_stderr_suffix(class, detail, &stderr_tail),
                partial_text,
            },
            other => other,
        };
        Ok(match (outcome, cleanup_uncertain) {
            (NativeTerminalOutcome::Completed { text, .. }, true) => {
                NativeTerminalOutcome::Uncertain {
                    class: FailureClass::CleanupUncertain,
                    detail: "answer completed but owned descendant cleanup is uncertain".to_owned(),
                    partial_text: Some(text),
                }
            }
            (
                NativeTerminalOutcome::Failed {
                    class,
                    mut message,
                    partial_text,
                },
                true,
            ) => {
                message.push_str("; cleanup uncertain");
                NativeTerminalOutcome::Failed {
                    class,
                    message,
                    partial_text,
                }
            }
            (
                NativeTerminalOutcome::Uncertain {
                    class,
                    mut detail,
                    partial_text,
                },
                true,
            ) => {
                detail.push_str("; cleanup uncertain");
                NativeTerminalOutcome::Uncertain {
                    class,
                    detail,
                    partial_text,
                }
            }
            (outcome, _) => outcome,
        })
    }
}
