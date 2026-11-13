//! Scripted ports for the native pipeline and a scripted chat provider
//! (ADR 0021 R10: one fake per port, shared by every consumer of it).
//!
//! The native ports stand in for the official vendor CLI: a
//! [`ScriptedSupervisor`] hands out a [`ScriptedChild`] that replays the
//! frames queued with [`ScriptedSupervisor::emit`], a [`ScriptedBackend`]
//! plans a PlainChat launch of that child, and a [`ScriptedLocator`]
//! reports an installed executable and a subscription login. Together they
//! drive the real executor, the real dispatch wrapper and the real durable
//! store — only the process is a script.
//!
//! The miniature NDJSON dialect the scripted child replays:
//! `{"e":"init","s":..} {"e":"delta","t":..} {"e":"result","r":..}`.

use agentkit_core::{ChatProvider, ChatRequest, ProviderEvent, ProviderStream};
use agentkit_native::{
    AuthEvidence, AuthMode, BillingInclusion, CapabilitySnapshot, CapabilityState, ContextBlock,
    EXECUTION_TARGET_SCHEMA_VERSION, ExecutableEvidence, ExecutableLocator, FailureClass,
    FinishDisposition, FrameLine, InterruptPolicy, InterruptReceipt, LaunchPlan, McpEndpointSpec,
    NativeBackend, NativeError, NativeEvent, NativeExecutionTarget, NativeProfile, NativeRefusal,
    NativeTerminalOutcome, NativeUsage, OwnedChildHandle, PermissionPromptPolicy, PolicyState,
    PreflightReport, ProcessExit, ProcessSupervisor, ProtocolViolation, ResourceLimits,
    StreamFrameAdapter, TokenSet, TransportFault, TurnInput, TurnResume, UsageProvenance,
};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;

/// The backend id the scripted ports claim.
pub const SCRIPTED_BACKEND_ID: &str = "claude-code";
/// The qualified version the scripted executable and snapshot report.
pub const SCRIPTED_VERSION: &str = "2.1.310";
/// The model the scripted child reports observing.
pub const SCRIPTED_OBSERVED_MODEL: &str = "claude-sonnet";

/// A capability flag the scripted backend supports.
pub fn supported() -> CapabilityState {
    CapabilityState::Supported {
        qualified_version: SCRIPTED_VERSION.into(),
        evidence: "scripted fixture".into(),
    }
}

/// A capability flag the scripted backend does not support.
pub fn unsupported(reason: &str) -> CapabilityState {
    CapabilityState::Unsupported {
        reason: reason.into(),
    }
}

/// A frozen admission snapshot for the PlainChat profile: text input and
/// plain chat are supported, the profiles that would use native coding
/// tools are explicitly unsupported, so a scripted turn can never be routed
/// through them by accident.
pub fn plain_chat_target(config_identity: &str) -> NativeExecutionTarget {
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
        backend: SCRIPTED_BACKEND_ID.into(),
        backend_version: Some(SCRIPTED_VERSION.into()),
        profile: NativeProfile::PlainChat,
        executable: "/usr/bin/claude".into(),
        requested_model: Some("sonnet".into()),
        requested_effort: None,
        auth_expectation: agentkit_native::AuthExpectation::SubscriptionLogin,
        capabilities: CapabilitySnapshot {
            backend: SCRIPTED_BACKEND_ID.into(),
            matrix_version: 1,
            qualified_version: SCRIPTED_VERSION.into(),
            flags,
        },
        config_identity: config_identity.into(),
        system_prompt_revision: None,
        content: agentkit_native::ContentPolicy::default(),
    }
}

