use super::*;
use agentkit_engine::{SharedRuntime, SystemRuntime};
use agentkit_mcp::{Backend as _, SessionScope};
use agentkit_native::{
    AuthEvidence, AuthMode, CapabilitySnapshot, CapabilityState, ExecutableEvidence, LaunchPlan,
    McpEndpointSpec, NativeRefusal, OwnedChildHandle, PermissionPromptPolicy, PolicyState,
    StreamFrameAdapter,
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
        self.shared
            .outcome
            .lock()
            .unwrap()
            .clone()
            .unwrap_or(NativeTerminalOutcome::Uncertain {
                class: FailureClass::ExitWithoutResult,
                detail: "no scripted outcome".into(),
                partial_text: None,
            })
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
    async fn deliver_input(&mut self, bytes: &[u8], _close: bool) -> Result<(), TransportFault> {
        self.shared.delivered.lock().unwrap().push(bytes.to_vec());
        if self.shared.delivery_hang.load(Ordering::SeqCst) {
            std::future::pending::<()>().await;
        }
        Ok(())
    }
    async fn interrupt(&mut self, policy: InterruptPolicy) -> agentkit_native::InterruptReceipt {
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
    async fn drain_owned(&self, policy: InterruptPolicy) -> Vec<agentkit_native::InterruptReceipt> {
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
    async fn drain_owned(&self, policy: InterruptPolicy) -> Vec<agentkit_native::InterruptReceipt> {
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
    ) -> Result<agentkit_native_broker::BrokerExecuted, agentkit_native_broker::BrokerDenial> {
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

mod continuation;
mod dispatch;
mod mcp;
mod process;
mod turn;
