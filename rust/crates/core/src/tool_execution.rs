use crate::{
    CoreError, ExecutionBudget, ToolCall, ToolContext, ToolRegistry, budget::minimum, envelope,
};
use serde_json::{Value, json};
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
/// Repair budget (TU-11): add the stop line to a failed result's summary, so
/// the model sees it next to the failure it belongs to.
pub(crate) fn append_stop_retrying(outcome: &mut ToolOutcome) {
    let Ok(mut model) = serde_json::from_str::<Value>(&outcome.model_json) else {
        return;
    };
    let current = model["summary"]
        .as_str()
        .or_else(|| outcome.data["errorMessage"].as_str())
        .unwrap_or("The tool call failed.")
        .to_owned();
    model["summary"] = crate::failure_catalog::with_stop_retrying(&current).into();
    outcome.model_json = envelope::stringify(&model);
}
const CUT_OFF_HINT: &str = "Your output reached its limit before this call was complete. Send the content in smaller parts, for example several shorter calls.";
/// A call whose arguments broke because the output limit cut the turn: tell
/// the model why, so it splits the work instead of resending the same call.
pub(crate) fn append_cut_off_hint(outcome: &mut ToolOutcome) {
    let Ok(mut model) = serde_json::from_str::<Value>(&outcome.model_json) else {
        return;
    };
    let current = model["summary"]
        .as_str()
        .or_else(|| outcome.data["errorMessage"].as_str())
        .unwrap_or("The tool call failed.")
        .to_owned();
    model["summary"] = format!("{current}\n{CUT_OFF_HINT}").into();
    outcome.model_json = envelope::stringify(&model);
}
pub(crate) async fn execute(
    registry: &ToolRegistry,
    call: &ToolCall,
    context: ToolContext,
    default_timeout: Option<u64>,
    budget: Option<Arc<ExecutionBudget>>,
    close: Option<tokio_util::sync::CancellationToken>,
    output: Option<Arc<crate::OutputGate>>,
) -> ToolOutcome {
    let max = context.limits.max_bytes;
    let Some(tool) = registry.get(&call.name) else {
        return failed(
            call,
            &crate::call_pipeline::resolve_failure(registry, &call.name),
            max,
            json!({}),
        );
    };
    let args = match agentkit_contracts::parse_javascript_json(&call.arguments_json) {
        Ok(args) => args,
        Err(error) => {
            return failed(
                call,
                &CoreError::new(
                    "bad_args",
                    &format!(
                        "Invalid arguments JSON: {error}. Send the arguments as one JSON object."
                    ),
                ),
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
        // Audit symmetry (the durable-host gap): a wrapper that records
        // receipts for this tool records the pre-body refusal too, so the
        // execution ledger has a row for it. The model-facing outcome is
        // exactly the unchanged `schema_invalid` failure below.
        tool.record_schema_refusal(&context, &args, &message).await;
        return failed(
            call,
            &CoreError::new("schema_invalid", &message),
            max,
            json!({"phase":"validation","retryable":false}),
        );
    }
    if let Some(budget) = &budget
        && let Err(error) = budget.consume("toolCalls").await
    {
        return failed(call, &error, max, json!({}));
    }
    if let Some(guard) = &context.dispatch_guard
        && let Err(error) = guard.check().await
    {
        context.cancellation.cancel();
        return failed(call, &error, max, json!({}));
    }
    if let Some(budget) = &budget
        && let Err(error) = budget.check()
    {
        return failed(call, &error, max, json!({}));
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
    let result = if let (None, Some(close)) = (&budget, &close) {
        wait_unbudgeted(&mut running, &child, deadline, close, &call.name).await
    } else if let Some(ms) = deadline {
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
            _ = context.cancellation.cancelled() => { child.cancel(); Err(cancelled()) }
            _ = closed(&close) => { child.cancel(); Err(cancelled()) }
        }
    } else {
        tokio::select! { biased;
            result = &mut running => result,
            _ = context.cancellation.cancelled() => { child.cancel(); Err(cancelled()) }
            _ = closed(&close) => { child.cancel(); Err(cancelled()) }
        }
    };
    let result = render_failure_text(registry, result);
    let session = context
        .scope_id
        .as_deref()
        .or(context.chat_id.as_deref())
        .filter(|session| !session.is_empty())
        // No chat or scope: the run is its own session, so the output is
        // still stored (and removed with the task's chat).
        .unwrap_or(context.run_id.as_str());
    // Roadmap 2.1: a result over its inline limit is stored before the model
    // first sees it; the envelope then carries the preview and the handle.
    let result = match (&output, result) {
        (Some(gate), Ok(result)) => Ok(gate
            .shape(
                crate::OutputCall {
                    session,
                    task: &context.run_id,
                    call_id: &call.id,
                    tool: &call.name,
                    max_bytes: max,
                },
                result,
            )
            .await),
        (_, result) => result,
    };
    outcome(call, result, max, &context.limits)
}
/// Fills the placeholders a tool left in its result text (`{{tool:..}}`,
/// `{{call:..}}`) with the names the session's dialect offered, so a teaching
/// error never names a tool or parameter the model cannot see.
fn render_failure_text(
    registry: &ToolRegistry,
    result: Result<crate::ToolResult, CoreError>,
) -> Result<crate::ToolResult, CoreError> {
    let dialect = registry.dialect();
    let render = |text: &mut String| {
        if text.contains("{{") {
            *text = crate::dialect::render_text(dialect, text);
        }
    };
    match result {
        Ok(mut result) => {
            if let Some(summary) = &mut result.summary {
                render(summary);
            }
            result.warnings.iter_mut().for_each(render);
            render_value(result.data.as_mut(), dialect);
            render_value(result.model_data.as_mut(), dialect);
            Ok(result)
        }
        Err(mut error) => {
            render(&mut error.message);
            Err(error)
        }
    }
}
fn render_value(value: Option<&mut Value>, dialect: Option<&crate::Dialect>) {
    match value {
        Some(Value::String(text)) if text.contains("{{") => {
            *text = crate::dialect::render_text(dialect, text);
        }
        Some(Value::Array(items)) => items
            .iter_mut()
            .for_each(|item| render_value(Some(item), dialect)),
        Some(Value::Object(map)) => map
            .values_mut()
            .for_each(|item| render_value(Some(item), dialect)),
        _ => {}
    }
}
async fn closed(close: &Option<tokio_util::sync::CancellationToken>) {
    match close {
        Some(close) => close.cancelled().await,
        None => std::future::pending::<()>().await,
    }
}
fn cancelled() -> CoreError {
    CoreError::new(
        "cancelled",
        "Run cancelled before this tool produced a result.",
    )
}
async fn wait_unbudgeted<F>(
    running: &mut F,
    child: &tokio_util::sync::CancellationToken,
    deadline: Option<u64>,
    close: &tokio_util::sync::CancellationToken,
    name: &str,
) -> Result<crate::ToolResult, CoreError>
where
    F: std::future::Future<Output = Result<crate::ToolResult, CoreError>> + Unpin,
{
    tokio::select! { biased;
        result = running => result,
        _ = async { if let Some(ms) = deadline { tokio::time::sleep(Duration::from_millis(ms)).await } else { std::future::pending::<()>().await } } => {
            child.cancel();
            Err(CoreError::new("exec_failed", &format!("Tool {name} timed out after {}ms", deadline.unwrap_or(0))))
        },
        _ = close.cancelled() => { child.cancel(); Err(cancelled()) },
    }
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
                // A tool that carries the failure type reports its own code
                // and one-sentence message; the model text stays the summary.
                let carried = carried_failure(&result.data);
                let message = carried
                    .and_then(|failure| failure["message"].as_str().map(str::to_owned))
                    .unwrap_or(message);
                data["errorMessage"] = envelope::truncate(&message, envelope::text_budget(max))
                    .0
                    .into();
                data["errorCode"] = carried
                    .and_then(|failure| failure["code"].as_str())
                    .unwrap_or("tool_failed")
                    .into();
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

/// The `failure` object a tool put in its structured data, when it did.
fn carried_failure(data: &Option<Value>) -> Option<&Value> {
    data.as_ref()
        .and_then(|data| data.get("failure"))
        .filter(|failure| failure["code"].is_string())
}

pub fn dedupe(calls: &mut [ToolCall]) -> Vec<String> {
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
