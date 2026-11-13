//! Injected ports: what the host's generic `NativeAgentExecutor` asks a
//! driver and a process supervisor to do (AGENTKIT-108; realized by
//! AGENTKIT-110/111/113).
//!
//! The host owns durable governance; the driver owns vendor protocol; the
//! supervisor owns OS process ownership. No port exposes credentials, raw
//! store/lease internals, or coding-pack permissions.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::error::{NativeError, NativeRefusal};
use crate::events::{NativeEvent, NativeTerminalOutcome, ProcessTerminalEvidence};

/// How permission prompts are handled for an unattended run. The delivered
/// general-Chat adapter must not require the private stdio control protocol.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "policy", rename_all = "camelCase", deny_unknown_fields)]
pub enum PermissionPromptPolicy {
    /// Nobody can answer; anything that would prompt is denied by the
    /// runtime (deterministic; used for plain Chat).
    None,
    /// Exact preapproved AgentKit-owned broker tool names may execute
    /// without prompting; everything else is denied. Host-side governance
    /// still gates every actual operation.
    PreapprovedBroker { broker_tool_names: Vec<String> },
}

/// Endpoint of the per-attempt scoped MCP bridge staged by the host
/// (AGENTKIT-114). The token is unrelated to vendor credentials, scoped to
/// one live attempt, and revoked at cancellation or settlement.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct McpEndpointSpec {
    pub server_name: String,
    /// Exact broker tool names Claude may see (names only, no schemas).
    pub broker_tool_names: Vec<String>,
    /// Loopback stdio bridge command (argv) or loopback HTTP URL — one of
    /// the two must be present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stdio_command: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http_url: Option<String>,
    /// Bearer token for the loopback endpoint, unrelated to vendor
    /// credentials. The driver writes it only into the 0600 private
    /// per-run MCP config; it never enters argv, logs or evidence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_token: Option<String>,
}

/// A frozen launch plan produced by the driver for one attempt. Prompts are
/// never embedded in shell command strings: argv is typed (AGENTKIT-110).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LaunchPlan {
    pub argv: Vec<String>,
    /// Additive child-environment entries. Must never contain credential
    /// material; the official configuration namespace is preserved as-is.
    pub env: Vec<(String, String)>,
    /// Controlled application working directory. Stable and shared across
    /// attempts of one backend (vendor session stores key on it); never
    /// removed by per-attempt cleanup.
    pub cwd: std::path::PathBuf,
    /// Private per-run directory owned by this attempt (config/settings/MCP
    /// files); removed on cleanup.
    pub per_run_dir: std::path::PathBuf,
    pub permission_policy: PermissionPromptPolicy,
    /// Bounded turn configuration the driver encoded into argv/env.
    pub max_vendor_turns: Option<u32>,
    /// Vendor session identity assigned by the driver for this attempt
    /// (preassigned for a fresh session, or the resumed id). The host binds
    /// it to the public ancestry before spawn so a crash mid-attempt still
    /// has the identity on record.
    pub native_session_id: Option<String>,
    /// Stable fingerprint over argv+env keys+cwd for evidence (no secrets).
    pub fingerprint: String,
}

impl LaunchPlan {
    /// argv with values that look like tokens replaced by `«redacted»`,
    /// for logs/snapshots/diagnostics (AGENTKIT-109 acceptance).
    pub fn redacted_argv(&self) -> Vec<String> {
        self.argv
            .iter()
            .map(|arg| {
                let lower = arg.to_lowercase();
                if lower.contains("token") || lower.contains("key=") || lower.contains("secret") {
                    "«redacted»".to_owned()
                } else {
                    arg.clone()
                }
            })
            .collect()
    }
}