/// One PlainChat turn input with a single selected context block.
pub fn scripted_turn_input() -> TurnInput {
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

/// The supervisor of the scripted child: every child replays the frames
/// queued here, in order, then reports a clean exit.
#[derive(Clone, Default)]
pub struct ScriptedSupervisor {
    frames: Arc<Mutex<VecDeque<FrameLine>>>,
}

impl ScriptedSupervisor {
    /// Queue one frame line of the scripted NDJSON dialect.
    pub fn emit(&self, value: &Value) {
        self.emit_frame(FrameLine::Line(value.to_string().into_bytes()));
    }

    /// Queue one raw frame line.
    pub fn emit_frame(&self, frame: FrameLine) {
        self.frames.lock().unwrap().push_back(frame);
    }
}

/// The scripted child process: frames drain from the supervisor's queue,
/// input is swallowed, every interrupt succeeds, exit is always clean.
pub struct ScriptedChild {
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

#[async_trait]
impl ProcessSupervisor for ScriptedSupervisor {
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

/// Decodes the scripted NDJSON dialect into normalized native events. The
/// session id the init frame reports is remembered; without one the child's
/// launch session (`session`) stands in.
#[derive(Clone)]
pub struct ScriptedAdapter {
    default_session: String,
    text: Arc<Mutex<String>>,
    final_text: Arc<Mutex<Option<String>>>,
    session: Arc<Mutex<Option<String>>>,
}

impl ScriptedAdapter {
    pub fn new(default_session: impl Into<String>) -> Self {
        Self {
            default_session: default_session.into(),
            text: Arc::new(Mutex::new(String::new())),
            final_text: Arc::new(Mutex::new(None)),
            session: Arc::new(Mutex::new(None)),
        }
    }
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
                    observed_model: Some(SCRIPTED_OBSERVED_MODEL.into()),
                    observed_effort: None,
                    observed_version: Some(SCRIPTED_VERSION.into()),
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
        // EOF/exit without a root result can never produce Completed.
        match self.final_text.lock().unwrap().as_deref() {
            Some(text) => NativeTerminalOutcome::Completed {
                text: text.to_string(),
                session_id: self
                    .session
                    .lock()
                    .unwrap()
                    .clone()
                    .unwrap_or_else(|| self.default_session.clone()),
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
                observed_model: Some(SCRIPTED_OBSERVED_MODEL.into()),
                observed_effort: None,
            },
            None => NativeTerminalOutcome::Uncertain {
                class: FailureClass::ExitWithoutResult,
                detail: "scripted child ended without a result frame".into(),
                partial_text: None,
            },
        }
    }
}

/// Plans the scripted PlainChat launch and hands out the decoder.
pub struct ScriptedBackend {
    session: String,
    per_run_dir: PathBuf,
    fingerprint: String,
    planned: Arc<AtomicBool>,
}

impl ScriptedBackend {
    /// A backend whose child reports the given vendor session id.
    pub fn new(session_id: impl Into<String>) -> Self {
        Self {
            session: session_id.into(),
            per_run_dir: PathBuf::from("/tmp/agentkit-scripted-run"),
            fingerprint: "scripted-fingerprint".into(),
            planned: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Override the per-run directory the launch plan names.
    pub fn with_run_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.per_run_dir = dir.into();
        self
    }

    /// Override the launch fingerprint the plan carries.
    pub fn with_fingerprint(mut self, fingerprint: impl Into<String>) -> Self {
        self.fingerprint = fingerprint.into();
        self
    }

    /// Whether `plan_turn` has run — i.e. the scripted child was really
    /// launched through this backend.
    pub fn planned(&self) -> bool {
        self.planned.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl NativeBackend for ScriptedBackend {
    fn backend_id(&self) -> &str {
        SCRIPTED_BACKEND_ID
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
            per_run_dir: self.per_run_dir.clone(),
            permission_policy: PermissionPromptPolicy::None,
            max_vendor_turns: Some(1),
            native_session_id: Some(self.session.clone()),
            fingerprint: self.fingerprint.clone(),
        })
    }

    fn encode_input(&self, input: &TurnInput) -> Result<Vec<u8>, NativeRefusal> {
        let mut line = json!({"type": "user", "text": input.text});
        if !input.context_blocks.is_empty() {
            line["context"] = serde_json::to_value(&input.context_blocks).expect("serializable");
        }
        Ok(format!("{line}\n").into_bytes())
    }

    fn new_frame_adapter(&self) -> Box<dyn StreamFrameAdapter> {
        Box::new(ScriptedAdapter::new(self.session.clone()))
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
            class: FailureClass::ExitWithoutResult,
            detail: format!("scripted backend helper (exit {:?})", exit.exit_code),
            partial_text: None,
        }
    }
}

/// Reports an installed executable and a subscription login, so preflight
/// has evidence without a real CLI.
#[derive(Clone)]
pub struct ScriptedLocator {
    digest: String,
    account_scope: String,
}

impl Default for ScriptedLocator {
    fn default() -> Self {
        Self {
            digest: "sha256:scripted".into(),
            account_scope: "claude-pro (scripted)".into(),
        }
    }
}

impl ScriptedLocator {
    /// Override the reported executable digest and account scope.
    pub fn with_evidence(
        mut self,
        digest: impl Into<String>,
        account_scope: impl Into<String>,
    ) -> Self {
        self.digest = digest.into();
        self.account_scope = account_scope.into();
        self
    }
}

#[async_trait]
impl ExecutableLocator for ScriptedLocator {
    async fn locate(&self) -> Result<ExecutableEvidence, NativeRefusal> {
        Ok(ExecutableEvidence {
            path: PathBuf::from("/usr/bin/claude"),
            version: SCRIPTED_VERSION.into(),
            digest: Some(self.digest.clone()),
        })
    }

    async fn auth_status(&self) -> Result<AuthEvidence, NativeRefusal> {
        Ok(AuthEvidence {
            mode: AuthMode::Subscription {
                account_scope: self.account_scope.clone(),
            },
            policy: PolicyState::None,
            config_directory: None,
        })
    }
}

/// A [`ChatProvider`] that replays a fixed event script, so a composition
/// root can be built — or a turn run — with no network and no account.
pub struct ScriptedChatProvider {
    id: String,
    events: Vec<ProviderEvent>,
}

impl ScriptedChatProvider {
    /// Replay `events` for every request.
    pub fn new(id: impl Into<String>, events: Vec<ProviderEvent>) -> Self {
        Self {
            id: id.into(),
            events,
        }
    }

    /// Complete every request with `text` as the whole assistant reply.
    pub fn completing(id: impl Into<String>, text: impl Into<String>) -> Self {
        Self::new(
            id,
            vec![ProviderEvent::Completed {
                content: text.into(),
                tool_calls: vec![],
                finish_reason: "stop".into(),
            }],
        )
    }
}

impl ChatProvider for ScriptedChatProvider {
    fn id(&self) -> &str {
        &self.id
    }

    fn stream_chat(&self, _input: ChatRequest, _cancellation: CancellationToken) -> ProviderStream {
        Box::pin(futures::stream::iter(
            self.events.clone().into_iter().map(Ok),
        ))
    }
}
