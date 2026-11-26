//! The result envelope, as the coding pack's: a user-level error is an
//! `ok: false` result whose text starts `<code>: <message>`, with the code
//! in `data.errorCode`; a tool never throws for one. A refusal before a
//! sandbox operation's row exists carries the ledger's not-started shape
//! (`errorCode: command_not_started`, `phase: before_spawn`,
//! `started: false`), so the receipt is completed as not started. An
//! unknown outcome never does.

use agentkit_core::failure_catalog::{failure, failure_json, render};
use agentkit_core::{ToolLimits, ToolResult};
use serde_json::{Map, Value, json};

/// A refusal raised before anything ran: a code and the text after it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Refusal {
    pub(crate) code: String,
    pub(crate) message: String,
}

impl Refusal {
    pub(crate) fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.to_owned(),
            message: message.into(),
        }
    }
}

impl From<agentkit_workspaces::SandboxError> for Refusal {
    fn from(error: agentkit_workspaces::SandboxError) -> Self {
        Self::new(error.code(), error.to_string())
    }
}

fn merged(base: Value, extra: Value) -> Value {
    let (Value::Object(mut base), Value::Object(extra)) = (base, extra) else {
        return Value::Null;
    };
    for (key, value) in extra {
        base.insert(key, value);
    }
    Value::Object(base)
}

/// An `ok: false` result for `code`. `extra` joins the structured data.
pub(crate) fn failed(code: &str, message: &str, extra: Value, limits: &ToolLimits) -> ToolResult {
    let made = failure(code, &format!("{code}: {message}"), None);
    let text = render(&made);
    let data = json!({
        "errorCode": code,
        "errorMessage": message,
        "phase": "execution",
        "retryable": made.retryable,
        "failure": failure_json(&made),
    });
    result(false, text.clone(), Some(text), merged(data, extra), limits)
}

/// A refusal of a sandbox operation before its row existed: nothing ran.
pub(crate) fn not_started(refusal: &Refusal, limits: &ToolLimits) -> ToolResult {
    let made = failure(
        &refusal.code,
        &format!("{}: {}", refusal.code, refusal.message),
        None,
    );
    let text = render(&made);
    let data = json!({
        "errorCode": "command_not_started",
        "code": refusal.code,
        "errorMessage": refusal.message,
        "started": false,
        "phase": "before_spawn",
        "retryable": false,
        "freshApprovalRequired": true,
        "failure": failure_json(&made),
    });
    result(false, text.clone(), Some(text), data, limits)
}

/// A result the model reads as `text`; `data` is the structured part.
pub(crate) fn result(
    ok: bool,
    text: String,
    summary: Option<String>,
    data: Value,
    limits: &ToolLimits,
) -> ToolResult {
    ToolResult {
        ok,
        data: Some(data),
        model_data: Some(Value::String(text)),
        sources: vec![],
        warnings: vec![],
        truncated: false,
        limits: limits.clone(),
        summary,
        status: None,
        model_content: None,
    }
}

/// A `ok: true` result.
pub(crate) fn ok(text: String, data: Map<String, Value>, limits: &ToolLimits) -> ToolResult {
    let summary = text.lines().next().map(str::to_owned);
    result(true, text, summary, Value::Object(data), limits)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn not_started_is_the_ledger_no_start_shape_and_names_the_real_code() {
        let refusal = Refusal::new("sandbox_busy", "sandbox is busy: Researcher holds it");
        let result = not_started(&refusal, &ToolLimits::default());
        assert!(!result.ok);
        let data = result.data.unwrap();
        assert_eq!(data["errorCode"], "command_not_started");
        assert_eq!(data["phase"], "before_spawn");
        assert_eq!(data["started"], false);
        assert_eq!(data["code"], "sandbox_busy");
        let text = result.model_data.unwrap();
        assert!(
            text.as_str()
                .unwrap()
                .starts_with("sandbox_busy: sandbox is busy")
        );
    }

    #[test]
    fn a_failure_keeps_its_code_and_extra_fields() {
        let result = failed(
            "export_stale",
            "changed",
            json!({"path": "/workspace/a"}),
            &ToolLimits::default(),
        );
        let data = result.data.unwrap();
        assert_eq!(data["errorCode"], "export_stale");
        assert_eq!(data["path"], "/workspace/a");
        assert_ne!(data["phase"], "before_spawn");
    }
}
