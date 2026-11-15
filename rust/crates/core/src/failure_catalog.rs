//! One failure type for every tool (TU-10). Builds `AiToolFailure` from a code
//! and the text a tool raised, looks the code up in the table
//! (`failure_table.rs`) for its class and next step, and renders it for the
//! model: the message, then "Why: <cause>", then "Next: <next step>".
use crate::{ToolLimits, ToolResult};
use agentkit_contracts::{AiToolFailure, AiToolOutcomeClass, Optional};
use serde_json::{Value, json};
use std::collections::BTreeMap;

/// Appended from the second consecutive failure of one tool in one class.
pub const STOP_RETRYING: &str = "stop retrying; explain what you were trying to do";

const UNLISTED_NEXT_STEP: &str = "Read the message, adjust the call, and try once more.";
const TIMED_OUT: &str = "timed out after";

#[derive(Clone, Copy, Debug)]
pub struct Spec {
    pub code: &'static str,
    pub class: AiToolOutcomeClass,
    pub retryable: bool,
    /// Sentence used when a tool raised only the bare code.
    pub message: &'static str,
    pub next_step: &'static str,
}

/// Every known failure code, in table order.
pub fn catalog() -> impl Iterator<Item = &'static Spec> {
    crate::failure_table::all()
}

pub fn spec(code: &str) -> Option<&'static Spec> {
    catalog().find(|spec| spec.code == code)
}

/// The table row that governs `code` for this message: a generic execution
/// failure that says it timed out is a timeout.
fn governing(code: &str, message: &str) -> Option<&'static Spec> {
    if code == "exec_failed" && message.contains(TIMED_OUT) {
        return spec("tool_timeout");
    }
    spec(code)
}

/// Outcome class of a failure code (T0.1 classes).
pub fn class_of(code: &str, message: &str) -> AiToolOutcomeClass {
    governing(code, message).map_or(AiToolOutcomeClass::ToolError, |row| row.class)
}

/// Build the failure for `code`. `message` is what the tool said; an empty one
/// falls back to the table sentence. The next step always comes from the table.
pub fn failure(code: &str, message: &str, cause: Option<&str>) -> AiToolFailure {
    let row = governing(code, message);
    let text = if message.trim().is_empty() {
        row.map_or("The tool failed.", |row| row.message)
    } else {
        message
    };
    AiToolFailure {
        code: code.to_owned(),
        class: row.map_or(AiToolOutcomeClass::ToolError, |row| row.class),
        message: text.to_owned(),
        cause: cause.map_or(Optional::Absent, |cause| {
            Optional::Present(cause.to_owned())
        }),
        next_step: row
            .map_or(UNLISTED_NEXT_STEP, |row| row.next_step)
            .to_owned(),
        retryable: row.is_some_and(|row| row.retryable),
        extra: BTreeMap::new(),
    }
}

