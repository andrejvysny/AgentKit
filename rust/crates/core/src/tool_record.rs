//! Per-call tool record (TU-35): classifies each terminal tool outcome.
//! Holds counts and sizes only, never argument or result content.
use agentkit_contracts::AiToolOutcomeClass as Class;
use serde_json::{Value, json};
use std::collections::HashMap;

/// Failures of one tool in a row, all in one class (the repair budget).
#[derive(Clone, Copy)]
struct Streak {
    class: Class,
    length: u32,
}

#[derive(Default)]
pub(crate) struct ToolRecords {
    /// Failures per tool name over the run; a later success is a repair.
    failures: HashMap<String, u32>,
    /// Times each identical failing call has failed (tool name, arguments).
    failed_calls: HashMap<(String, String), u32>,
    streaks: HashMap<String, Streak>,
}

/// Run-wide identity stamped on every tool-call record: what the call loop
/// itself cannot know (provider, lane, session profile).
#[derive(Clone, Debug, Default)]
pub struct RecordIdentity {
    pub provider: Option<String>,
    pub lane: Option<String>,
    pub profile: Option<String>,
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

/// The part of a finished call that decides its class.
pub(crate) struct Judged<'a> {
    pub tool_name: &'a str,
    pub arguments_json: &'a str,
    pub succeeded: bool,
    pub error_code: Option<&'a str>,
    pub error_message: Option<&'a str>,
}

impl<'a> From<&Finished<'a>> for Judged<'a> {
    fn from(finished: &Finished<'a>) -> Self {
        Self {
            tool_name: finished.tool_name,
            arguments_json: finished.arguments_json,
            succeeded: finished.succeeded,
            error_code: finished.error_code,
            error_message: finished.error_message,
        }
    }
}

/// What one finished call means for the run: its class, the failures before
/// it, and whether the repair budget says to stop.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Verdict {
    pub class: Class,
    pub prior: u32,
    pub stop_retrying: bool,
}

/// Codes that say nothing about the model's call, so they never spend the
/// repair budget.
pub(crate) fn outside_budget(code: Option<&str>) -> bool {
    matches!(
        code,
        Some("cancelled" | "tool_call_cap" | "execution_budget_exhausted" | "execution_timeout")
    )
}

/// What makes two calls identical for `repeat_identical`: the tool name and
/// the exact arguments text.
pub(crate) fn call_identity(tool: &str, arguments_json: &str) -> (String, String) {
    (tool.to_string(), arguments_json.to_string())
}

impl ToolRecords {
    /// Classify a finished call and update the run's counters. Runs for every
    /// call, whether or not records are emitted, because the repair budget
    /// depends on it.
    pub(crate) fn judge(&mut self, call: &Judged<'_>) -> Verdict {
        let failure = (!call.succeeded).then(|| {
            (
                classify(call.error_code, call.error_message),
                !outside_budget(call.error_code),
            )
        });
        self.decide(call.tool_name, call.arguments_json, failure)
    }

    /// The shared rules: `failure` is the failure's class and whether it
    /// spends the repair budget, or `None` for a success.
    fn decide(
        &mut self,
        tool: &str,
        arguments_json: &str,
        failure: Option<(Class, bool)>,
    ) -> Verdict {
        let tool = tool.to_string();
        let key = call_identity(&tool, arguments_json);
        let prior = self.failures.get(&tool).copied().unwrap_or(0);
        let Some((base, spends)) = failure else {
            self.streaks.remove(&tool);
            return Verdict {
                class: if prior > 0 {
                    Class::OkAfterRepair
                } else {
                    Class::Ok
                },
                prior,
                stop_retrying: false,
            };
        };
        // The identical third call is a repeat; the second only fails again.
        let repeats = self.failed_calls.get(&key).copied().unwrap_or(0);
        let class = if repeats >= 2 {
            Class::RepeatIdentical
        } else {
            base
        };
        *self.failures.entry(tool.clone()).or_default() += 1;
        *self.failed_calls.entry(key).or_default() += 1;
        let length = match self.streaks.get(&tool) {
            Some(streak) if streak.class == base => streak.length + 1,
            _ => 1,
        };
        if spends {
            self.streaks.insert(
                tool,
                Streak {
                    class: base,
                    length,
                },
            );
        }
        Verdict {
            class,
            prior,
            stop_retrying: spends && length >= 2,
        }
    }

