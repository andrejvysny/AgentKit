//! Replays sanitized captures of the REAL Claude Code CLI (v2.1.322,
//! `claude -p --output-format stream-json`) line by line through
//! [`ClaudeStreamAdapter`]. These pin behaviour the hermetic, hand-written
//! frames cannot: frames the adapter does not model must never kill a
//! healthy turn, and tool/MCP evidence must survive normalization.

use agentkit_claude::ClaudeStreamAdapter;
use agentkit_native::{
    McpServerStatus, NativeEvent, NativeTerminalOutcome, ProcessExit, StreamFrameAdapter,
    VendorToolPhase,
};

const DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/live-2.1.322");

struct Replay {
    adapter: ClaudeStreamAdapter,
    events: Vec<NativeEvent>,
}

fn exit(code: Option<i32>, after_interrupt: bool) -> ProcessExit {
    ProcessExit {
        pid: 1,
        generation: 1,
        exit_code: code,
        signal: None,
        after_interrupt,
    }
}

/// Feed every line of a capture; any protocol violation fails the test.
fn replay(name: &str) -> Replay {
    let path = format!("{DIR}/{name}.ndjson");
    let raw = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
    let mut adapter = ClaudeStreamAdapter::new();
    let mut events = Vec::new();
    for (n, line) in raw.lines().enumerate() {
        match adapter.push_line(line.as_bytes()) {
            Ok(mut evs) => events.append(&mut evs),
            Err(v) => panic!("{name}:{}: protocol violation {v:?}", n + 1),
        }
    }
    Replay { adapter, events }
}

fn init_of(events: &[NativeEvent]) -> (String, Vec<McpServerStatus>) {
    events
        .iter()
        .find_map(|e| match e {
            NativeEvent::Init {
                session_id,
                mcp_servers,
                ..
            } => Some((session_id.clone(), mcp_servers.clone())),
            _ => None,
        })
        .expect("init event")
}

/// Assert a clean-exit `Completed` with non-empty text and the init session.
fn assert_completed(name: &str) -> Replay {
    let r = replay(name);
    let (session, _) = init_of(&r.events);
    match r.adapter.terminal_outcome(&exit(Some(0), false)) {
        NativeTerminalOutcome::Completed {
            text, session_id, ..
        } => {
            assert!(!text.trim().is_empty(), "{name}: empty text");
            assert_eq!(session_id, session, "{name}: session id");
            // One user-visible text sequence: deltas and complete blocks
            // must not both be counted.
            let visible = r.adapter.partial_text().expect("visible text");
            assert_eq!(visible.trim(), text.trim(), "{name}: duplicated text");
        }
        other => panic!("{name}: expected Completed, got {other:?}"),
    }
    r
}

fn result_observation(events: &[NativeEvent]) -> (String, Option<bool>, Option<String>) {
    events
        .iter()
        .find_map(|e| match e {
            NativeEvent::ToolObservation {
                phase: VendorToolPhase::Result,
                name,
                is_error,
                result,
                ..
            } => Some((name.clone(), *is_error, result.clone())),
            _ => None,
        })
        .expect("tool result observation")
}

#[test]
fn plain_captures_complete_with_one_text_sequence() {
    for name in [
        "plain_fresh_haiku",
        "plain_exact_resume",
        "plain_thinking_default_model",
    ] {
        assert_completed(name);
    }
}

#[test]
fn mcp_tool_call_success_carries_status_name_and_result() {
    let r = assert_completed("mcp_tool_call_success");
    let (_, servers) = init_of(&r.events);
    assert_eq!(
        servers,
        vec![McpServerStatus {
            name: "agentkit".into(),
            status: "connected".into()
        }]
    );
    let (name, is_error, result) = result_observation(&r.events);
    assert_eq!(name, "mcp__agentkit__lookup_secret");
    assert_eq!(is_error, Some(false));
    assert!(result.unwrap().contains("7341"));
}

#[test]
fn mcp_slow_approval_completes() {
    assert_completed("mcp_slow_approval");
}

#[test]
fn mcp_tool_denied_with_thinking_tokens_is_tolerated_and_flagged() {
    let r = assert_completed("mcp_tool_denied_with_thinking_tokens");
    assert!(
        r.adapter
            .ignored_frames()
            .contains_key("system:thinking_tokens"),
        "diagnostics: {:?}",
        r.adapter.ignored_frames()
    );
    // Tolerated frames never surface as user-visible warnings.
    assert!(
        !r.events
            .iter()
            .any(|e| matches!(e, NativeEvent::Warning { .. }))
    );
    let (name, is_error, result) = result_observation(&r.events);
    assert_eq!(name, "mcp__agentkit__forbidden");
    assert_eq!(is_error, Some(true));
    assert!(result.unwrap().contains("denied"));
}

#[test]
fn mcp_broker_failed_reports_failed_status() {
    let r = replay("mcp_broker_failed_hallucinated");
    let (_, servers) = init_of(&r.events);
    assert_eq!(
        servers,
        vec![McpServerStatus {
            name: "agentkit".into(),
            status: "failed".into()
        }]
    );
}

#[test]
fn interrupted_while_parked_is_interrupted_only_after_a_requested_interrupt() {
    let r = replay("mcp_interrupted_while_parked");
    assert!(!r.adapter.saw_root_result());
    assert!(matches!(
        r.adapter.terminal_outcome(&exit(Some(0), true)),
        NativeTerminalOutcome::Interrupted { .. }
    ));
    assert!(matches!(
        r.adapter.terminal_outcome(&exit(Some(0), false)),
        NativeTerminalOutcome::Uncertain { .. }
    ));
}
