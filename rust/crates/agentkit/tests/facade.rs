//! Facade plumbing tests: the real Claude backend + scripted locator +
//! scripted in-process supervisor exercise `chat_turn`'s drive loop without
//! any OS process or durable store (AGENTKIT-117).

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

use async_trait::async_trait;

use agentkit_rust::native_chat::{
    AuthEvidence, AuthExpectation, AuthMode, ContentPolicy, EXECUTION_TARGET_SCHEMA_VERSION,
    ExecutableEvidence, ExecutableLocator, FacadeError, FrameLine, InterruptPolicy,
    InterruptReceipt, LaunchPlan, NativeChat, NativeChatOptions, NativeExecutionTarget,
    NativeProfile, NativeRefusal, NativeTerminalOutcome, OwnedChildHandle, PolicyState,
    ProcessExit, ProcessSupervisor, ResourceLimits, TurnInput, TurnResume,
};

fn fixture_path() -> PathBuf {
    std::env::temp_dir()
        .join(format!("agentkit-facade-test-{}", std::process::id()))
        .join("claude")
}

fn ensure_fixture() {
    let path = fixture_path();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, b"official-cli-fixture").unwrap();
}

struct ScriptedLocator;

#[async_trait]
impl ExecutableLocator for ScriptedLocator {
    async fn locate(&self) -> Result<ExecutableEvidence, NativeRefusal> {
        Ok(ExecutableEvidence {
            path: fixture_path(),
            version: "2.1.310".to_owned(),
            digest: None,
        })
    }

    async fn auth_status(&self) -> Result<AuthEvidence, NativeRefusal> {
        Ok(AuthEvidence {
            mode: AuthMode::Subscription {
                account_scope: "test".to_owned(),
            },
            policy: PolicyState::None,
            config_directory: None,
        })
    }
}

fn plain_target(profile: NativeProfile) -> NativeExecutionTarget {
    NativeExecutionTarget {
        schema_version: EXECUTION_TARGET_SCHEMA_VERSION,
        backend: "claude-code".to_owned(),
        backend_version: Some("2.1.310".to_owned()),
        profile,
        executable: fixture_path().display().to_string(),
        requested_model: None,
        requested_effort: None,
        auth_expectation: AuthExpectation::SubscriptionLogin,
        capabilities: agentkit_claude::ClaudeSupportMatrix::snapshot(Some("2.1.310")),
        config_identity: "facade-test".to_owned(),
        system_prompt_revision: None,
        content: ContentPolicy::default(),
    }
}

fn turn_input(resume: TurnResume, text: &str) -> TurnInput {
    TurnInput {
        input_id: "in-facade-1".to_owned(),
        text: text.to_owned(),
        context_blocks: Vec::new(),
        resume,
        system_prompt_revision: None,
        system_prompt: None,
        max_vendor_turns: None,
        turn_deadline_ms: 10_000,
    }
}

/// Shared observation: what the facade delivered, interrupted and spawned.
#[derive(Default)]
struct Shared {
    delivered: Mutex<Vec<u8>>,
    interrupts: Mutex<Vec<InterruptPolicy>>,
    spawn_calls: AtomicUsize,
    stderr: Mutex<Vec<u8>>,
    cleanup_uncertain: std::sync::atomic::AtomicBool,
}

struct FakeChild {
    frames: VecDeque<FrameLine>,
    hang_after_frames: bool,
    shared: Arc<Shared>,
    exit: ProcessExit,
}

#[async_trait]
impl OwnedChildHandle for FakeChild {
    fn pid(&self) -> u32 {
        self.exit.pid
    }

    fn generation(&self) -> u64 {
        self.exit.generation
    }

    async fn next_frame(&mut self) -> Option<FrameLine> {
        match self.frames.pop_front() {
            Some(frame) => Some(frame),
            None if self.hang_after_frames => std::future::pending().await,
            None => None,
        }
    }

    fn stderr_tail(&self) -> Vec<u8> {
        self.shared.stderr.lock().unwrap().clone()
    }

    async fn deliver_input(
        &mut self,
        bytes: &[u8],
        _close_stdin: bool,
    ) -> Result<(), agentkit_rust::native_chat::TransportFault> {
        self.shared
            .delivered
            .lock()
            .unwrap()
            .extend_from_slice(bytes);
        Ok(())
    }

    async fn interrupt(&mut self, policy: InterruptPolicy) -> InterruptReceipt {
        self.shared.interrupts.lock().unwrap().push(policy);
        InterruptReceipt {
            terminated: true,
            escalated_to_sigterm: false,
            escalated_to_sigkill: false,
            cleanup_uncertain: false,
            detail: None,
        }
    }