/// One admitted native turn input (AGENTKIT-113 semantics; contract here).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TurnInput {
    /// Host-owned identity for the admitted input (idempotency anchor).
    pub input_id: String,
    /// New admitted user text. Never a full-transcript replay when a valid
    /// resume exists.
    pub text: String,
    /// Explicit dynamic context blocks (selected source snapshots, newly
    /// added extracted-text attachments) with provenance labels.
    pub context_blocks: Vec<ContextBlock>,
    /// Resume mode.
    pub resume: TurnResume,
    /// Revision of the stable application system prompt in force.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt_revision: Option<String>,
    /// Frozen application system prompt text (stable per revision). The
    /// driver writes it into the private per-run directory and passes it by
    /// file, never inline in a shell string.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
    /// Bound on vendor-internal agentic turns for this admitted input
    /// (foreground tool loops count against it). `None` = driver default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_vendor_turns: Option<u32>,
    /// Overall wall-clock budget for the vendor turn.
    pub turn_deadline_ms: i64,
}

/// A provenance-labelled context block admitted with the turn.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ContextBlock {
    /// e.g. `"user"`, `"selected-source"`, `"attachment"`, `"seed-history"`.
    pub kind: String,
    /// Identity of the underlying content (id + revision/digest), so retries
    /// re-send the same bytes, not newly fetched mutable content.
    pub content_identity: String,
    pub text: String,
}

/// How the vendor conversation continues (or starts). There is deliberately
/// no "continue latest" variant: exact identity or nothing (AGENTKIT-107 §5).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "camelCase", deny_unknown_fields)]
pub enum TurnResume {
    /// Fresh vendor session; the driver assigns the session id if the
    /// backend supports preassignment, otherwise records the observed one.
    Fresh {
        /// Seed ancestry rendered as inert context (AGENTKIT-113). Never
        /// executable tool calls; empty for a first turn.
        #[serde(default)]
        seed_history: Vec<ContextBlock>,
    },
    /// Exact resume of the bound vendor session.
    Exact { vendor_session_id: String },
    /// Qualified fork of the bound vendor session into a new id (only after
    /// exact-parent behavior is qualified; otherwise use `Fresh`).
    Fork {
        vendor_session_id: String,
        /// The exact parent session the fork must descend from.
        expected_parent: String,
    },
}

/// Bounded resources for one native turn (AGENTKIT-110/116).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ResourceLimits {
    /// Maximum bytes in one stdout NDJSON frame.
    pub max_frame_bytes: usize,
    /// Maximum buffered stderr bytes retained (tail) per process.
    pub max_stderr_bytes: usize,
    /// Maximum queued decoded events while the consumer lags.
    pub max_pending_events: usize,
    /// Maximum bytes written to stdin in total.
    pub max_stdin_bytes: usize,
    /// Time allowed for the vendor to emit its first event after input.
    pub startup_timeout_ms: i64,
    /// Time allowed between consecutive events.
    pub idle_timeout_ms: i64,
    /// SIGINT grace before SIGTERM, and SIGTERM grace before SIGKILL.
    pub interrupt_grace_ms: i64,
    pub terminate_grace_ms: i64,
    /// Hard bound on cleanup work per attempt.
    pub cleanup_timeout_ms: i64,
}

impl Default for ResourceLimits {
    fn default() -> Self {
        ResourceLimits {
            max_frame_bytes: 8 * 1024 * 1024,
            max_stderr_bytes: 64 * 1024,
            max_pending_events: 4096,
            max_stdin_bytes: 10 * 1024 * 1024,
            startup_timeout_ms: 90_000,
            idle_timeout_ms: 120_000,
            interrupt_grace_ms: 10_000,
            terminate_grace_ms: 5_000,
            cleanup_timeout_ms: 15_000,
        }
    }
}

/// Interrupt request flavors with distinct recorded reasons
/// (AGENTKIT-116): user stop, host shutdown and lost lease differ.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum InterruptPolicy {
    /// User-requested stop: graceful interrupt first, then bounded kill.
    UserStop,
    /// Host shutdown drain: bounded, records host-shutdown reason.
    HostShutdown,
    /// Lease lost or stale authority: quarantine promptly.
    Quarantine,
}

/// Receipt from an interrupt attempt: did the child terminate within the
/// grace windows, and is any residual work uncertain?
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct InterruptReceipt {
    pub terminated: bool,
    pub escalated_to_sigterm: bool,
    pub escalated_to_sigkill: bool,
    /// True when residual descendants or an unreaped child remain: cleanup
    /// is visible uncertainty, never silence.
    pub cleanup_uncertain: bool,
    pub detail: Option<String>,
}

