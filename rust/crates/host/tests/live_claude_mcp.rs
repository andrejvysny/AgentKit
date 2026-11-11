//! Opt-in live qualification of the OwnedMcpChat profile against the REAL
//! `claude` CLI: the per-attempt loopback broker, the driver's launch plan
//! and the frame adapter, wired exactly as the host executor wires them.
//!
//! Skipped (returns early) unless `AGENTKIT_LIVE_CLAUDE=1`; additionally
//! `#[ignore]`d so a plain `cargo test` never spends tokens. Each turn costs
//! a fraction of a cent on the `haiku` model. Output never includes the
//! broker bearer token or account identity.
//!
//! ```text
//! AGENTKIT_LIVE_CLAUDE=1 cargo test -p agentkit-host --features http \
//!     --test live_claude_mcp -- --ignored --nocapture
//! ```

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use agentkit_claude::backend::default_backend;
use agentkit_claude::discovery::ClaudeSupportMatrix;
use agentkit_claude::transport::OwnedProcessSupervisor;
use agentkit_host::native_bridge::{
    AttemptBroker, BrokerAuthority, BrokerDenial, BrokerExecuted, BrokerInvocation, BrokerLimits,
    BrokerToolDefinition,
};
use agentkit_native::{
    AuthExpectation, ContentPolicy, EXECUTION_TARGET_SCHEMA_VERSION, FrameLine, InterruptPolicy,
    InterruptReceipt, NativeBackend, NativeEvent, NativeExecutionTarget, NativeProfile,
    NativeTerminalOutcome, ProcessSupervisor, ResourceLimits, TurnInput, TurnResume,
    VendorToolPhase,
};
use async_trait::async_trait;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

const SERVER_NAME: &str = "agentkit";
const SECRET: &str = "7341";

/// Scripted authority: `forbidden` is denied at authorize; `lookup_secret`
/// executes and returns text containing the secret. Records which tools
/// reached `execute`.
struct Authority {
    executed: Mutex<Vec<String>>,
    authorize_sleep: Duration,
}

#[async_trait]
impl BrokerAuthority for Authority {
    fn attempt_generation(&self) -> Option<u64> {
        Some(1)
    }

    async fn authorize(
        &self,
        invocation: &BrokerInvocation,
        _arguments: &Value,
    ) -> Result<(), BrokerDenial> {
        tokio::time::sleep(self.authorize_sleep).await;
        if invocation.tool_name == "forbidden" {
            return Err(BrokerDenial {
                code: "denied".into(),
                message: "the user denied this tool".into(),
            });
        }
        Ok(())
    }

    async fn execute(
        &self,
        invocation: &BrokerInvocation,
        _arguments: Value,
    ) -> Result<BrokerExecuted, BrokerDenial> {
        self.executed
            .lock()
            .unwrap()
            .push(invocation.tool_name.clone());
        Ok(BrokerExecuted {
            content: vec![json!({
                "type": "text",
                "text": format!("the secret number is {SECRET}"),
            })],
            structured: None,
        })
    }
}

/// Everything one live turn produced.
struct Run {
    events: Vec<NativeEvent>,
    violations: Vec<String>,
    outcome: NativeTerminalOutcome,
    receipt: Option<InterruptReceipt>,
    executed: Vec<String>,
}

impl Run {
    /// The `Result`-phase observations for the named tool (matched through
    /// the `Requested` observation that carries the tool name).
    fn results_for(&self, tool: &str) -> Vec<(Option<bool>, Option<String>)> {
        let ids: Vec<&str> = self
            .events
            .iter()
            .filter_map(|event| match event {
                NativeEvent::ToolObservation {
                    vendor_tool_use_id,
                    name,
                    phase: VendorToolPhase::Requested,
                    ..
                } if name.ends_with(tool) => Some(vendor_tool_use_id.as_str()),
                _ => None,
            })
            .collect();
        self.events
            .iter()
            .filter_map(|event| match event {
                NativeEvent::ToolObservation {
                    vendor_tool_use_id,
                    phase: VendorToolPhase::Result,
                    is_error,
                    result,
                    ..
                } if ids.contains(&vendor_tool_use_id.as_str()) => {
                    Some((*is_error, result.clone()))
                }
                _ => None,
            })
            .collect()
    }
}