    pub(crate) fn data(&self, finished: &Finished<'_>, verdict: &Verdict) -> Value {
        let mut data = json!({
            "toolCallId": finished.tool_call_id,
            "toolName": finished.tool_name,
            "step": finished.step,
            "model": finished.model,
            "outcomeClass": verdict.class,
            "aliasesUsed": finished.aliases_used,
            "valuesConverted": finished.values_converted,
            "salvaged": false,
            "repairAttempt": verdict.prior,
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

    #[cfg(test)]
    pub(crate) fn record(&mut self, finished: &Finished<'_>) -> Value {
        let verdict = self.judge(&Judged::from(finished));
        self.data(finished, &verdict)
    }
}

/// How a finished call failed, for [`OutcomeClassifier`].
#[derive(Clone, Copy, Debug)]
pub enum CallFailure<'a> {
    /// A failure code and message; the failure table decides the class.
    Code { code: &'a str, message: &'a str },
    /// A class the caller already knows (a harness with its own checks).
    Class(Class),
}

/// The T0.1 outcome rules as public API (T0.3): classes of a run's calls in
/// order, `ok_after_repair` after an earlier failure of the same tool,
/// `repeat_identical` on the third identical failing call, and the repair
/// budget's stop. The run loop and the conformance runner share it, so a
/// harness classifies exactly as the run record does.
#[derive(Default)]
pub struct OutcomeClassifier {
    records: ToolRecords,
}

impl OutcomeClassifier {
    pub fn new() -> Self {
        Self::default()
    }

    /// Judge one finished call (`failure` is `None` when it succeeded) and
    /// update the run's counters.
    pub fn judge(
        &mut self,
        tool: &str,
        arguments_json: &str,
        failure: Option<CallFailure<'_>>,
    ) -> Verdict {
        let failure = failure.map(|failure| match failure {
            CallFailure::Code { code, message } => (
                classify(Some(code), Some(message)),
                !outside_budget(Some(code)),
            ),
            CallFailure::Class(class) => (class, true),
        });
        self.records.decide(tool, arguments_json, failure)
    }
}

fn classify(code: Option<&str>, message: Option<&str>) -> Class {
    crate::failure_catalog::class_of(code.unwrap_or(""), message.unwrap_or(""))
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
    fn success_after_failure_is_repair_and_third_identical_failure_repeats() {
        let mut records = ToolRecords::default();
        let first = records.record(&finished(false, Some("schema_invalid"), "{}"));
        assert_eq!(first["outcomeClass"], "invalid_structure");
        let again = records.record(&finished(false, Some("schema_invalid"), "{}"));
        assert_eq!(again["outcomeClass"], "invalid_structure");
        assert_eq!(again["repairAttempt"], 1);
        let third = records.record(&finished(false, Some("schema_invalid"), "{}"));
        assert_eq!(third["outcomeClass"], "repeat_identical");
        assert_eq!(third["repairAttempt"], 2);
        assert_eq!(third["failureCode"], "schema_invalid");
        let fixed = records.record(&finished(true, None, r#"{"a":1}"#));
        assert_eq!(fixed["outcomeClass"], "ok_after_repair");
        assert_eq!(fixed["repairAttempt"], 3);
        assert!(fixed.get("failureCode").is_none());
    }

    #[test]
    fn second_failure_in_one_class_stops_retrying_and_the_third_stays_stopped() {
        let mut records = ToolRecords::default();
        let first = records.judge(&Judged::from(&finished(
            false,
            Some("schema_invalid"),
            r#"{"a":1}"#,
        )));
        assert!(!first.stop_retrying);
        let second = records.judge(&Judged::from(&finished(
            false,
            Some("schema_invalid"),
            r#"{"a":2}"#,
        )));
        assert!(second.stop_retrying);
        let third = records.judge(&Judged::from(&finished(
            false,
            Some("schema_invalid"),
            r#"{"a":3}"#,
        )));
        assert!(third.stop_retrying);
        assert_eq!(third.class, Class::InvalidStructure);
    }

    #[test]
    fn a_different_class_a_success_or_a_cancel_does_not_extend_the_streak() {
        let mut records = ToolRecords::default();
        records.judge(&Judged::from(&finished(
            false,
            Some("schema_invalid"),
            "{}",
        )));
        let other = records.judge(&Judged::from(&finished(false, Some("bad_args"), "{}")));
        assert!(!other.stop_retrying, "a new class starts a new streak");
        records.judge(&Judged::from(&finished(true, None, "{}")));
        let after_success = records.judge(&Judged::from(&finished(false, Some("bad_args"), "{}")));
        assert!(!after_success.stop_retrying, "success resets the streak");
        let cancel = records.judge(&Judged::from(&finished(false, Some("cancelled"), "{}")));
        assert!(!cancel.stop_retrying, "cancelled never spends the budget");
    }

    #[test]
    fn the_budget_counts_each_tool_on_its_own() {
        let mut records = ToolRecords::default();
        records.judge(&Judged::from(&finished(
            false,
            Some("schema_invalid"),
            "{}",
        )));
        let mut other = finished(false, Some("schema_invalid"), "{}");
        other.tool_name = "read";
        assert!(!records.judge(&Judged::from(&other)).stop_retrying);
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