    async fn wait(&mut self) -> ProcessExit {
        self.exit.clone()
    }
}

struct FakeSupervisor {
    frames: Vec<FrameLine>,
    hang_after_frames: bool,
    exit: ProcessExit,
    shared: Arc<Shared>,
}

impl FakeSupervisor {
    fn new(frames: Vec<&str>, hang_after_frames: bool) -> Self {
        FakeSupervisor {
            frames: frames
                .into_iter()
                .map(|f| FrameLine::Line(f.as_bytes().to_vec()))
                .collect(),
            hang_after_frames,
            exit: ProcessExit {
                pid: 4242,
                generation: 7,
                exit_code: Some(0),
                signal: None,
                after_interrupt: false,
            },
            shared: Arc::new(Shared::default()),
        }
    }
}

#[async_trait]
impl ProcessSupervisor for FakeSupervisor {
    async fn spawn(
        &self,
        _plan: &LaunchPlan,
        _limits: &ResourceLimits,
        _cancellation: &agentkit_rust::native_chat::CancellationToken,
    ) -> Result<Box<dyn OwnedChildHandle>, agentkit_rust::native_chat::NativeError> {
        self.shared.spawn_calls.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(FakeChild {
            frames: self.frames.iter().cloned().collect(),
            hang_after_frames: self.hang_after_frames,
            shared: Arc::clone(&self.shared),
            exit: self.exit.clone(),
        }))
    }

    fn descendant_cleanup(&self, _pid: u32, _generation: u64) -> Option<InterruptReceipt> {
        self.shared
            .cleanup_uncertain
            .load(Ordering::SeqCst)
            .then(|| InterruptReceipt {
                terminated: true,
                escalated_to_sigterm: false,
                escalated_to_sigkill: false,
                cleanup_uncertain: true,
                detail: Some("injected unobservable descendants".into()),
            })
    }

    async fn drain_owned(&self, _policy: InterruptPolicy) -> Vec<InterruptReceipt> {
        Vec::new()
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

fn fast_limits() -> ResourceLimits {
    ResourceLimits {
        startup_timeout_ms: 40,
        idle_timeout_ms: 40,
        interrupt_grace_ms: 5,
        terminate_grace_ms: 5,
        cleanup_timeout_ms: 50,
        ..ResourceLimits::default()
    }
}

fn chat(run_root: &std::path::Path, supervisor: FakeSupervisor) -> NativeChat {
    NativeChat::new(Some(fixture_path()), run_root)
        .with_locator(Arc::new(ScriptedLocator))
        .with_supervisor(Arc::new(supervisor))
}

#[tokio::test]
async fn chat_turn_plain_chat_completes_via_adapter() {
    ensure_fixture();
    let tmp = tempfile::tempdir().unwrap();
    let supervisor = FakeSupervisor::new(
        vec![
            r#"{"type":"system","subtype":"init","session_id":"sess-fx","model":"sonnet"}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"hi there"}]}}"#,
            r#"{"type":"result","subtype":"success","is_error":false,"result":"hi there","session_id":"sess-fx"}"#,
        ],
        false,
    );
    let shared = Arc::clone(&supervisor.shared);
    let subject = chat(tmp.path(), supervisor);

    let outcome = subject
        .chat_turn(
            &plain_target(NativeProfile::PlainChat),
            &turn_input(
                TurnResume::Exact {
                    vendor_session_id: "sess-fx".to_owned(),
                },
                "hello facade",
            ),
        )
        .await
        .expect("turn settles");

    match outcome {
        NativeTerminalOutcome::Completed {
            text, session_id, ..
        } => {
            assert_eq!(text, "hi there");
            assert_eq!(session_id, "sess-fx");
        }
        other => panic!("expected Completed, got {other:?}"),
    }

    // The admitted prompt reached stdin, framed by the driver's encoder.
    let delivered = shared.delivered.lock().unwrap();
    assert!(!delivered.is_empty());
    assert!(
        String::from_utf8_lossy(&delivered).contains("hello facade"),
        "stdin payload missing the admitted text"
    );
    // The facade settles the private run directory it staged; only the
    // stable shared `cwd` (never removed per attempt) remains.
    let leftover: Vec<_> = std::fs::read_dir(tmp.path())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(leftover, vec![std::ffi::OsString::from("cwd")]);
    assert_eq!(shared.spawn_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn completed_answer_exposes_supervisor_cleanup_uncertainty() {
    ensure_fixture();
    let tmp = tempfile::tempdir().unwrap();
    let supervisor = FakeSupervisor::new(
        vec![
            r#"{"type":"system","subtype":"init","session_id":"sess-fx","model":"sonnet"}"#,
            r#"{"type":"result","subtype":"success","is_error":false,"result":"VISIBLE","session_id":"sess-fx"}"#,
        ],
        false,
    );
    supervisor
        .shared
        .cleanup_uncertain
        .store(true, Ordering::SeqCst);
    let outcome = chat(tmp.path(), supervisor)
        .chat_turn(
            &plain_target(NativeProfile::PlainChat),
            &turn_input(
                TurnResume::Exact {
                    vendor_session_id: "sess-fx".to_owned(),
                },
                "hello",
            ),
        )
        .await
        .unwrap();
    assert!(matches!(outcome, NativeTerminalOutcome::Uncertain {
        class: agentkit_native::FailureClass::CleanupUncertain,
        partial_text: Some(text), ..
    } if text == "VISIBLE"));
}

/// Fix C: a CLI that dies before any frame explains itself on stderr; the
/// facade surfaces that tail in the exit-derived failure.
#[tokio::test]
async fn early_exit_failure_carries_the_stderr_tail() {
    ensure_fixture();
    let tmp = tempfile::tempdir().unwrap();
    let mut supervisor = FakeSupervisor::new(vec![], false);
    supervisor.exit.exit_code = Some(1);
    *supervisor.shared.stderr.lock().unwrap() =
        b"\x1b[31mError: Invalid session ID. Must be a valid UUID.\x1b[0m\n".to_vec();
    let subject = chat(tmp.path(), supervisor);
    let outcome = subject
        .chat_turn(
            &plain_target(NativeProfile::PlainChat),
            &turn_input(
                TurnResume::Fresh {
                    seed_history: Vec::new(),
                },
                "hello",
            ),
        )
        .await
        .expect("turn settles");
    let text = match outcome {
        NativeTerminalOutcome::Failed {
            class: agentkit_native::FailureClass::ExitWithoutResult,
            message,
            ..
        } => message,
        NativeTerminalOutcome::Uncertain {
            class: agentkit_native::FailureClass::ExitWithoutResult,
            detail,
            ..
        } => detail,
        other => panic!("expected an exit-derived failure, got {other:?}"),
    };
    assert!(text.contains("exit_code=Some(1)"), "{text}");
    assert!(
        text.contains("; stderr: [31mError: Invalid session ID. Must be a valid UUID.[0m"),
        "{text}"
    );
}

#[tokio::test]
async fn chat_turn_refuses_owned_mcp_profile_without_touching_ports() {
    ensure_fixture();
    let tmp = tempfile::tempdir().unwrap();
    let supervisor = FakeSupervisor::new(vec![], false);
    let shared = Arc::clone(&supervisor.shared);
    let subject = chat(tmp.path(), supervisor);

    let err = subject
        .chat_turn(
            &plain_target(NativeProfile::OwnedMcpChat),
            &turn_input(
                TurnResume::Fresh {
                    seed_history: Vec::new(),
                },
                "hello",
            ),
        )
        .await
        .expect_err("OwnedMcpChat must be refused");
    assert_eq!(err.code(), "facade_profile_unsupported");
    assert!(!err.retryable());
    assert!(matches!(err, FacadeError::Refusal { .. }));
    assert_eq!(shared.spawn_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn chat_turn_refuses_before_spawn_on_preflight_failure() {
    ensure_fixture();
    let tmp = tempfile::tempdir().unwrap();
    let supervisor = FakeSupervisor::new(vec![], false);
    let shared = Arc::clone(&supervisor.shared);
    let subject = chat(tmp.path(), supervisor);

    let mut target = plain_target(NativeProfile::PlainChat);
    target.backend = "codex".to_owned();
    let err = subject
        .chat_turn(
            &target,
            &turn_input(
                TurnResume::Fresh {
                    seed_history: Vec::new(),
                },
                "hello",
            ),
        )
        .await
        .expect_err("foreign backend must be refused");
    assert_eq!(err.code(), "backend_mismatch");
    assert!(matches!(err, FacadeError::Refusal { .. }));
    assert_eq!(shared.spawn_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn chat_turn_startup_timeout_interrupts_child() {
    ensure_fixture();
    let tmp = tempfile::tempdir().unwrap();
    let supervisor = FakeSupervisor::new(vec![], true); // never emits, never EOFs
    let shared = Arc::clone(&supervisor.shared);
    let subject = chat(tmp.path(), supervisor).with_options(NativeChatOptions {
        limits: fast_limits(),
        cancellation: Default::default(),
    });

    let outcome = subject
        .chat_turn(
            &plain_target(NativeProfile::PlainChat),
            &turn_input(
                TurnResume::Fresh {
                    seed_history: Vec::new(),
                },
                "silence",
            ),
        )
        .await
        .expect("timeout is an outcome, not an error");

    match outcome {
        NativeTerminalOutcome::Failed { class, message, .. } => {
            assert_eq!(
                class,
                agentkit_rust::native_chat::FailureClass::StartupTimeout
            );
            assert!(message.contains("budget"), "{message}");
        }
        other => panic!("expected Failed(StartupTimeout), got {other:?}"),
    }
    assert_eq!(
        *shared.interrupts.lock().unwrap(),
        vec![InterruptPolicy::UserStop]
    );
}

#[tokio::test]
async fn chat_turn_reports_protocol_violation_as_failed() {
    ensure_fixture();
    let tmp = tempfile::tempdir().unwrap();
    let supervisor = FakeSupervisor::new(vec!["this is not json"], false);
    let shared = Arc::clone(&supervisor.shared);
    let subject = chat(tmp.path(), supervisor).with_options(NativeChatOptions {
        limits: fast_limits(),
        cancellation: Default::default(),
    });

    let outcome = subject
        .chat_turn(
            &plain_target(NativeProfile::PlainChat),
            &turn_input(
                TurnResume::Fresh {
                    seed_history: Vec::new(),
                },
                "garbage stream",
            ),
        )
        .await
        .expect("violation is an outcome, not an error");

    match outcome {
        NativeTerminalOutcome::Failed { class, message, .. } => {
            assert_eq!(
                class,
                agentkit_rust::native_chat::FailureClass::ProtocolViolation
            );
            assert!(message.contains("malformed_json"), "{message}");
        }
        other => panic!("expected Failed(ProtocolViolation), got {other:?}"),
    }
    assert_eq!(
        *shared.interrupts.lock().unwrap(),
        vec![InterruptPolicy::Quarantine]
    );
}

#[tokio::test]
async fn chat_turn_refuses_frozen_session_identity_mismatch() {
    ensure_fixture();
    let tmp = tempfile::tempdir().unwrap();
    // Fresh resume: the plan preassigns a session id; an init frame naming
    // a different session contradicts the frozen identity.
    let supervisor = FakeSupervisor::new(
        vec![r#"{"type":"system","subtype":"init","session_id":"sess-other"}"#],
        false,
    );
    let shared = Arc::clone(&supervisor.shared);
    let subject = chat(tmp.path(), supervisor).with_options(NativeChatOptions {
        limits: fast_limits(),
        cancellation: Default::default(),
    });

    let err = subject
        .chat_turn(
            &plain_target(NativeProfile::PlainChat),
            &turn_input(
                TurnResume::Fresh {
                    seed_history: Vec::new(),
                },
                "wrong session",
            ),
        )
        .await
        .expect_err("identity drift must be an explicit refusal");
    assert_eq!(err.code(), "session_identity_mismatch");
    assert!(matches!(err, FacadeError::Protocol { .. }));
    assert_eq!(
        *shared.interrupts.lock().unwrap(),
        vec![InterruptPolicy::Quarantine]
    );
}

#[tokio::test]
async fn chat_turn_cancellation_interrupts_and_reports_cancelled() {
    ensure_fixture();
    let tmp = tempfile::tempdir().unwrap();
    let supervisor = FakeSupervisor::new(vec![], true); // hangs: cancel mid-stream
    let shared = Arc::clone(&supervisor.shared);
    let cancellation = agentkit_rust::native_chat::CancellationToken::new();
    let subject = chat(tmp.path(), supervisor).with_options(NativeChatOptions {
        limits: ResourceLimits {
            startup_timeout_ms: 5_000,
            idle_timeout_ms: 5_000,
            interrupt_grace_ms: 5,
            terminate_grace_ms: 5,
            cleanup_timeout_ms: 50,
            ..ResourceLimits::default()
        },
        cancellation: cancellation.clone(),
    });

    let canceller = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        cancellation.cancel();
    });

    let err = subject
        .chat_turn(
            &plain_target(NativeProfile::PlainChat),
            &turn_input(
                TurnResume::Fresh {
                    seed_history: Vec::new(),
                },
                "never answers",
            ),
        )
        .await
        .expect_err("cancellation is an error, never a completion");
    canceller.await.unwrap();

    assert!(matches!(err, FacadeError::Cancelled));
    assert!(err.retryable());
    assert_eq!(
        *shared.interrupts.lock().unwrap(),
        vec![InterruptPolicy::HostShutdown]
    );
}