/// Split `code` or `code: detail` (a guard reason, a subagent refusal).
pub fn split_refusal(reason: &str) -> (&str, &str) {
    let end = reason
        .find(|ch: char| !(ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_'))
        .unwrap_or(reason.len());
    let code = &reason[..end];
    match reason[end..].strip_prefix(':') {
        Some(detail) if !code.is_empty() => (code, detail.trim()),
        None if !code.is_empty() && end == reason.len() => (code, ""),
        _ => ("", reason),
    }
}

/// Failure for a refusal reason such as `read_only: edit_file is not allowed`
/// or a bare code. A reason without a known code is a guard refusal.
pub fn from_refusal(reason: &str) -> AiToolFailure {
    let (code, detail) = split_refusal(reason);
    if spec(code).is_some() {
        // A bare code has no text of its own; the table sentence stands in.
        return failure(code, if detail.is_empty() { "" } else { reason }, None);
    }
    failure("tool_guard_refused", reason, None)
}

/// What the model reads.
pub fn render(failure: &AiToolFailure) -> String {
    let mut text = failure.message.trim_end().to_owned();
    if let Some(cause) = failure.cause.as_ref().filter(|cause| !cause.is_empty()) {
        text.push_str("\nWhy: ");
        text.push_str(cause);
    }
    if !failure.next_step.is_empty() && !states(&failure.message, &failure.next_step) {
        text.push_str("\nNext: ");
        text.push_str(&failure.next_step);
    }
    text
}

/// True when the message already tells the model the next step, so the
/// "Next:" line would only repeat it.
fn states(message: &str, next_step: &str) -> bool {
    let needle = next_step.trim_end_matches('.').to_lowercase();
    message.to_lowercase().contains(&needle)
}

/// The failure as JSON (the shape of `AiToolFailureSchema`).
pub fn failure_json(failure: &AiToolFailure) -> Value {
    serde_json::to_value(failure).expect("failure serializes")
}

/// Add the repair-budget line to a rendered failure.
pub fn with_stop_retrying(rendered: &str) -> String {
    format!("{rendered}\nRepeated failure: {STOP_RETRYING}.")
}

/// An `ok:false` result carrying the failure: the model text in `summary`, the
/// structured failure in `data` beside the legacy error fields.
pub fn failure_result(
    error_code: &str,
    failure: &AiToolFailure,
    phase: &str,
    extra: Option<Value>,
    limits: &ToolLimits,
) -> ToolResult {
    let mut data = json!({
        "errorCode": error_code,
        "errorMessage": failure.message,
        "phase": phase,
        "retryable": failure.retryable,
        "failure": failure_json(failure),
    });
    if let Some(Value::Object(extra)) = extra {
        for (key, value) in extra {
            data[key] = value;
        }
    }
    ToolResult {
        ok: false,
        data: Some(data.clone()),
        model_data: Some(data),
        summary: Some(render(failure)),
        sources: vec![],
        warnings: vec![],
        truncated: false,
        limits: limits.clone(),
        status: None,
        model_content: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentkit_contracts::JsonDeserialize as _;

    #[test]
    fn renders_message_why_next() {
        let mut made = failure("not_found", "nope.txt does not exist", Some("no such file"));
        let text = render(&made);
        assert_eq!(
            text,
            "nope.txt does not exist\nWhy: no such file\nNext: List the folder, or search by name, to find the right path."
        );
        made.cause = Optional::Absent;
        assert!(!render(&made).contains("Why:"));
    }

    #[test]
    fn next_line_is_skipped_when_the_message_already_says_it() {
        let made = failure(
            "file_changed",
            "a.rs changed since you read it. Read it again and redo the edit.",
            None,
        );
        assert!(!render(&made).contains("Next:"));
    }

    #[test]
    fn bare_code_uses_the_table_sentence_and_refusals_split() {
        let made = from_refusal("spawn_key_required");
        assert_eq!(made.code, "spawn_key_required");
        assert!(
            made.message
                .starts_with("This spawn has no tool-call identity")
        );
        let made = from_refusal("read_only: edit_file is not allowed");
        assert_eq!(made.code, "read_only");
        assert_eq!(made.message, "read_only: edit_file is not allowed");
        assert_eq!(made.class, AiToolOutcomeClass::GuardRefused);
        let made = from_refusal("A loop hook could not authorize this tool call.");
        assert_eq!(made.code, "tool_guard_refused");
    }

    #[test]
    fn a_timed_out_execution_failure_is_a_timeout() {
        let made = failure("exec_failed", "Tool x timed out after 5ms", None);
        assert_eq!(made.class, AiToolOutcomeClass::Timeout);
        assert!(made.next_step.contains("longer timeout"));
        assert_eq!(made.code, "exec_failed");
    }

    #[test]
    fn unlisted_codes_still_get_a_next_step() {
        let made = failure("made_up", "boom", None);
        assert_eq!(made.class, AiToolOutcomeClass::ToolError);
        assert!(!made.next_step.is_empty());
    }

    #[test]
    fn json_matches_the_contract_shape() {
        let value = failure_json(&failure("bad_args", "no", None));
        assert_eq!(value["class"], "invalid_json");
        assert_eq!(
            value["nextStep"],
            "Send the arguments as one JSON object and call the tool again."
        );
        assert!(value.get("cause").is_none());
        assert!(agentkit_contracts::AiToolFailure::from_json(value).is_ok());
    }
}
