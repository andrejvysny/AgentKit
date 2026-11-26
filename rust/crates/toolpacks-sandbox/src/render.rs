//! What the model reads of an operation's outcome. A receipt gives the
//! exit status and the bounded output tail (the service already cut it to
//! the last 2,000 lines or 50 KB); a refused connection names its host and
//! port; an unknown outcome is said to be unknown (SBX-07) and is never the
//! not-started shape.

use agentkit_core::{ToolLimits, ToolResult};
use agentkit_workspaces::{OperationOutcome, OperationState, PathRoot, Settlement, Sha256};
use serde_json::{Map, Value, json};

use crate::envelope::{failed, result};
use crate::paths::root_path;

/// What a write put where.
pub(crate) struct Written {
    pub(crate) path: String,
    pub(crate) bytes: u64,
    pub(crate) sha256: Sha256,
}

fn state_name(state: OperationState) -> &'static str {
    match state {
        OperationState::Accepted => "accepted",
        OperationState::Running => "running",
        OperationState::Succeeded { .. } => "succeeded",
        OperationState::Failed { .. } => "failed",
        OperationState::TimedOut => "timed_out",
        OperationState::Cancelled => "cancelled",
        OperationState::OutputLimit => "output_limit",
        OperationState::Unknown => "unknown",
    }
}

fn footer(state: OperationState) -> String {
    match state {
        OperationState::Succeeded { exit } => format!("[exit code {exit}]"),
        OperationState::Failed {
            exit: Some(code), ..
        } => format!("[exit code {code}]"),
        OperationState::Failed {
            signal: Some(signal),
            ..
        } => format!("[killed by signal {signal}]"),
        OperationState::Failed { .. } => "[failed]".to_owned(),
        OperationState::TimedOut => {
            "[timed out: the sandbox ended the command at its deadline]".to_owned()
        }
        OperationState::Cancelled => "[cancelled]".to_owned(),
        OperationState::OutputLimit => {
            "[stopped: the output passed the log limit; the command was ended]".to_owned()
        }
        other => format!("[ended in state {}]", state_name(other)),
    }
}

fn log_path(outcome: &OperationOutcome) -> Option<String> {
    outcome
        .log_path
        .as_ref()
        .map(|path| root_path(PathRoot::Workspace, path))
}

fn denied_lines(outcome: &OperationOutcome) -> Vec<String> {
    outcome
        .network_denied
        .iter()
        .map(|target| {
            format!(
                "network_denied: the connection to {}:{} was refused by the sandbox network policy. A request was recorded for the owner; wait for their decision before retrying.",
                target.host(),
                target.port()
            )
        })
        .collect()
}

/// The structured part every outcome carries.
fn base_data(outcome: &OperationOutcome) -> Map<String, Value> {
    let denied: Vec<Value> = outcome
        .network_denied
        .iter()
        .map(|target| json!({"host": target.host(), "port": target.port()}))
        .collect();
    let duration = outcome
        .ended_at
        .map(|ended| ended.saturating_sub(outcome.started_at).max(0));
    let mut data = Map::new();
    data.insert("operationId".into(), json!(outcome.operation_id.as_str()));
    data.insert("workspaceId".into(), json!(outcome.workspace_id.as_str()));
    data.insert("generation".into(), json!(outcome.generation.0));
    data.insert("imageDigest".into(), json!(outcome.image_digest));
    data.insert("durationMs".into(), json!(duration));
    data.insert("truncated".into(), json!(outcome.log_truncated));
    data.insert("outputCut".into(), json!(outcome.output_cut));
    data.insert("logPath".into(), json!(log_path(outcome)));
    data.insert("networkDenied".into(), Value::Array(denied));
    data.insert("replayed".into(), json!(outcome.replayed));
    data
}

/// The unknown outcome of a command or a write (`what`). The ledger keeps
/// its receipt admitted (uncertain), so the result names the generation a
/// repeat of the call asks the owner about: the operation id, which is the
/// execution id (INV-2).
fn uncertain(
    what: &str,
    outcome: &OperationOutcome,
    reason: &str,
    fenced: bool,
    limits: &ToolLimits,
) -> ToolResult {
    let mut message = format!(
        "outcome unknown: OneMind stopped the sandbox before the {what} reported back. Files may be partly changed; do not assume it succeeded or failed, and inspect the sandbox before deciding ({reason})."
    );
    if fenced {
        message.push_str(
            " The sandbox is fenced now: nothing else runs in it until the owner recovers it.",
        );
    }
    let mut extra = base_data(outcome);
    extra.insert("uncertain".into(), json!(true));
    extra.insert("fenced".into(), json!(fenced));
    extra.insert("reason".into(), json!(reason));
    extra.insert(
        "approvalGeneration".into(),
        json!(outcome.operation_id.as_str()),
    );
    let mut done = failed(
        "tool_result_uncertain",
        &message,
        Value::Object(extra),
        limits,
    );
    if let Some(text) = done.model_data.as_ref().and_then(Value::as_str) {
        let mut lines = vec![text.to_owned()];
        lines.extend(denied_lines(outcome));
        done.model_data = Some(Value::String(lines.join("\n")));
    }
    done
}