/// Process exit facts; distinct from turn completion (AGENTKIT-107 §3.5).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProcessExit {
    pub pid: u32,
    pub generation: u64,
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    /// Whether the exit follows an acknowledged graceful interrupt.
    pub after_interrupt: bool,
}

impl ProcessExit {
    pub fn terminal_evidence(&self, saw_root_result: bool) -> ProcessTerminalEvidence {
        ProcessTerminalEvidence {
            exit_code: self.exit_code,
            signal: self.signal,
            saw_root_result,
            interrupt_requested: self.after_interrupt,
        }
    }
}

/// Why a frame/transport failed; `code` is stable.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TransportFault {
    pub code: String,
    pub message: String,
}

/// One framed stdout chunk (AGENTKIT-110: readers start immediately and
/// frame incrementally; EOF is `None` on the stream, not a completion).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FrameLine {
    /// A complete stdout line (NDJSON frame) within bounds.
    Line(Vec<u8>),
    /// A transport-level fault (frame overflow, invalid encoding…).
    Fault(TransportFault),
}

/// Why the frame adapter ended its stream.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FinishDisposition {
    /// Stream ended without ever observing a root result (EOF/exit).
    NoResult { partial_text: Option<String> },
}

/// A protocol violation from the driver's frame adapter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProtocolViolation {
    pub code: String,
    pub message: String,
}

/// Maximum number of stderr bytes kept for failure diagnostics.
pub const STDERR_TAIL_MAX_BYTES: usize = 2048;

/// Turn a child's retained stderr bytes into bounded, display-safe text for
/// failure messages and durable payloads: lossy UTF-8, only the LAST
/// [`STDERR_TAIL_MAX_BYTES`] bytes (cut on a char boundary), ASCII control
/// characters other than `\n`/`\t` stripped, trimmed. Empty when the child
/// said nothing. Shared by the host executor and the standalone facade.
#[must_use]
pub fn bounded_stderr_tail(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let mut start = text.len().saturating_sub(STDERR_TAIL_MAX_BYTES);
    while !text.is_char_boundary(start) {
        start += 1;
    }
    text[start..]
        .chars()
        .filter(|c| !c.is_ascii_control() || *c == '\n' || *c == '\t')
        .collect::<String>()
        .trim()
        .to_owned()
}

/// Append the child's stderr tail to an exit-derived failure message. Only
/// `ExitWithoutResult`/`ProcessLost` qualify: those are the classes where no
/// root result decided the outcome, so stderr is the only explanation. Any
/// other class (and an empty tail) returns the message unchanged.
#[must_use]
pub fn with_stderr_suffix(
    class: crate::events::FailureClass,
    message: String,
    stderr_tail: &str,
) -> String {
    use crate::events::FailureClass;
    if stderr_tail.is_empty()
        || !matches!(
            class,
            FailureClass::ExitWithoutResult | FailureClass::ProcessLost
        )
    {
        return message;
    }
    format!("{message}; stderr: {stderr_tail}")
}

/// Handle to an owned child process. Implemented by the supervisor; used by
/// the executor and safe for a standalone consumer (AGENTKIT-110: no second
/// task scheduler).
#[async_trait]
pub trait OwnedChildHandle: Send {
    fn pid(&self) -> u32;
    fn generation(&self) -> u64;

    /// Next framed stdout chunk; `None` means EOF (never success).
    async fn next_frame(&mut self) -> Option<FrameLine>;

    /// Current retained stderr tail (bounded).
    fn stderr_tail(&self) -> Vec<u8>;

    /// Deliver the encoded admitted input to stdin. Readers must already be
    /// running; the write is bounded and must not deadlock against output
    /// (acceptance: a fake child that emits nothing until it receives the
    /// prompt completes).
    async fn deliver_input(
        &mut self,
        bytes: &[u8],
        close_stdin: bool,
    ) -> Result<(), TransportFault>;

    /// Graceful interrupt followed by bounded escalation.
    async fn interrupt(&mut self, policy: InterruptPolicy) -> InterruptReceipt;