/// Drive one OwnedMcpChat turn against the real CLI. `interrupt_after`
/// requests a `UserStop` that long after spawn.
async fn run_turn(
    prompt: &str,
    authorize_sleep: Duration,
    interrupt_after: Option<Duration>,
) -> Run {
    let run_root = tempfile::tempdir().unwrap();
    let (backend, locator) = default_backend(None, run_root.path());
    let evidence = locator.locate().await.expect("locate claude");
    let target = NativeExecutionTarget {
        schema_version: EXECUTION_TARGET_SCHEMA_VERSION,
        backend: "claude-code".into(),
        backend_version: Some(evidence.version.clone()),
        profile: NativeProfile::OwnedMcpChat,
        executable: evidence.path.display().to_string(),
        requested_model: Some("haiku".into()),
        requested_effort: None,
        auth_expectation: AuthExpectation::AsConfigured,
        capabilities: ClaudeSupportMatrix::snapshot(Some(&evidence.version)),
        config_identity: "live-qualification".into(),
        system_prompt_revision: Some("r1".into()),
        content: ContentPolicy::default(),
    };
    let authority = Arc::new(Authority {
        executed: Mutex::new(Vec::new()),
        authorize_sleep,
    });
    let catalog = vec![
        BrokerToolDefinition {
            name: "lookup_secret".into(),
            description: "Look up the secret number for a given key.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {"key": {"type": "string"}},
                "required": ["key"],
            }),
            writes: false,
        },
        BrokerToolDefinition {
            name: "forbidden".into(),
            description: "Delete a note by id.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {"id": {"type": "string"}},
                "required": ["id"],
            }),
            writes: true,
        },
    ];
    let broker = AttemptBroker::start(
        "task-live",
        "attempt-live",
        SERVER_NAME,
        catalog,
        BrokerLimits::default(),
        authority.clone(),
    )
    .await
    .expect("broker starts");
    let spec = broker.spec();

    let input = TurnInput {
        input_id: "live-mcp-1".into(),
        text: prompt.into(),
        context_blocks: vec![],
        resume: TurnResume::Fresh {
            seed_history: vec![],
        },
        system_prompt_revision: Some("r1".into()),
        system_prompt: Some("You are a helpful assistant with access to tools.".into()),
        max_vendor_turns: None,
        turn_deadline_ms: 180_000,
    };
    let plan = backend
        .plan_turn(&target, &input, Some(&spec))
        .expect("plan the owned-MCP turn");
    let supervisor = OwnedProcessSupervisor::new();
    let cancel = CancellationToken::new();
    let mut child = supervisor
        .spawn(&plan, &ResourceLimits::default(), &cancel)
        .await
        .expect("spawn claude");
    let bytes = backend.encode_input(&input).expect("encode input");
    child
        .deliver_input(&bytes, true)
        .await
        .expect("deliver input");

    let mut adapter = backend.new_frame_adapter();
    let mut events = Vec::new();
    let mut violations = Vec::new();
    let mut receipt = None;
    let started = Instant::now();
    let hard_stop = Duration::from_secs(240);
    loop {
        let wait = match interrupt_after {
            Some(after) if receipt.is_none() => after.saturating_sub(started.elapsed()),
            _ => hard_stop.saturating_sub(started.elapsed()),
        };
        tokio::select! {
            frame = child.next_frame() => match frame {
                None => break,
                Some(FrameLine::Line(line)) => match adapter.push_line(&line) {
                    Ok(new) => events.extend(new),
                    Err(violation) => violations.push(format!("{}: {}", violation.code, violation.message)),
                },
                Some(FrameLine::Fault(fault)) => violations.push(format!("transport fault: {fault:?}")),
            },
            _ = tokio::time::sleep(wait) => {
                if receipt.is_none() && interrupt_after.is_some() {
                    receipt = Some(child.interrupt(InterruptPolicy::UserStop).await);
                } else {
                    let _ = child.interrupt(InterruptPolicy::Quarantine).await;
                    panic!("live turn exceeded its {hard_stop:?} bound");
                }
            }
        }
    }
    let exit = child.wait().await;
    let stderr = agentkit_native::bounded_stderr_tail(&child.stderr_tail());
    let outcome = adapter.terminal_outcome(&exit);
    println!("exit={exit:?} outcome={outcome:?} stderr={stderr:?}");
    broker.shutdown().await;
    let _ = std::fs::remove_dir_all(&plan.per_run_dir);
    let executed = authority.executed.lock().unwrap().clone();
    Run {
        events,
        violations,
        outcome,
        receipt,
        executed,
    }
}

