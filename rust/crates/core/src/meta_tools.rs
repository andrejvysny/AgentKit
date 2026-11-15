//! The three meta tools (roadmap 2.3, ADR 0031): `tool_search`,
//! `tool_describe`, `tool_call`. They have no authority of their own.
//! `tool_search` and `tool_describe` read the session catalog. `tool_call` is
//! unwrapped by the call pipeline before guards run (see `call_pipeline`), so
//! the inner tool is resolved, normalised, validated, guarded, approved and
//! recorded exactly like a direct call; its own `execute` is only reached by
//! a host that does not run the pipeline, and refuses.
use crate::deferred::{
    Catalog, CatalogCell, CatalogEntry, TOOL_CALL, TOOL_DESCRIBE, TOOL_SEARCH, first_sentence,
    parameter_names, required_names,
};
use crate::failure_catalog::{failure, failure_result};
use crate::{CoreError, Tool, ToolContext, ToolDefinition, ToolLimits, ToolResult, closest_names};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::sync::Arc;

const DEFAULT_LIMIT: usize = 5;
const MAX_LIMIT: usize = 8;
const SUMMARY_CHARS: usize = 160;

fn definition(name: &str, description: &str, schema: Value, example: Value) -> ToolDefinition {
    ToolDefinition {
        name: name.into(),
        version: "1".into(),
        // The contract's effect is read or write. The meta tools hold no
        // authority: the inner tool's own effect and guards decide.
        effect: "read".into(),
        capability: "tools.meta".into(),
        description: description.into(),
        input_schema: schema,
        output_schema: None,
        timeout_ms: None,
        examples: vec![example],
    }
}

pub fn tool_search_definition() -> ToolDefinition {
    definition(
        TOOL_SEARCH,
        "Find tools that are not in your tool list. Describe what you want to do in a few words, such as \"calendar events\" or \"read page\"; matches are ranked by tool name, parameter names and description. Each match shows the name, a short description, the effect and the parameter names (a star marks a required one). Call tool_describe for a full schema, then tool_call to run the tool.",
        json!({
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "A few words about what you want to do."},
                "limit": {"type": "integer", "minimum": 1, "maximum": 8, "description": "How many matches to return. Default 5."},
                "namespace": {"type": "string", "description": "Only match tools of this namespace, as listed in the system prompt."}
            },
            "required": ["query"],
            "additionalProperties": false
        }),
        json!({"query": "calendar events", "limit": 3}),
    )
}

pub fn tool_describe_definition() -> ToolDefinition {
    definition(
        TOOL_DESCRIBE,
        "Show the full definition of one tool by its exact name: description, effect, input schema and example calls. Use it on a tool found with tool_search before you run it with tool_call, so the arguments match the schema. It also works for tools already in your tool list. An unknown name returns the closest names.",
        json!({
            "type": "object",
            "properties": {
                "name": {"type": "string", "description": "The exact tool name."}
            },
            "required": ["name"],
            "additionalProperties": false
        }),
        json!({"name": "calendar_list_events"}),
    )
}

/// `arguments` carries another tool's argument object, whatever its schema
/// says, so it is the one open object among the meta tools' parameters. The
/// inner schema is checked by the pipeline, not by this one.
pub fn tool_call_definition() -> ToolDefinition {
    definition(
        TOOL_CALL,
        "Run a tool that is not in your tool list, by its exact name, with the arguments its schema asks for. Find the tool with tool_search and read its schema with tool_describe first. The call is checked and run exactly as if you had called the tool directly: the same approvals and refusals apply, and an approval names the inner tool and its arguments.",
        json!({
            "type": "object",
            "properties": {
                "name": {"type": "string", "description": "The exact name of the tool to run."},
                "arguments": {"type": "object", "additionalProperties": true, "description": "The arguments for that tool, exactly as its schema describes them."}
            },
            "required": ["name", "arguments"],
            "additionalProperties": false
        }),
        json!({"name": "calendar_list_events", "arguments": {"day": "2026-03-02"}}),
    )
}

pub fn meta_definitions() -> Vec<ToolDefinition> {
    vec![
        tool_search_definition(),
        tool_describe_definition(),
        tool_call_definition(),
    ]
}

#[derive(Clone, Copy)]
enum Kind {
    Search,
    Describe,
    Call,
}

struct MetaTool {
    kind: Kind,
    definition: ToolDefinition,
    catalog: CatalogCell,
}

