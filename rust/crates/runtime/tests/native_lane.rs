//! The open builder: a consumer-registered task kind is served by the turn
//! executor, and with a native lane configured, a task whose frozen target is
//! native runs the durable native path through the scripted ports
//! (AGENTKIT-107 §8 / phase C1).

use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use agentkit_core::{ChatProvider, ChatRequest, ProviderEvent, ProviderStream};
use agentkit_native::{
    AuthEvidence, AuthMode, BillingInclusion, CapabilitySnapshot, CapabilityState, ContextBlock,
    EXECUTION_TARGET_PAYLOAD_KEY, EXECUTION_TARGET_SCHEMA_VERSION, ExecutableEvidence,
    ExecutableLocator, FinishDisposition, FrameLine, InterruptPolicy, InterruptReceipt, LaunchPlan,
    McpEndpointSpec, NativeBackend, NativeError, NativeEvent, NativeExecutionTarget, NativeProfile,
    NativeRefusal, NativeTerminalOutcome, NativeUsage, OwnedChildHandle, PermissionPromptPolicy,
    PolicyState, PreflightReport, ProcessExit, ProcessSupervisor, ProtocolViolation,
    ResourceLimits, StreamFrameAdapter, TURN_INPUT_PAYLOAD_KEY, TokenSet, TransportFault,
    TurnInput, TurnResume, UsageProvenance,
};
use agentkit_native_exec::NativeExecutorOptions;
use agentkit_runtime::{AgentRuntime, NativeLane};
use agentkit_store::{SqliteStore, TaskStatus};
use async_trait::async_trait;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

const BACKEND: &str = "claude-code";
const SESSION: &str = "sess-native-lane-1";
const KIND: &str = "consumer.chat";