    /// Await process termination (idempotent).
    async fn wait(&mut self) -> ProcessExit;
}

/// Discovery/auth evidence types (AGENTKIT-109). Never secret material.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExecutableEvidence {
    pub path: std::path::PathBuf,
    /// Observed version text from the executable itself.
    pub version: String,
    /// Optional sha256 of the executable at probe time (rechecked before
    /// spawn to detect replacement).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub digest: Option<String>,
}

/// Observed authentication route from official status output. Subscription
/// authentication is not proof of model entitlement.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "camelCase", deny_unknown_fields)]
pub enum AuthMode {
    Subscription {
        /// Account-scope evidence available without inspecting credentials
        /// (e.g. the auth method label reported by the official status).
        account_scope: String,
    },
    ApiKey,
    ApiKeyHelper,
    ThirdParty {
        provider: String,
    },
    /// Signed out or unobservable.
    None,
    /// Version did not provide a machine-readable status.
    Unknown,
}

/// Managed policy observation: restrictions are preserved, never bypassed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PolicyState {
    None,
    PresentCompatible,
    Conflicting,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AuthEvidence {
    pub mode: AuthMode,
    pub policy: PolicyState,
    /// Configuration directory reported by the official status (probe and
    /// run share this namespace; credentials are never read).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_directory: Option<String>,
}

/// Result of preflight readiness. Preflight-ready is NOT live-verified:
/// status cannot prove token refresh, entitlement, quota or inference
/// success (AGENTKIT-109).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PreflightReport {
    pub executable: ExecutableEvidence,
    pub auth: AuthEvidence,
    /// Blocking readiness problems: a non-empty list means preflight is
    /// NOT ready and the executor MUST refuse the spawn. The official
    /// driver returns every blocking state as a `NativeRefusal` instead,
    /// leaving this empty — the field is the authoritative gate for any
    /// host-supplied backend.
    pub blocking_issues: Vec<String>,
    /// Honest informational notes (subscription sign-in proves neither
    /// entitlement nor quota, managed policy present/unobserved…).
    /// Surfaced to operators, never swallowed (AGENTKIT-109), and never
    /// a reason to refuse a spawn that passed all blocking checks.
    pub readiness_notes: Vec<String>,
}

/// Resolves the canonical executable and official auth status for a
/// host-supplied environment (locator port, AGENTKIT-109). Probe and
/// execution use the same official configuration namespace.
#[async_trait]
pub trait ExecutableLocator: Send + Sync {
    /// Locate one canonical executable with identity/version evidence.
    async fn locate(&self) -> Result<ExecutableEvidence, NativeRefusal>;

    /// Official auth status without reading credential material.
    async fn auth_status(&self) -> Result<AuthEvidence, NativeRefusal>;
}

/// Incremental decoder from framed stdout lines to normalized events.
/// One accumulator reconciles partial deltas with complete messages so a
/// turn yields exactly one user-visible text sequence (AGENTKIT-111).
pub trait StreamFrameAdapter: Send {
    /// Decode one framed line. Correctness-affecting unknown shapes return
    /// `Err` (fail explicitly); qualified additive metadata is tolerated.
    fn push_line(&mut self, line: &[u8]) -> Result<Vec<NativeEvent>, ProtocolViolation>;

    /// Stream ended (EOF or exit) without a completed outcome: report
    /// retained partial text for truthful failure handling.
    fn finish(&mut self) -> FinishDisposition;

    /// Authoritative terminal mapping for one attempt. The adapter is the
    /// only component that has seen the vendor's root result facts, so the
    /// executor — generic over drivers — asks the adapter, never the
    /// backend's stateless helper. EOF/exit without a root result can never
    /// produce `Completed` (AGENTKIT-107 §7).
    fn terminal_outcome(&self, exit: &ProcessExit) -> crate::events::NativeTerminalOutcome;
}

/// The vendor-protocol port implemented by an optional driver (e.g. the
/// Claude Code driver). The generic executor in `agentkit-native-exec` consumes
/// only these operations; it never depends on a driver crate.
#[async_trait]
pub trait NativeBackend: Send + Sync {
    /// Stable backend identity, e.g. `"claude-code"`.
    fn backend_id(&self) -> &str;