/// The outcome of `sandbox_run`.
pub(crate) fn command(outcome: &OperationOutcome, limits: &ToolLimits) -> ToolResult {
    match &outcome.settlement {
        Settlement::Settled(state) => finished(outcome, *state, false, limits),
        Settlement::Interrupted(state) => finished(outcome, *state, true, limits),
        Settlement::Uncertain { reason, fenced } => {
            uncertain("command", outcome, reason, *fenced, limits)
        }
    }
}

fn command_text(outcome: &OperationOutcome, state: OperationState, interrupted: bool) -> String {
    let mut lines = vec![];
    if outcome.output_cut {
        let log = log_path(outcome).unwrap_or_else(|| "the log".to_owned());
        lines.push(format!("[earlier output left out; full log: {log}]"));
    }
    let output = String::from_utf8_lossy(&outcome.output);
    let output = output.trim_end_matches('\n');
    if !output.is_empty() {
        lines.push(output.to_owned());
    }
    if outcome.replayed {
        lines.push(
            "[read back from an earlier delivery of this call; its output is in the full log]"
                .to_owned(),
        );
    }
    if outcome.log_truncated {
        lines.push("[the full log reached its size limit]".to_owned());
    }
    lines.extend(denied_lines(outcome));
    if interrupted {
        lines.push(
            "[interrupted: OneMind quit and stopped the sandbox; the receipt below says how the command ended]"
                .to_owned(),
        );
    }
    lines.push(footer(state));
    lines.join("\n")
}

/// A receipt, read in the normal way or within a quit's grace: the state
/// it reports decides, an interruption is only noted.
fn finished(
    outcome: &OperationOutcome,
    state: OperationState,
    interrupted: bool,
    limits: &ToolLimits,
) -> ToolResult {
    let ok = matches!(state, OperationState::Succeeded { .. });
    let (exit, signal) = match state {
        OperationState::Succeeded { exit } => (Some(exit), None),
        OperationState::Failed { exit, signal } => (exit, signal),
        _ => (None, None),
    };
    let mut data = base_data(outcome);
    data.insert("state".into(), json!(state_name(state)));
    data.insert("exitCode".into(), json!(exit));
    data.insert("signal".into(), json!(signal));
    data.insert("interrupted".into(), json!(interrupted));
    if !ok && !outcome.network_denied.is_empty() {
        data.insert("errorCode".into(), json!("network_denied"));
    }
    let text = command_text(outcome, state, interrupted);
    let mut done = result(ok, text, Some(footer(state)), Value::Object(data), limits);
    done.truncated = outcome.output_cut || outcome.log_truncated;
    done
}

/// The outcome of `sandbox_write_file`.
pub(crate) fn write(
    outcome: &OperationOutcome,
    written: &Written,
    limits: &ToolLimits,
) -> ToolResult {
    let mut data = base_data(outcome);
    data.insert("path".into(), json!(written.path));
    data.insert("bytes".into(), json!(written.bytes));
    data.insert("sha256".into(), json!(written.sha256.as_str()));
    let state = match &outcome.settlement {
        Settlement::Uncertain { reason, fenced } => {
            return uncertain("write", outcome, reason, *fenced, limits);
        }
        Settlement::Settled(state) | Settlement::Interrupted(state) => *state,
    };
    data.insert("state".into(), json!(state_name(state)));
    let interrupted = matches!(outcome.settlement, Settlement::Interrupted(_));
    data.insert("interrupted".into(), json!(interrupted));
    if matches!(state, OperationState::Succeeded { .. }) {
        let text = format!("Wrote {} bytes to {}.", written.bytes, written.path);
        return result(true, text.clone(), Some(text), Value::Object(data), limits);
    }
    let message = format!(
        "{} was not written {}; the disk may be full: check sandbox_get_status",
        written.path,
        footer(state)
    );
    failed("write_failed", &message, Value::Object(data), limits)
}

#[cfg(test)]
mod tests;