struct Fake(&'static str);

impl ChatProvider for Fake {
    fn id(&self) -> &str {
        self.0
    }
    fn stream_chat(&self, _: ChatRequest, _: CancellationToken) -> ProviderStream {
        Box::pin(futures::stream::iter([Ok(ProviderEvent::Completed {
            content: "ok".into(),
            tool_calls: vec![],
            finish_reason: "stop".into(),
        })]))
    }
}

fn supported() -> CapabilityState {
    CapabilityState::Supported {
        qualified_version: "2.1.310".into(),
        evidence: "test fixture".into(),
    }
}

fn unsupported(reason: &str) -> CapabilityState {
    CapabilityState::Unsupported {
        reason: reason.into(),
    }
}

fn target() -> NativeExecutionTarget {
    let mut flags = BTreeMap::new();
    flags.insert("text-input".to_string(), supported());
    flags.insert("extracted-text-input".to_string(), supported());
    flags.insert("plain-chat".to_string(), supported());
    flags.insert("owned-mcp-chat".to_string(), unsupported("plain profile"));
    flags.insert(
        "native-coding-tools".to_string(),
        unsupported("out of profile"),
    );
    NativeExecutionTarget {
        schema_version: EXECUTION_TARGET_SCHEMA_VERSION,
        backend: BACKEND.into(),
        backend_version: Some("2.1.310".into()),
        profile: NativeProfile::PlainChat,
        executable: "/usr/bin/claude".into(),
        requested_model: Some("sonnet".into()),
        requested_effort: None,
        auth_expectation: agentkit_native::AuthExpectation::SubscriptionLogin,
        capabilities: CapabilitySnapshot {
            backend: BACKEND.into(),
            matrix_version: 1,
            qualified_version: "2.1.310".into(),
            flags,
        },
        config_identity: "test-config".into(),
        system_prompt_revision: None,
        content: agentkit_native::ContentPolicy::default(),
    }
}

fn turn_input() -> TurnInput {
    TurnInput {
        input_id: "in-1".into(),
        text: "Say hello.".into(),
        context_blocks: vec![ContextBlock {
            kind: "user".into(),
            content_identity: "note-7@r3".into(),
            text: "Selected note: the launch is at dawn.".into(),
        }],
        resume: TurnResume::Fresh {
            seed_history: vec![],
        },
        system_prompt_revision: Some("rev-1".into()),
        system_prompt: Some("You are a helpful assistant.".into()),
        max_vendor_turns: Some(1),
        turn_deadline_ms: 30_000,
    }
}

// ----- scripted ports (copied from native-exec's lifecycle example) -----

#[derive(Clone, Default)]
struct Scripted {
    frames: Arc<Mutex<VecDeque<FrameLine>>>,
}

impl Scripted {
    fn emit(&self, value: &Value) {
        self.frames
            .lock()
            .unwrap()
            .push_back(FrameLine::Line(value.to_string().into_bytes()));
    }
}

struct ScriptedChild {
    frames: Arc<Mutex<VecDeque<FrameLine>>>,
    exited: bool,
}

#[async_trait]
impl OwnedChildHandle for ScriptedChild {
    fn pid(&self) -> u32 {
        std::process::id()
    }
    fn generation(&self) -> u64 {
        1
    }
    async fn next_frame(&mut self) -> Option<FrameLine> {
        self.frames.lock().unwrap().pop_front()
    }
    fn stderr_tail(&self) -> Vec<u8> {
        Vec::new()
    }
    async fn deliver_input(
        &mut self,
        _bytes: &[u8],
        _close_stdin: bool,
    ) -> Result<(), TransportFault> {
        Ok(())
    }
    async fn interrupt(&mut self, _policy: InterruptPolicy) -> InterruptReceipt {
        InterruptReceipt {
            terminated: true,
            escalated_to_sigterm: false,
            escalated_to_sigkill: false,
            cleanup_uncertain: false,
            detail: None,
        }
    }
    async fn wait(&mut self) -> ProcessExit {
        self.exited = true;
        ProcessExit {
            pid: self.pid(),
            generation: 1,
            exit_code: Some(0),
            signal: None,
            after_interrupt: false,
        }
    }
}

#[derive(Clone, Default)]
struct ScriptedAdapter {
    text: Arc<Mutex<String>>,
    final_text: Arc<Mutex<Option<String>>>,
    session: Arc<Mutex<Option<String>>>,
}

impl StreamFrameAdapter for ScriptedAdapter {
    fn push_line(&mut self, line: &[u8]) -> Result<Vec<NativeEvent>, ProtocolViolation> {
        let raw = String::from_utf8_lossy(line).into_owned();
        let value: Value = serde_json::from_str(&raw).map_err(|_| ProtocolViolation {
            code: "not_json".into(),
            message: raw,
        })?;
        match value["e"].as_str().unwrap_or_default() {
            "init" => {
                let session = value["s"].as_str().unwrap_or_default().to_string();
                *self.session.lock().unwrap() = Some(session.clone());
                Ok(vec![NativeEvent::Init {
                    session_id: session,
                    observed_model: Some("claude-sonnet".into()),
                    observed_effort: None,
                    observed_version: Some("2.1.310".into()),
                    capabilities: vec![],
                    mcp_servers: vec![],
                }])
            }
            "delta" => {
                let delta = value["t"].as_str().unwrap_or_default().to_string();
                self.text.lock().unwrap().push_str(&delta);
                Ok(vec![NativeEvent::TextDelta { text: delta }])
            }
            "result" => {
                *self.final_text.lock().unwrap() =
                    Some(value["r"].as_str().unwrap_or_default().to_string());
                Ok(vec![])
            }
            _ => Ok(vec![]),
        }
    }

    fn finish(&mut self) -> FinishDisposition {
        FinishDisposition::NoResult {
            partial_text: {
                let text = self.text.lock().unwrap().clone();
                if text.is_empty() { None } else { Some(text) }
            },
        }
    }

    fn terminal_outcome(&self, _exit: &ProcessExit) -> NativeTerminalOutcome {
        match self.final_text.lock().unwrap().as_deref() {
            Some(text) => NativeTerminalOutcome::Completed {
                text: text.to_string(),
                session_id: self
                    .session
                    .lock()
                    .unwrap()
                    .clone()
                    .unwrap_or_else(|| SESSION.into()),
                usage: Some(NativeUsage {
                    per_turn: Some(TokenSet {
                        input_tokens: Some(7),
                        output_tokens: Some(5),
                        ..Default::default()
                    }),
                    cumulative: None,
                    api_equivalent_estimate_usd: None,
                    estimate_provenance: UsageProvenance::VendorReported,
                    billing_inclusion: BillingInclusion::Unknown,
                }),
                permission_denials: vec![],
                observed_model: Some("claude-sonnet".into()),
                observed_effort: None,
            },
            None => NativeTerminalOutcome::Uncertain {
                class: agentkit_native::FailureClass::ExitWithoutResult,
                detail: "scripted child ended without a result frame".into(),
                partial_text: None,
            },
        }
    }
}

struct ScriptedBackend {
    planned: Arc<AtomicBool>,
}

#[async_trait]
impl NativeBackend for ScriptedBackend {
    fn backend_id(&self) -> &str {
        BACKEND
    }

    async fn preflight(
        &self,
        _target: &NativeExecutionTarget,
        locator: &dyn ExecutableLocator,
        _cancel: &CancellationToken,
    ) -> Result<PreflightReport, NativeRefusal> {
        Ok(PreflightReport {
            executable: locator.locate().await?,
            auth: locator.auth_status().await?,
            blocking_issues: vec![],
            readiness_notes: vec![],
        })
    }

    fn plan_turn(
        &self,
        _target: &NativeExecutionTarget,
        _input: &TurnInput,
        _mcp: Option<&McpEndpointSpec>,
    ) -> Result<LaunchPlan, NativeRefusal> {
        self.planned.store(true, Ordering::SeqCst);
        Ok(LaunchPlan {
            argv: vec![
                "/usr/bin/claude".into(),
                "-p".into(),
                "--output-format".into(),
                "stream-json".into(),
                "--tools".into(),
                "".into(),
            ],
            env: vec![("CLAUDE_CODE_ENTRYPOINT".into(), "agentkit".into())],
            cwd: PathBuf::from("/tmp"),
            per_run_dir: PathBuf::from("/tmp/agentkit-native-lane-test"),
            permission_policy: PermissionPromptPolicy::None,
            max_vendor_turns: Some(1),
            native_session_id: Some(SESSION.into()),
            fingerprint: "native-lane-test-fingerprint".into(),
        })
    }

    fn encode_input(&self, input: &TurnInput) -> Result<Vec<u8>, NativeRefusal> {
        let mut line = json!({"type": "user", "text": input.text});
        if !input.context_blocks.is_empty() {
            line["context"] = serde_json::to_value(&input.context_blocks).unwrap();
        }
        Ok(format!("{line}\n").into_bytes())
    }

    fn new_frame_adapter(&self) -> Box<dyn StreamFrameAdapter> {
        Box::new(ScriptedAdapter::default())
    }

    fn terminal_outcome(
        &self,
        exit: &ProcessExit,
        _observed: &[NativeEvent],
        _partial_text: Option<&str>,
    ) -> NativeTerminalOutcome {
        NativeTerminalOutcome::Uncertain {
            class: agentkit_native::FailureClass::ExitWithoutResult,
            detail: format!("scripted backend helper (exit {:?})", exit.exit_code),
            partial_text: None,
        }
    }
}

#[async_trait]
impl ProcessSupervisor for Scripted {
    async fn spawn(
        &self,
        _plan: &LaunchPlan,
        _limits: &ResourceLimits,
        _cancellation: &CancellationToken,
    ) -> Result<Box<dyn OwnedChildHandle>, NativeError> {
        Ok(Box::new(ScriptedChild {
            frames: self.frames.clone(),
            exited: false,
        }))
    }

    async fn drain_owned(&self, _policy: InterruptPolicy) -> Vec<InterruptReceipt> {
        vec![]
    }

    async fn terminate_owned(
        &self,
        _pid: u32,
        _generation: u64,
        _policy: InterruptPolicy,
    ) -> Option<InterruptReceipt> {
        None
    }
}

struct ScriptedLocator;

#[async_trait]
impl ExecutableLocator for ScriptedLocator {
    async fn locate(&self) -> Result<ExecutableEvidence, NativeRefusal> {
        Ok(ExecutableEvidence {
            path: PathBuf::from("/usr/bin/claude"),
            version: "2.1.310".into(),
            digest: Some("sha256:test".into()),
        })
    }

    async fn auth_status(&self) -> Result<AuthEvidence, NativeRefusal> {
        Ok(AuthEvidence {
            mode: AuthMode::Subscription {
                account_scope: "claude-pro (test)".into(),
            },
            policy: PolicyState::None,
            config_directory: None,
        })
    }
}

// ----- tests --------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn consumer_kind_with_native_lane_runs_the_scripted_backend() {
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteStore::open(dir.path().join("store.sqlite"), 8).expect("open store");
    let scripted = Scripted::default();
    scripted.emit(&json!({"e": "init", "s": SESSION}));
    scripted.emit(&json!({"e": "delta", "t": "Hello"}));
    scripted.emit(&json!({"e": "delta", "t": ", AgentKit."}));
    scripted.emit(&json!({"e": "result", "r": "Hello, AgentKit."}));
    let planned = Arc::new(AtomicBool::new(false));

    let mut runtime = AgentRuntime::builder(store.clone())
        .default_provider(Arc::new(Fake("fake-native-lane")))
        .turn_kinds(vec![KIND.into()])
        .native_lane(NativeLane {
            backend: Arc::new(ScriptedBackend {
                planned: Arc::clone(&planned),
            }),
            supervisor: Arc::new(scripted.clone()),
            locator: Arc::new(ScriptedLocator),
            broker_factory: None,
            options: NativeExecutorOptions::default(),
        })
        .build()
        .await
        .expect("build runtime with native lane");
    // Deterministic driving: park the background worker, then pump by hand.
    if let Some(worker) = runtime.take_worker() {
        worker.shutdown().await.expect("shutdown worker");
    }

    runtime
        .host()
        .submit(agentkit_engine::SubmitTask {
            task_id: "native-turn-1".into(),
            kind: KIND.into(),
            scope_id: "conversation-1".into(),
            payload: json!({
                EXECUTION_TARGET_PAYLOAD_KEY: { "native": serde_json::to_value(target()).unwrap() },
                TURN_INPUT_PAYLOAD_KEY: serde_json::to_value(turn_input()).unwrap(),
            }),
            priority: 0,
            now: 1,
            available_at: None,
            depends_on: vec![],
            parent_task_id: None,
            exclusive_scope: false,
        })
        .await
        .expect("admitted under the consumer kind");

    for _ in 0..50 {
        runtime.host().run_once().await.expect("pump");
        let task = store
            .get_task("native-turn-1")
            .await
            .unwrap()
            .expect("task exists");
        if task.status == TaskStatus::Completed {
            assert!(
                planned.load(Ordering::SeqCst),
                "the scripted backend must have been called"
            );
            let binding = store
                .current_native_binding("native-turn-1")
                .await
                .unwrap()
                .expect("vendor session bound");
            assert_eq!(binding.vendor_session_id, SESSION);
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("native turn did not complete");
}

#[tokio::test(flavor = "multi_thread")]
async fn registering_an_executor_for_an_owned_kind_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteStore::open(dir.path().join("store.sqlite"), 8).expect("open store");
    let duplicate: Arc<dyn agentkit_engine::TaskExecutor<SqliteStore>> = Arc::new(NoopExecutor);
    let error = match AgentRuntime::builder(store)
        .default_provider(Arc::new(Fake("fake-dup")))
        .executor("chat.turn", duplicate)
        .build()
        .await
    {
        Ok(_) => panic!("chat.turn is owned by the runtime"),
        Err(error) => error,
    };
    assert!(
        error.to_string().contains("registered twice"),
        "unexpected error: {error}"
    );
}

struct NoopExecutor;

#[async_trait]
impl agentkit_engine::TaskExecutor<SqliteStore> for NoopExecutor {
    async fn execute(
        &self,
        _context: agentkit_engine::ExecutionContext<SqliteStore>,
    ) -> agentkit_engine::EngineResult<(), agentkit_store::StoreError> {
        Ok(())
    }
}