    /// Deterministic pre-spawn validation beyond capability policy: profile
    /// compatibility, managed policy, executable replacement, and any
    /// preflight the driver requires. Refusals carry stable codes and never
    /// trigger a fallback inference route.
    async fn preflight(
        &self,
        target: &crate::target::NativeExecutionTarget,
        locator: &dyn ExecutableLocator,
        cancel: &CancellationToken,
    ) -> Result<PreflightReport, NativeRefusal>;

    /// Build the frozen launch plan for one attempt (argv/env/cwd/per-run
    /// layout). `mcp` is the host-staged scoped endpoint for OwnedMcpChat.
    fn plan_turn(
        &self,
        target: &crate::target::NativeExecutionTarget,
        input: &TurnInput,
        mcp: Option<&McpEndpointSpec>,
    ) -> Result<LaunchPlan, NativeRefusal>;

    /// Encode the admitted input bytes for stdin (typed, prompt-free argv).
    fn encode_input(&self, input: &TurnInput) -> Result<Vec<u8>, NativeRefusal>;

    /// A fresh per-attempt frame adapter.
    fn new_frame_adapter(&self) -> Box<dyn StreamFrameAdapter>;

    /// Map observed frames + process exit to the single authoritative
    /// terminal outcome. EOF/exit without a root result can never produce
    /// `Completed` (AGENTKIT-107 §7).
    fn terminal_outcome(
        &self,
        exit: &ProcessExit,
        observed: &[NativeEvent],
        partial_text: Option<&str>,
    ) -> NativeTerminalOutcome;
}

/// OS process ownership port (AGENTKIT-110). Implementations must:
/// * spawn with typed argv, explicit controlled cwd and environment;
/// * start stdout/stderr readers immediately, independently of input
///   delivery (no prompt-after-stream deadlocks);
/// * bind every child to a generation so stale handles are fenced;
/// * terminate only owned descendants and report cleanup uncertainty.
#[async_trait]
pub trait ProcessSupervisor: Send + Sync {
    async fn spawn(
        &self,
        plan: &LaunchPlan,
        limits: &ResourceLimits,
        cancellation: &CancellationToken,
    ) -> Result<Box<dyn OwnedChildHandle>, NativeError>;

    /// Host-shutdown drain: interrupt/await all live owned children and
    /// report which remain uncertain. Reserved for whole-runtime shutdown;
    /// per-attempt cleanup uses [`Self::terminate_owned`] so one dropped
    /// attempt can never interrupt another's process.
    async fn drain_owned(&self, policy: InterruptPolicy) -> Vec<InterruptReceipt>;

    /// Terminate the single live owned child matching `(pid, generation)`
    /// with the given policy and deregister it; returns `None` when no live
    /// child carries that identity (already reaped, exited, or never owned
    /// by this supervisor). Every other live child is untouched — this is
    /// the per-attempt cleanup handle (AGENTKIT-110 lifecycle hardening).
    async fn terminate_owned(
        &self,
        pid: u32,
        generation: u64,
        policy: InterruptPolicy,
    ) -> Option<InterruptReceipt>;

    /// Normal-completion group-reconcile read-back (review round-4 F5): the
    /// honest descendant-cleanup outcome recorded when the owned child with
    /// this `(pid, generation)` exited on its own (`wait`) — with no
    /// interrupt ladder to verify the process group. A supervisor that
    /// could NOT confirm its owned process group empty reports
    /// `cleanup_uncertain: true`, and the executor must surface that as
    /// visible uncertainty, never as a clean attempt.
    ///
    /// `None` means "no report recorded for that identity", which is NOT a
    /// claim that the group was clean. The default serves supervisors
    /// without descendant visibility (and every pre-existing implementor,
    /// untouched): an honest supervisor overrides this.
    fn descendant_cleanup(&self, _pid: u32, _generation: u64) -> Option<InterruptReceipt> {
        None
    }
}

