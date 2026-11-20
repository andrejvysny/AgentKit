//! `tool_call` in the call pipeline (roadmap 2.3, CE-08, CE-09).
//!
//! The model's `tool_call {name, arguments}` is replaced, before guards run,
//! by the inner call `name(arguments)` under the SAME call id. From there it
//! is an ordinary call: resolve, normalise, validate, the run hooks, the
//! guards and approvals wrapped around the tool, receipts, events and the
//! call record all see the inner tool and its arguments, never `tool_call`.
//! That is the governance parity: nothing here decides who may call what.
//! A refusal at this stage (unknown name, invalid arguments) runs no guard
//! and no tool.
use crate::call_pipeline::{Prepared, closest_names, prepare_resolved, resolve_failure};
use crate::deferred::{TOOL_CALL, is_meta_tool};
use crate::{CoreError, ToolCall, ToolRegistry, envelope};
use serde_json::Value;

const CLOSEST: usize = 3;

/// The inner call of a well-formed `tool_call` (same id, the inner name and
/// arguments), as `prepare` builds it before normalisation. Hosts that
/// compare a declared call with an approval or a receipt use this, because
/// both name the inner tool.
pub fn unwrap_tool_call(call: &ToolCall) -> Option<ToolCall> {
    if call.name != TOOL_CALL {
        return None;
    }
    let args = agentkit_contracts::parse_javascript_json(&call.arguments_json).ok()?;
    let name = args["name"].as_str().filter(|name| !name.is_empty())?;
    let arguments = match &args["arguments"] {
        Value::Object(arguments) => arguments.clone(),
        Value::String(text) => agentkit_contracts::parse_javascript_json(text)
            .ok()?
            .as_object()?
            .clone(),
        _ => return None,
    };
    Some(ToolCall {
        id: call.id.clone(),
        name: name.to_owned(),
        arguments_json: envelope::stringify(&Value::Object(arguments)),
    })
}

pub(crate) fn prepare(registry: &ToolRegistry, call: &ToolCall) -> Prepared {
    let outer = || Prepared {
        call: call.clone(),
        outer: None,
        aliases_used: vec![],
        values_converted: 0,
        failure: None,
        schema_refusal: false,
    };
    let refuse = |error: CoreError| Prepared {
        failure: Some(error),
        ..outer()
    };
    let args = match agentkit_contracts::parse_javascript_json(&call.arguments_json) {
        Ok(args) => args,
        Err(error) => {
            return refuse(CoreError::new(
                "bad_args",
                &format!("Invalid arguments JSON: {error}. Send the arguments as one JSON object."),
            ));
        }
    };
    // `tool_call`'s own parameters are normalised like any tool's (a text
    // `arguments` that holds a JSON object becomes the object).
    let args = registry.normalise(TOOL_CALL, args).args;
    let Some(name) = args["name"].as_str().filter(|name| !name.is_empty()) else {
        return refuse(invalid(
            registry,
            TOOL_CALL,
            "name: a tool name is required",
        ));
    };
    let Some(arguments) = args["arguments"].as_object() else {
        return refuse(invalid(
            registry,
            TOOL_CALL,
            "arguments: an object is required (use {} for a tool with no parameters)",
        ));
    };
    if is_meta_tool(name) {
        return refuse(invalid(
            registry,
            TOOL_CALL,
            &format!("name: {name} is in your tool list; call it directly"),
        ));
    }
    // The inner name is a name the model was offered (T3.1); the registry
    // knows tools by canonical id. `prepare_resolved` translates the call.
    if let Some(error) = unknown(registry, &registry.canonical_name(name)) {
        return refuse(error);
    }
    let inner = ToolCall {
        id: call.id.clone(),
        name: name.to_owned(),
        arguments_json: envelope::stringify(&Value::Object(arguments.clone())),
    };
    let mut prepared = prepare_resolved(registry, &inner, true);
    if prepared.failure.is_none() {
        prepared.failure = invalid_inner(registry, &prepared.call);
        prepared.schema_refusal = prepared.failure.is_some();
    }
    prepared.outer = Some(call.clone());
    prepared
}

/// A name the session does not know, or knows but cannot run this turn.
/// A guard-withheld tool falls through: the pipeline refuses it exactly as it
/// refuses a direct call.
fn unknown(registry: &ToolRegistry, name: &str) -> Option<CoreError> {
    let catalog = registry.catalog()?;
    // A retired name is resolved (or taught) by the pipeline that follows.
    if registry.get(name).is_none() && registry.legacy_name(name).is_some() {
        return None;
    }
    let known = catalog.get(name).is_some() || registry.withheld_reason(name).is_some();
    if registry.get(name).is_some() {
        return None;
    }
    if registry.withheld_reason(name).is_some() {
        return Some(resolve_failure(registry, name));
    }
    if known {
        return Some(CoreError::new(
            "tool_unavailable",
            &format!(
                "Tool {name} is in this session's catalog but no contributor offers it right now. Use another tool, or try {name} again in a later turn."
            ),
        ));
    }
    let nearest = closest_names(name, &catalog.names(), CLOSEST);
    Some(CoreError::new(
        "tool_not_found",
        &format!(
            "No tool named {name}. The closest names are: {}. Call tool_search to find the right one.",
            nearest.join(", ")
        ),
    ))
}

/// CE-09: arguments that do not fit the target schema are refused here, with
/// the schema, before any guard or tool runs.
fn invalid_inner(registry: &ToolRegistry, call: &ToolCall) -> Option<CoreError> {
    let args = agentkit_contracts::parse_javascript_json(&call.arguments_json).ok()?;
    let errors = registry.validate_input(&call.name, &args);
    (!errors.is_empty()).then(|| invalid(registry, &call.name, &errors.join("; ")))
}

fn invalid(registry: &ToolRegistry, tool: &str, errors: &str) -> CoreError {
    let schema = registry
        .get(tool)
        .map(|tool| envelope::stringify(&tool.definition().input_schema))
        .unwrap_or_default();
    CoreError::new(
        "invalid_arguments",
        &format!(
            "Invalid arguments for {tool}: {errors}. Its input schema is {schema}. Fix the arguments and call tool_call again."
        ),
    )
}
