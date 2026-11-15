//! Per-call tool record (TU-35): classifies each terminal tool outcome.
//! Holds counts and sizes only, never argument or result content.
use agentkit_contracts::AiToolOutcomeClass as Class;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};

#[derive(Default)]
pub(crate) struct ToolRecords {
    /// Failures per tool name over the run; a later success is a repair.
    failures: HashMap<String, u32>,
    failed_calls: HashSet<(String, String)>,
}

pub(crate) struct Finished<'a> {
    pub tool_call_id: &'a str,
    pub tool_name: &'a str,
    pub arguments_json: &'a str,
    pub model: &'a str,
    pub step: u32,
    pub succeeded: bool,
    pub error_code: Option<&'a str>,
    pub error_message: Option<&'a str>,
    pub duration_ms: u64,
    pub result_bytes: usize,
    pub aliases_used: &'a [String],
    pub values_converted: u32,
}

impl ToolRecords {
    pub(crate) fn record(&mut self, finished: &Finished<'_>) -> Value {
        let key = (
            finished.tool_name.to_string(),
            finished.arguments_json.to_string(),
        );
        let prior = self.failures.get(finished.tool_name).copied().unwrap_or(0);
        let class = if finished.succeeded {
            if prior > 0 {
                Class::OkAfterRepair
            } else {
                Class::Ok
            }
        } else if self.failed_calls.contains(&key) {
            Class::RepeatIdentical
        } else {
            classify(finished.error_code, finished.error_message)
        };
        if !finished.succeeded {
            *self
                .failures
                .entry(finished.tool_name.to_string())
                .or_default() += 1;
            self.failed_calls.insert(key);
        }
        let mut data = json!({
            "toolCallId": finished.tool_call_id,
            "toolName": finished.tool_name,
            "step": finished.step,
            "model": finished.model,
            "outcomeClass": class,
            "aliasesUsed": finished.aliases_used,
            "valuesConverted": finished.values_converted,
            "salvaged": false,
            "repairAttempt": prior,
            "durationMs": finished.duration_ms,
            "argumentBytes": finished.arguments_json.len(),
            "resultBytes": finished.result_bytes,
        });
        if !finished.succeeded
            && let Some(code) = finished.error_code
        {
            data["failureCode"] = code.into();
        }
        data
    }
}

fn classify(code: Option<&str>, message: Option<&str>) -> Class {
    match code {
        Some("tool_missing") => Class::UnknownTool,
        Some("bad_args") => Class::InvalidJson,
        Some("schema_invalid" | "tool_name_missing") => Class::InvalidStructure,
        Some("tool_hook_denied" | "lease_lost" | "tool_not_allowed") => Class::GuardRefused,
        Some("execution_timeout" | "execution_budget_exhausted") => Class::Timeout,
        Some("exec_failed") if message.is_some_and(|text| text.contains("timed out after")) => {
            Class::Timeout
        }
        _ => Class::ToolError,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn finished<'a>(ok: bool, code: Option<&'a str>, args: &'a str) -> Finished<'a> {
        Finished {
            tool_call_id: "c",
            tool_name: "edit",
            arguments_json: args,
            model: "m",
            step: 1,
            succeeded: ok,
            error_code: code,
            error_message: None,
            duration_ms: 1,
            result_bytes: 2,
            aliases_used: &[],
            values_converted: 0,
        }
    }

    #[test]
    fn success_after_failure_is_repair_and_same_failure_repeats() {
        let mut records = ToolRecords::default();
        let first = records.record(&finished(false, Some("schema_invalid"), "{}"));
        assert_eq!(first["outcomeClass"], "invalid_structure");
        let again = records.record(&finished(false, Some("schema_invalid"), "{}"));
        assert_eq!(again["outcomeClass"], "repeat_identical");
        assert_eq!(again["repairAttempt"], 1);
        let fixed = records.record(&finished(true, None, r#"{"a":1}"#));
        assert_eq!(fixed["outcomeClass"], "ok_after_repair");
        assert_eq!(fixed["repairAttempt"], 2);
        assert!(fixed.get("failureCode").is_none());
    }

    #[test]
    fn aliases_and_conversions_are_recorded() {
        let mut records = ToolRecords::default();
        let aliases = vec!["path->file_path".to_string()];
        let mut call = finished(true, None, "{}");
        call.aliases_used = &aliases;
        call.values_converted = 2;
        let data = records.record(&call);
        assert_eq!(data["aliasesUsed"], json!(["path->file_path"]));
        assert_eq!(data["valuesConverted"], 2);
    }

    #[test]
    fn timeouts_and_guards_have_their_own_classes() {
        assert_eq!(classify(Some("execution_timeout"), None), Class::Timeout);
        assert_eq!(
            classify(Some("exec_failed"), Some("Tool x timed out after 5ms")),
            Class::Timeout
        );
        assert_eq!(
            classify(Some("tool_hook_denied"), None),
            Class::GuardRefused
        );
        assert_eq!(
            classify(Some("exec_failed"), Some("boom")),
            Class::ToolError
        );
    }
}