/// The meta tools bound to `catalog`, which the registry fills at freeze.
pub fn meta_tools(catalog: &CatalogCell) -> Vec<Arc<dyn Tool>> {
    [
        (Kind::Search, tool_search_definition()),
        (Kind::Describe, tool_describe_definition()),
        (Kind::Call, tool_call_definition()),
    ]
    .into_iter()
    .map(|(kind, definition)| {
        Arc::new(MetaTool {
            kind,
            definition,
            catalog: catalog.clone(),
        }) as Arc<dyn Tool>
    })
    .collect()
}

#[async_trait]
impl Tool for MetaTool {
    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }

    async fn execute(&self, context: ToolContext, input: Value) -> Result<ToolResult, CoreError> {
        let limits = context.limits;
        let Some(catalog) = self.catalog.get() else {
            return Ok(refusal(
                "tool_unavailable",
                "The session's tool catalog is not ready yet.",
                &limits,
            ));
        };
        Ok(match self.kind {
            Kind::Search => search(catalog, &input, &limits),
            Kind::Describe => describe(catalog, &input, &limits),
            Kind::Call => refusal(
                "tool_unavailable",
                "tool_call is run by the session loop and cannot run on its own here. Call the tool directly if it is in your tool list.",
                &limits,
            ),
        })
    }
}

fn refusal(code: &str, message: &str, limits: &ToolLimits) -> ToolResult {
    failure_result(code, &failure(code, message, None), "execution", None, limits)
}

fn ok(data: Value, summary: String, limits: &ToolLimits) -> ToolResult {
    ToolResult {
        ok: true,
        data: Some(data),
        sources: vec![],
        warnings: vec![],
        truncated: false,
        limits: limits.clone(),
        model_data: None,
        summary: Some(summary),
        status: None,
        model_content: None,
    }
}

fn compact(entry: &CatalogEntry) -> Value {
    let definition = &entry.definition;
    let required = required_names(&definition.input_schema);
    let params: Vec<String> = parameter_names(&definition.input_schema)
        .into_iter()
        .map(|name| {
            if required.contains(&name) {
                format!("{name}*")
            } else {
                name
            }
        })
        .collect();
    let mut value = json!({
        "name": definition.name,
        "namespace": entry.namespace,
        "description": first_sentence(&definition.description, SUMMARY_CHARS),
        "effect": definition.effect,
        "params": params,
    });
    if !entry.available {
        value["available"] = false.into();
    }
    value
}

fn search(catalog: &Catalog, input: &Value, limits: &ToolLimits) -> ToolResult {
    let query = input["query"].as_str().unwrap_or_default();
    let limit = input["limit"]
        .as_u64()
        .map_or(DEFAULT_LIMIT, |value| value as usize)
        .clamp(1, MAX_LIMIT);
    let namespace = input["namespace"].as_str().filter(|name| !name.is_empty());
    let (matches, total) = catalog.search(query, namespace, limit);
    let shown: Vec<Value> = matches.into_iter().map(compact).collect();
    let summary = if shown.is_empty() {
        format!("No deferred tool matches \"{query}\". Try other words.")
    } else {
        format!(
            "{} of {total} matching tools. Call tool_describe for a schema, tool_call to run one.",
            shown.len()
        )
    };
    ok(json!({"matches": shown, "total_matched": total}), summary, limits)
}

fn describe(catalog: &Catalog, input: &Value, limits: &ToolLimits) -> ToolResult {
    let name = input["name"].as_str().unwrap_or_default();
    let Some(entry) = catalog.get(name) else {
        let closest = closest_names(name, &catalog.names(), 3);
        let message = format!(
            "No tool named {name}. The closest names are: {}. Call tool_search to find the right one.",
            closest.join(", ")
        );
        return failure_result(
            "tool_not_found",
            &failure("tool_not_found", &message, None),
            "execution",
            Some(json!({"closest": closest})),
            limits,
        );
    };
    let definition = &entry.definition;
    let mut data = json!({
        "name": definition.name,
        "namespace": entry.namespace,
        "description": definition.description,
        "effect": definition.effect,
        "input_schema": definition.input_schema,
        "loading": if entry.deferred { "deferred" } else { "direct" },
    });
    if !definition.examples.is_empty() {
        data["examples"] = definition.examples.clone().into();
    }
    if !entry.available {
        data["available"] = false.into();
    }
    let summary = if entry.deferred {
        format!(
            "{} is deferred: run it with tool_call, name {} and the arguments above.",
            definition.name, definition.name
        )
    } else {
        format!("{} is in your tool list: call it directly.", definition.name)
    };
    ok(data, summary, limits)
}
