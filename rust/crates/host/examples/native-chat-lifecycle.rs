//! Standalone Claude Code backend lifecycle proof (AGENTKIT-118 / Page §8.1).
//!
//! Runs the *generic* native pipeline end to end — durable admission →
//! claim → preflight/plan → owned spawn → normalized events → authoritative
//! terminal outcome → durable session lineage — without the real `claude`
//! executable by supplying scripted in-process ports. The same wiring drives
//! the real driver (`agentkit_claude::default_backend()` /
//! `ClaudeCliBackend` + `ClaudeLocator`) when an official executable is
//! installed; only the ports differ.
//!
//! ```text
//! cargo run -p agentkit-host --example native-chat-lifecycle
//! ```

use agentkit_host::native_executor::{
    EXECUTION_TARGET_PAYLOAD_KEY, NativeAgentExecutor, NativeExecutorOptions,
    TURN_INPUT_PAYLOAD_KEY,
};
use agentkit_host::{AgentHost, HostResult, Runtime, SystemRuntime, TaskExecutor};
use agentkit_native::{
    AuthEvidence, AuthMode, BillingInclusion, CapabilitySnapshot, CapabilityState, ContextBlock,
    ExecutableEvidence, ExecutableLocator, FinishDisposition, FrameLine, InterruptPolicy,
    InterruptReceipt, LaunchPlan, McpEndpointSpec, NativeBackend, NativeError, NativeEvent,
    NativeExecutionTarget, NativeProfile, NativeRefusal, NativeTerminalOutcome, NativeUsage,
    OwnedChildHandle, PermissionPromptPolicy, PolicyState, PreflightReport, ProcessExit,
    ProcessSupervisor, ProtocolViolation, ResourceLimits, StreamFrameAdapter, TokenSet,
    TransportFault, TurnInput, TurnResume, UsageProvenance,
};
use agentkit_store::{SqliteStore, SubmitTask};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;

const BACKEND: &str = "claude-code";
const SESSION: &str = "sess-example-1";

fn supported() -> CapabilityState {
    CapabilityState::Supported {
        qualified_version: "2.1.310".into(),
        evidence: "example fixture".into(),
    }
}

fn unsupported(reason: &str) -> CapabilityState {
    CapabilityState::Unsupported {
        reason: reason.into(),
    }
}

/// Frozen admission snapshot: PlainChat only; the profile that would use
/// native coding tools is explicitly unsupported, so the standalone proof
/// cannot accidentally route through them.
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
        schema_version: agentkit_native::EXECUTION_TARGET_SCHEMA_VERSION,
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
        config_identity: "example-config".into(),
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

// ----- scripted ports (stand in for the official CLI) --------------------

/// The miniature NDJSON dialect the scripted child emits:
/// `{"e":"init","s":..} {"e":"delta","t":..} {"e":"result","r":..}`.
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
        // EOF/exit without a root result can never produce Completed.
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

struct ScriptedBackend;

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
            per_run_dir: PathBuf::from("/tmp/agentkit-example-run"),
            permission_policy: PermissionPromptPolicy::None,
            max_vendor_turns: Some(1),
            native_session_id: Some(SESSION.into()),
            fingerprint: "example-fingerprint".into(),
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
        // The executor asks the per-attempt adapter; this stateless helper
        // mirrors the fail-closed default.
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
}

struct ScriptedLocator;

#[async_trait]
impl ExecutableLocator for ScriptedLocator {
    async fn locate(&self) -> Result<ExecutableEvidence, NativeRefusal> {
        Ok(ExecutableEvidence {
            path: PathBuf::from("/usr/bin/claude"),
            version: "2.1.310".into(),
            digest: Some("sha256:example".into()),
        })
    }

    async fn auth_status(&self) -> Result<AuthEvidence, NativeRefusal> {
        Ok(AuthEvidence {
            mode: AuthMode::Subscription {
                account_scope: "claude-pro (example)".into(),
            },
            policy: PolicyState::None,
            config_directory: None,
        })
    }
}

// ----- lifecycle ----------------------------------------------------------

#[tokio::main]
async fn main() -> HostResult<()> {
    let store = SqliteStore::memory(64).expect("in-memory store");
    let runtime: Arc<dyn Runtime> = Arc::new(SystemRuntime);

    let payload = json!({
        EXECUTION_TARGET_PAYLOAD_KEY: { "native": serde_json::to_value(target()).unwrap() },
        TURN_INPUT_PAYLOAD_KEY: serde_json::to_value(turn_input()).unwrap(),
    });

    store
        .submit(SubmitTask {
            task_id: "native-turn-1".into(),
            kind: "native.turn".into(),
            scope_id: "conversation-1".into(),
            payload,
            priority: 0,
            now: runtime.now_ms(),
            available_at: None,
            depends_on: vec![],
            parent_task_id: None,
            exclusive_scope: false,
        })
        .await
        .expect("admitted");

    let scripted = Scripted::default();
    scripted.emit(&json!({"e": "init", "s": SESSION}));
    scripted.emit(&json!({"e": "delta", "t": "Hello"}));
    scripted.emit(&json!({"e": "delta", "t": ", AgentKit."}));
    scripted.emit(&json!({"e": "result", "r": "Hello, AgentKit."}));

    let executor = Arc::new(NativeAgentExecutor::new(
        store.clone(),
        Arc::new(ScriptedBackend),
        Arc::new(scripted.clone()),
        Arc::new(ScriptedLocator),
        None,
        NativeExecutorOptions::default(),
    ));
    assert!(
        NativeAgentExecutor::capability_verdict(
            &NativeAgentExecutor::parse_target(
                &store
                    .get_task("native-turn-1")
                    .await
                    .unwrap()
                    .expect("task")
                    .payload
            )
            .unwrap()
        )
        .is_ok()
    );

    // Durable dispatch: the same host loop any consumer runs — the executor
    // only ever sees a fenced ExecutionContext built by the runner.
    let mut executors = std::collections::HashMap::new();
    executors.insert("native.turn".to_string(), executor as Arc<dyn TaskExecutor>);
    let host = AgentHost::new(
        store.clone(),
        executors,
        runtime.clone(),
        agentkit_host::HostOptions::default(),
    )?;
    // Fresh store: nothing to recover (manual-mode `recover` interrupts
    // queued tasks by design — it is a crash-recovery entry point, not a
    // pump). Drive the durable loop directly.
    for _ in 0..50 {
        if host.run_once().await.expect("pump") {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }

    let events = store.events("native-turn-1", None, 1000).await.unwrap();
    for event in &events {
        let kind = event
            .event
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("event");
        println!("event {:>3.0}: {kind}", event.seq);
    }
    let task = store.get_task("native-turn-1").await.unwrap().unwrap();
    println!(
        "task status: {:?} attempts={} error={:?}",
        task.status, task.attempt_count, task.error
    );
    assert_eq!(task.status, agentkit_store::TaskStatus::Completed);
    let binding = store
        .current_native_binding("native-turn-1")
        .await
        .unwrap()
        .expect("vendor session bound and observed");
    println!(
        "lineage: session={} state={} assigned_at={} observed_at={:?}",
        binding.vendor_session_id, binding.state, binding.assigned_at, binding.observed_at
    );
    assert_eq!(binding.vendor_session_id, SESSION);
    assert_eq!(binding.state, "observed");
    println!("standalone native lifecycle: OK");
    Ok(())
}