/// Convenience: collect adapter events until EOF (used by the executor and
/// the conformance harness).
pub async fn collect_frames(
    child: &mut dyn OwnedChildHandle,
    adapter: &mut dyn StreamFrameAdapter,
) -> Result<(Vec<NativeEvent>, Option<TransportFault>), NativeError> {
    let mut events = Vec::new();
    while let Some(frame) = child.next_frame().await {
        match frame {
            FrameLine::Line(line) => match adapter.push_line(&line) {
                Ok(more) => events.extend(more),
                Err(violation) => {
                    return Err(NativeError::Protocol {
                        code: violation.code,
                        message: violation.message,
                    });
                }
            },
            FrameLine::Fault(fault) => return Ok((events, Some(fault))),
        }
    }
    Ok((events, None))
}

/// Helper for drivers: canonical JSON digest used in fingerprints.
pub fn fingerprint(value: &Value) -> String {
    crate::canonical_digest(value)
}

#[cfg(test)]
mod tests {
    #[test]
    fn stderr_tail_is_bounded_char_safe_and_sanitised() {
        assert_eq!(super::bounded_stderr_tail(b""), "");
        assert_eq!(super::bounded_stderr_tail(b"  \n"), "");
        // Long input keeps only the tail, within the byte bound.
        let long = format!("{}END", "x".repeat(10_000));
        let tail = super::bounded_stderr_tail(long.as_bytes());
        assert!(tail.len() <= super::STDERR_TAIL_MAX_BYTES);
        assert!(tail.ends_with("END"));
        // The cut lands inside a multi-byte char: no panic, no U+FFFD.
        let wide = "é".repeat(3_000);
        let tail = super::bounded_stderr_tail(wide.as_bytes());
        assert!(tail.len() <= super::STDERR_TAIL_MAX_BYTES);
        assert!(tail.chars().all(|c| c == 'é'));
        // Control characters (ANSI escape, NUL, CR) are stripped; \n \t stay.
        let tail = super::bounded_stderr_tail(b"\x1b[31mError:\x1b[0m bad\x00\r\n\tnext\n");
        assert_eq!(tail, "[31mError:[0m bad\n\tnext");
    }

    use super::*;

    #[test]
    fn limits_defaults_are_bounded() {
        let limits = ResourceLimits::default();
        assert!(limits.max_frame_bytes > 0 && limits.max_stdin_bytes > 0);
        assert!(limits.interrupt_grace_ms > 0 && limits.terminate_grace_ms > 0);
    }

    #[test]
    fn launch_plan_redacts_token_shaped_argv() {
        let plan = LaunchPlan {
            argv: vec![
                "claude".into(),
                "-p".into(),
                "--mcp-config".into(),
                "/tmp/mcp.json".into(),
                "--token=super-secret".into(),
            ],
            env: vec![("PATH".into(), "/usr/bin".into())],
            cwd: "/tmp".into(),
            per_run_dir: "/tmp/run".into(),
            permission_policy: PermissionPromptPolicy::None,
            max_vendor_turns: Some(1),
            native_session_id: None,
            fingerprint: "fp".into(),
        };
        let redacted = plan.redacted_argv();
        assert_eq!(redacted[4], "«redacted»");
        assert_eq!(redacted[3], "/tmp/mcp.json");
        // The structured plan itself never carries credential fields.
        assert!(
            plan.env
                .iter()
                .all(|(k, _)| !k.to_lowercase().contains("token"))
        );
    }

    #[test]
    fn resume_shapes_are_exact_only() {
        let exact = TurnResume::Exact {
            vendor_session_id: "sess-1".into(),
        };
        let value = serde_json::to_value(&exact).unwrap();
        assert_eq!(value["mode"], "exact");
        // No continue-latest variant exists on the wire.
        assert!(
            serde_json::from_value::<TurnResume>(serde_json::json!({"mode": "continueLatest"}))
                .is_err()
        );
    }

    #[test]
    fn exit_evidence_keeps_result_visibility_separate_from_exit() {
        let exit = ProcessExit {
            pid: 5,
            generation: 1,
            exit_code: Some(0),
            signal: None,
            after_interrupt: false,
        };
        let evidence = exit.terminal_evidence(false);
        assert!(!evidence.saw_root_result);
        assert_eq!(evidence.exit_code, Some(0));
    }
}
