//! Outcome rendering for the states a fake run cannot reach cheaply (an
//! output limit fills 64 MiB) and for every settlement.

use super::*;
use agentkit_workspaces::{Generation, HostPort, OperationId, RelPath, WorkspaceId};

fn outcome(settlement: Settlement) -> OperationOutcome {
    OperationOutcome {
        operation_id: OperationId::new("exec-1"),
        workspace_id: WorkspaceId::new("ws-1"),
        generation: Generation(2),
        image_digest: "sha256:img".into(),
        settlement,
        output: b"line 1\nline 2\n".to_vec(),
        output_cut: false,
        log_truncated: false,
        log_path: Some(RelPath::parse(".sandbox/logs/exec-1.log").unwrap()),
        written: None,
        network_denied: vec![],
        replayed: false,
        started_at: 1_000,
        ended_at: Some(1_250),
    }
}

fn text(result: &ToolResult) -> &str {
    result.model_data.as_ref().and_then(Value::as_str).unwrap()
}

#[test]
fn a_receipt_reports_exit_signal_duration_and_the_absolute_log_path() {
    let done = command(
        &outcome(Settlement::Settled(OperationState::Succeeded { exit: 0 })),
        &ToolLimits::default(),
    );
    assert!(done.ok);
    assert_eq!(text(&done), "line 1\nline 2\n[exit code 0]");
    let data = done.data.unwrap();
    assert_eq!(data["exitCode"], 0);
    assert_eq!(data["signal"], Value::Null);
    assert_eq!(data["durationMs"], 250);
    assert_eq!(data["logPath"], "/workspace/.sandbox/logs/exec-1.log");
    assert_eq!(data["state"], "succeeded");
    let killed = command(
        &outcome(Settlement::Settled(OperationState::Failed {
            exit: None,
            signal: Some(9),
        })),
        &ToolLimits::default(),
    );
    assert!(!killed.ok);
    assert!(text(&killed).ends_with("[killed by signal 9]"));
}

#[test]
fn an_output_limit_and_a_cut_tail_are_named() {
    let mut limited = outcome(Settlement::Settled(OperationState::OutputLimit));
    limited.output_cut = true;
    limited.log_truncated = true;
    let done = command(&limited, &ToolLimits::default());
    assert!(!done.ok);
    assert!(done.truncated);
    let shown = text(&done);
    assert!(
        shown.starts_with(
            "[earlier output left out; full log: /workspace/.sandbox/logs/exec-1.log]"
        ),
        "{shown}"
    );
    assert!(shown.contains("[the full log reached its size limit]"));
    assert!(shown.ends_with("[stopped: the output passed the log limit; the command was ended]"));
    let data = done.data.unwrap();
    assert_eq!(
        (
            data["state"].as_str(),
            data["truncated"].as_bool(),
            data["outputCut"].as_bool()
        ),
        (Some("output_limit"), Some(true), Some(true))
    );
}

#[test]
fn an_interrupted_receipt_reports_how_it_ended() {
    let limits = ToolLimits::default();
    let done = command(
        &outcome(Settlement::Interrupted(OperationState::Cancelled)),
        &limits,
    );
    assert!(!done.ok);
    assert!(text(&done).contains("[interrupted: OneMind quit and stopped the sandbox"));
    assert!(text(&done).ends_with("[cancelled]"));
    assert_eq!(done.data.unwrap()["interrupted"], true);
    // It finished before the quit's cancel landed: that is a success.
    let finished = Settlement::Interrupted(OperationState::Succeeded { exit: 0 });
    let done = command(&outcome(finished.clone()), &limits);
    assert!(done.ok, "{}", text(&done));
    assert!(text(&done).ends_with("[exit code 0]"));
    let written = Written {
        path: "/workspace/a.txt".into(),
        bytes: 1,
        sha256: Sha256::of(b"x"),
    };
    let wrote = write(&outcome(finished), &written, &limits);
    assert!(wrote.ok, "a write the receipt confirms was written");
    assert_eq!(text(&wrote), "Wrote 1 bytes to /workspace/a.txt.");
    let lost = write(
        &outcome(Settlement::Interrupted(OperationState::Cancelled)),
        &written,
        &limits,
    );
    assert_eq!(lost.data.unwrap()["errorCode"], "write_failed");
}

#[test]
fn an_unknown_outcome_says_so_and_is_never_the_not_started_shape() {
    let mut unknown = outcome(Settlement::Uncertain {
        reason: "dispatch did not answer".into(),
        fenced: true,
    });
    unknown.network_denied = vec![HostPort::new("example.com", 443).unwrap()];
    let done = command(&unknown, &ToolLimits::default());
    assert!(!done.ok);
    let shown = text(&done);
    assert!(shown.starts_with("tool_result_uncertain: outcome unknown: OneMind stopped the sandbox before the command reported back."), "{shown}");
    assert!(shown.contains("fenced"));
    assert!(shown.contains("network_denied: the connection to example.com:443 was refused"));
    let data = done.data.unwrap();
    assert_eq!(data["errorCode"], "tool_result_uncertain");
    assert_eq!(data["fenced"], true);
    assert_ne!(data["phase"], "before_spawn");
    assert_ne!(data["started"], false);
    // The shape the ledger keeps uncertain, and the generation it asks about.
    assert_eq!(data["uncertain"], true);
    assert_eq!(data["approvalGeneration"], "exec-1");
}

#[test]
fn a_refused_connection_names_host_and_port_and_the_recorded_request() {
    let mut denied = outcome(Settlement::Settled(OperationState::Failed {
        exit: Some(7),
        signal: None,
    }));
    denied.network_denied = vec![HostPort::new("example.com", 443).unwrap()];
    let done = command(&denied, &ToolLimits::default());
    assert!(!done.ok);
    assert!(text(&done).contains(
        "network_denied: the connection to example.com:443 was refused by the sandbox network policy. A request was recorded for the owner"
    ));
    let data = done.data.unwrap();
    assert_eq!(data["errorCode"], "network_denied");
    assert_eq!(
        data["networkDenied"],
        json!([{"host": "example.com", "port": 443}])
    );
}
