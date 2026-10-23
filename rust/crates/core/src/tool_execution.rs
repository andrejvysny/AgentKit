use crate::{
    budget::minimum, envelope, CoreError, ExecutionBudget, ToolCall, ToolContext, ToolRegistry,
};
use serde_json::{json, Value};
use std::{collections::HashSet, sync::Arc, time::Duration};

pub(crate) struct ToolOutcome {
    pub kind: &'static str,
    pub data: Value,
    pub model_json: String,
}
pub(crate) fn failed(
    call: &ToolCall,
    error: &CoreError,
    max_bytes: usize,
    detail: Value,
) -> ToolOutcome {
    let message = envelope::truncate(&error.message, envelope::text_budget(max_bytes)).0;
    ToolOutcome {
        kind: "run.tool.failed",
        data: json!({"toolCallId":call.id,"toolName":call.name,"errorMessage":message,"errorCode":error.code}),
        model_json: envelope::stringify(&envelope::error_envelope(&error.code, &message, detail)),
    }
}
pub(crate) async fn execute(
    registry: &ToolRegistry,
    call: &ToolCall,
    context: ToolContext,
    default_timeout: Option<u64>,
    budget: Option<Arc<ExecutionBudget>>,
) -> ToolOutcome {
    let max = context.limits.max_bytes;
    let Some(tool) = registry.get(&call.name) else {
        return failed(
            call,
            &CoreError::new(
                "tool_missing",
                &format!("Tool not registered: {}", call.name),
            ),
            max,
            json!({}),
        );
    };
    let args = match agentkit_contracts::parse_javascript_json(&call.arguments_json) {
        Ok(args) => args,
        Err(error) => {
            return failed(
                call,
                &CoreError::new("bad_args", &format!("Invalid arguments JSON: {error}")),
                max,
                json!({}),
            );
        }
    };
    let errors = registry.validate_input(&call.name, &args);
    if !errors.is_empty() {
        let message = format!(
            "Invalid arguments for {}: {}. Fix the arguments and call again.",
            call.name,
            errors.join("; ")
        );
        return failed(
            call,
            &CoreError::new("schema_invalid", &message),
            max,
            json!({"phase":"validation","retryable":false}),
        );
    }
    if let Some(budget) = &budget {
        if let Err(error) = budget.consume("toolCalls").await {
            return failed(call, &error, max, json!({}));
        }
    }
    if let Some(guard) = &context.dispatch_guard {
        if let Err(error) = guard.check().await {
            context.cancellation.cancel();
            return failed(call, &error, max, json!({}));
        }
    }
    if let Some(budget) = &budget {
        if let Err(error) = budget.check() {
            return failed(call, &error, max, json!({}));
        }
    }
    if context.cancellation.is_cancelled() {
        return failed(
            call,
            &CoreError::new(
                "cancelled",
                "Run cancelled before this tool produced a result.",
            ),
            max,
            json!({}),
        );
    }
    let declared = tool
        .definition()
        .timeout_ms
        .filter(|value| *value > 0)
        .or(default_timeout);
    let budget_deadline = budget
        .as_ref()
        .and_then(|value| minimum(value.limits.tool_ms, value.remaining_ms()));
    let deadline = minimum(
        declared,
        budget
            .as_ref()
            .and_then(|value| minimum(value.limits.tool_ms, value.remaining_ms())),
    );
    let child = context.cancellation.child_token();
    let mut execute_context = context.clone();
    execute_context.cancellation = child.clone();
    let running = tool.execute(execute_context, args);
    tokio::pin!(running);
    let result = if let Some(ms) = deadline {
        tokio::select! { biased;
            result = &mut running => result,
            _ = tokio::time::sleep(Duration::from_millis(ms)) => {
                child.cancel();
                if budget_deadline.is_some_and(|limit|limit<=ms) {
                    let phase=if budget.as_ref().and_then(|budget|budget.remaining_ms())==Some(0) {"overall"} else {"tool"};
                    let error=crate::budget::timeout(phase);
                    if let Some(budget)=&budget {budget.fail(error.clone());}
                    Err(error)
                } else {Err(CoreError::new("exec_failed",&format!("Tool {} timed out after {ms}ms",call.name)))}
            }
            _ = context.cancellation.cancelled() => {
                child.cancel(); Err(CoreError::new("cancelled","Run cancelled before this tool produced a result."))
            }
        }
    } else {
        tokio::select! { biased;
            result = &mut running => result,
            _ = context.cancellation.cancelled() => { child.cancel(); Err(CoreError::new("cancelled","Run cancelled before this tool produced a result.")) }
        }
    };
    outcome(call, result, max, &context.limits)
}
fn outcome(
    call: &ToolCall,
    result: Result<crate::ToolResult, CoreError>,
    max: usize,
    limits: &crate::ToolLimits,
) -> ToolOutcome {
    match result {
        Err(error) => failed(
            call,
            &CoreError {
                code: if matches!(
                    error.code.as_str(),
                    "cancelled"
                        | "result_unserializable"
                        | "execution_timeout"
                        | "execution_budget_exhausted"
                ) {
                    error.code.clone()
                } else {
                    "exec_failed".into()
                },
                ..error.clone()
            },
            max,
            json!({"phase":"execution","retryable":error.retryable}),
        ),
        Ok(result) => {
            let model = envelope::build(&result, limits);
            let model_json = envelope::stringify(&model);
            let mut data = json!({"toolCallId":call.id,"toolName":call.name,"modelResultJson":model_json,"status":model["status"]});
            if result.ok {
                data["resultJson"] =
                    envelope::stringify(&result.data.unwrap_or(Value::Null)).into();
                data["sources"] = result.sources.into();
                data["truncated"] = model["truncated"].clone();
                data["warnings"] = result.warnings.into();
                if let Some(summary) = result.summary {
                    data["summary"] = summary.into();
                }
            } else {
                let message = result.summary.unwrap_or_else(|| {
                    if result.warnings.is_empty() {
                        format!("Tool {} reported failure", call.name)
                    } else {
                        result.warnings.join("; ")
                    }
                });
                data["errorMessage"] = envelope::truncate(&message, envelope::text_budget(max))
                    .0
                    .into();
                data["errorCode"] = "tool_failed".into();
            }
            ToolOutcome {
                kind: if result.ok {
                    "run.tool.succeeded"
                } else {
                    "run.tool.failed"
                },
                data,
                model_json,
            }
        }
    }
}

pub(crate) fn dedupe(calls: &mut [ToolCall]) -> Vec<String> {
    let reserved: HashSet<String> = calls.iter().map(|call| call.id.clone()).collect();
    let mut used = HashSet::new();
    let mut duplicates = vec![];
    for call in calls {
        if used.insert(call.id.clone()) {
            continue;
        }
        let original = call.id.clone();
        if !duplicates.contains(&original) {
            duplicates.push(original.clone());
        }
        let mut count = 2;
        loop {
            let candidate = format!("{original}#{count}");
            count += 1;
            if !reserved.contains(&candidate) && used.insert(candidate.clone()) {
                call.id = candidate;
                break;
            }
        }
    }
    duplicates
}