fn live_enabled() -> bool {
    if std::env::var("AGENTKIT_LIVE_CLAUDE").as_deref() == Ok("1") {
        true
    } else {
        eprintln!("skipped: AGENTKIT_LIVE_CLAUDE != 1");
        false
    }
}

#[tokio::test]
#[ignore = "live: needs a signed-in claude CLI; run with AGENTKIT_LIVE_CLAUDE=1 cargo test -p agentkit-host --features http --test live_claude_mcp -- --ignored"]
async fn live_tool_call_reaches_the_broker_and_returns_plain_text() {
    if !live_enabled() {
        return;
    }
    let run = run_turn(
        "Call the lookup_secret tool exactly once with key \"alpha\", then tell me the \
         secret number it returned.",
        Duration::ZERO,
        None,
    )
    .await;

    // The vendor attests the broker connected (Fix A: the rejected
    // `server/discover` probe must not take the listener down).
    let init = run
        .events
        .iter()
        .find_map(|event| match event {
            NativeEvent::Init { mcp_servers, .. } => Some(mcp_servers.clone()),
            _ => None,
        })
        .expect("an init event");
    assert!(
        init.iter()
            .any(|server| server.name == SERVER_NAME && server.status == "connected"),
        "broker must be connected at init: {init:?}"
    );
    assert_eq!(run.executed, ["lookup_secret"], "exactly one governed call");
    let results = run.results_for("lookup_secret");
    assert_eq!(results.len(), 1, "one Result observation: {results:?}");
    assert_eq!(results[0].0, Some(false), "tool result is not an error");
    let text = results[0].1.clone().expect("result text");
    assert!(text.contains(SECRET), "result text: {text}");
    assert!(
        !text.contains("\"type\"") && !text.contains('{'),
        "result text must be plain, not JSON-wrapped: {text}"
    );
    match &run.outcome {
        NativeTerminalOutcome::Completed { text, .. } => {
            assert!(text.contains(SECRET), "final answer: {text}");
        }
        other => panic!("expected Completed, got {other:?}"),
    }
    assert!(
        run.violations.is_empty(),
        "violations: {:?}",
        run.violations
    );
}

#[tokio::test]
#[ignore = "live: needs a signed-in claude CLI; run with AGENTKIT_LIVE_CLAUDE=1 cargo test -p agentkit-host --features http --test live_claude_mcp -- --ignored"]
async fn live_denied_tool_never_executes() {
    if !live_enabled() {
        return;
    }
    let run = run_turn(
        "Call the forbidden tool exactly once with id \"note-1\" and then report what happened.",
        Duration::ZERO,
        None,
    )
    .await;
    assert!(
        run.executed.is_empty(),
        "a denied tool must never reach execute: {:?}",
        run.executed
    );
    let results = run.results_for("forbidden");
    assert_eq!(results.len(), 1, "one Result observation: {results:?}");
    assert_eq!(results[0].0, Some(true), "denial is flagged as an error");
    assert!(
        matches!(run.outcome, NativeTerminalOutcome::Completed { .. }),
        "outcome: {:?}",
        run.outcome
    );
    assert!(
        run.violations.is_empty(),
        "violations: {:?}",
        run.violations
    );
}

#[tokio::test]
#[ignore = "live: needs a signed-in claude CLI; run with AGENTKIT_LIVE_CLAUDE=1 cargo test -p agentkit-host --features http --test live_claude_mcp -- --ignored"]
async fn live_user_stop_during_approval_interrupts_the_turn() {
    if !live_enabled() {
        return;
    }
    // `authorize` parks for 60 s (a human decision that never comes); the
    // stop request arrives 10 s after spawn.
    let run = run_turn(
        "Call the lookup_secret tool exactly once with key \"alpha\", then report the result.",
        Duration::from_secs(60),
        Some(Duration::from_secs(10)),
    )
    .await;
    let receipt = run.receipt.expect("an interrupt was requested");
    assert!(receipt.terminated, "receipt: {receipt:?}");
    assert!(
        matches!(run.outcome, NativeTerminalOutcome::Interrupted { .. }),
        "outcome: {:?}",
        run.outcome
    );
    assert!(
        run.executed.is_empty(),
        "the parked call never reached execute: {:?}",
        run.executed
    );
}
