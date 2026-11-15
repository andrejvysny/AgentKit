//! The three meta tools (roadmap 2.3, ADR 0031): `tool_search`,
//! `tool_describe`, `tool_call`. They have no authority of their own.
//! `tool_search` and `tool_describe` read the session catalog. `tool_call` is
//! unwrapped by the call pipeline before guards run (see `call_pipeline`), so
//! the inner tool is resolved, normalised, validated, guarded, approved and
//! recorded exactly like a direct call; its own `execute` is only reached by
//! a host that does not run the pipeline, and refuses.
use crate::deferred::{
    Catalog, CatalogCell, CatalogEntry, LoadMode, TOOL_CALL, TOOL_DESCRIBE, TOOL_SEARCH,
    TOOLSET_LOAD, first_sentence, parameter_names, required_names,
};
use crate::toolset::{META_TOOLSET, PromotionCell, render_guide};
use crate::failure_catalog::{failure, failure_result};
use crate::{CoreError, Tool, ToolContext, ToolDefinition, ToolLimits, ToolResult, closest_names};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
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

pub fn toolset_load_definition() -> ToolDefinition {
    definition(
        TOOLSET_LOAD,
        "Load a toolset: its usage guide (which tool for which situation, the usual order, worked examples, common mistakes) and the compact definitions of its tools, returned as this call's result. The system prompt lists the toolsets, one line each. Load a toolset before you use its tools for the first time. Some toolsets have topic guides; pass topic to read one. Loaded tools are run with tool_call, or called by name from your next turn when your tool list grows.",
        json!({
            "type": "object",
            "properties": {
                "toolset": {"type": "string", "description": "The toolset id, as listed in the system prompt, such as files or git."},
                "topic": {"type": "string", "description": "A topic guide within that set, when it has any."}
            },
            "required": ["toolset"],
            "additionalProperties": false
        }),
        json!({"toolset": "git"}),
    )
}

pub fn meta_definitions() -> Vec<ToolDefinition> {
    vec![
        tool_search_definition(),
        tool_describe_definition(),
        tool_call_definition(),
        toolset_load_definition(),
    ]
}

#[derive(Clone, Copy)]
enum Kind {
    Search,
    Describe,
    Call,
    Load,
}

struct MetaTool {
    kind: Kind,
    definition: ToolDefinition,
    catalog: CatalogCell,
    promotions: PromotionCell,
    mode: LoadMode,
}

/// The meta tools bound to `catalog`, which the registry fills at freeze.
/// `toolset_load` follows `mode` and asks `promotions` for promote mode.
pub fn meta_tools(
    catalog: &CatalogCell,
    promotions: &PromotionCell,
    mode: LoadMode,
) -> Vec<Arc<dyn Tool>> {
    [
        (Kind::Search, tool_search_definition()),
        (Kind::Describe, tool_describe_definition()),
        (Kind::Call, tool_call_definition()),
        (Kind::Load, toolset_load_definition()),
    ]
    .into_iter()
    .map(|(kind, definition)| {
        Arc::new(MetaTool {
            kind,
            definition,
            catalog: catalog.clone(),
            promotions: promotions.clone(),
            mode,
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
        let seen = with_promoted(catalog, &self.promotions);
        let catalog: &Catalog = &seen;
        Ok(match self.kind {
            Kind::Search => search(catalog, &self.promotions, &input, &limits),
            Kind::Describe => describe(catalog, &self.promotions, &input, &limits),
            Kind::Load => load(catalog, &self.promotions, self.mode, &input, &limits),
            Kind::Call => refusal(
                "tool_unavailable",
                "tool_call is run by the session loop and cannot run on its own here. Call the tool directly if it is in your tool list.",
                &limits,
            ),
        })
    }
}

/// The frozen catalog plus the tools promoted since: a promoted tool the
/// catalog does not hold (it joined after the freeze) is still found by
/// search and describe. Promoted tools stay listed so the model learns they
/// are now in its tool list.
fn with_promoted<'a>(catalog: &'a Catalog, promotions: &PromotionCell) -> Cow<'a, Catalog> {
    let extra: Vec<CatalogEntry> = promotions
        .promoted_definitions()
        .into_iter()
        .filter(|definition| catalog.get(&definition.name).is_none())
        .map(|definition| CatalogEntry {
            namespace: catalog.toolsets().id_of(&definition),
            canonical: definition.name.clone(),
            definition,
            deferred: true,
            available: true,
        })
        .collect();
    if extra.is_empty() {
        Cow::Borrowed(catalog)
    } else {
        Cow::Owned(catalog.with_extra(extra))
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

fn search(
    catalog: &Catalog,
    promotions: &PromotionCell,
    input: &Value,
    limits: &ToolLimits,
) -> ToolResult {
    let query = input["query"].as_str().unwrap_or_default();
    let limit = input["limit"]
        .as_u64()
        .map_or(DEFAULT_LIMIT, |value| value as usize)
        .clamp(1, MAX_LIMIT);
    let namespace = input["namespace"].as_str().filter(|name| !name.is_empty());
    let (matches, total) = catalog.search(query, namespace, limit);
    let shown: Vec<Value> = matches
        .into_iter()
        .map(|entry| {
            let mut value = compact(entry);
            if promotions.is_promoted_name(&entry.definition.name) {
                value["in_tool_list"] = true.into();
            }
            value
        })
        .collect();
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

fn describe(
    catalog: &Catalog,
    promotions: &PromotionCell,
    input: &Value,
    limits: &ToolLimits,
) -> ToolResult {
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
    // A promoted tool is in the tool list now, whatever the frozen catalog says.
    let deferred = entry.deferred && !promotions.is_promoted_name(&definition.name);
    let mut data = json!({
        "name": definition.name,
        "namespace": entry.namespace,
        "description": definition.description,
        "effect": definition.effect,
        "input_schema": definition.input_schema,
        "loading": if deferred { "deferred" } else { "direct" },
    });
    if !definition.examples.is_empty() {
        data["examples"] = definition.examples.clone().into();
    }
    if !entry.available {
        data["available"] = false.into();
    }
    let summary = if deferred {
        format!(
            "{} is deferred: run it with tool_call, name {} and the arguments above.",
            definition.name, definition.name
        )
    } else {
        format!("{} is in your tool list: call it directly.", definition.name)
    };
    ok(data, summary, limits)
}

/// Names a canonical tool is offered as, for guide placeholders: one name,
/// or "a, b or c" for a tool a dialect splits.
fn shown_names(catalog: &Catalog) -> BTreeMap<String, String> {
    let mut wires: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for entry in catalog.entries() {
        wires
            .entry(entry.canonical.clone())
            .or_default()
            .push(entry.definition.name.clone());
    }
    wires
        .into_iter()
        .map(|(canonical, names)| {
            let shown = match names.as_slice() {
                [only] => only.clone(),
                [rest @ .., last] => format!("{} or {last}", rest.join(", ")),
                [] => canonical.clone(),
            };
            (canonical, shown)
        })
        .collect()
}

fn load(
    catalog: &Catalog,
    promotions: &PromotionCell,
    mode: LoadMode,
    input: &Value,
    limits: &ToolLimits,
) -> ToolResult {
    let id = input["toolset"].as_str().unwrap_or_default().trim();
    let topic = input["topic"].as_str().map(str::trim).filter(|t| !t.is_empty());
    let view = catalog.toolsets();
    let mut members: Vec<CatalogEntry> = catalog
        .entries()
        .iter()
        .filter(|entry| entry.namespace == id)
        .cloned()
        .collect();
    if id == META_TOOLSET {
        members = meta_definitions()
            .into_iter()
            .map(|definition| CatalogEntry {
                canonical: definition.name.clone(),
                definition,
                deferred: false,
                available: true,
                namespace: META_TOOLSET.to_owned(),
            })
            .collect();
    }
    let set = view.toolset(id);
    if members.is_empty() {
        // A toolset none of whose tools this session has is not loadable.
        let known: BTreeSet<String> = catalog
            .entries()
            .iter()
            .map(|entry| entry.namespace.clone())
            .chain([META_TOOLSET.to_owned()])
            .collect();
        let known: Vec<String> = known.into_iter().collect();
        let closest = closest_names(id, &known, 3);
        let message = format!(
            "No toolset named {id}. The closest ids are: {}. Use an id from the toolset list in the system prompt.",
            closest.join(", ")
        );
        return failure_result(
            "tool_not_found",
            &failure("tool_not_found", &message, None),
            "execution",
            Some(json!({"closest": closest})),
            limits,
        );
    }
    let guide = match (topic, set) {
        (Some(topic), Some(set)) => match set.topic(topic) {
            Some(guide) => Some(guide),
            None => {
                let topics: Vec<&str> = set.topics.iter().map(|(name, _)| name.as_str()).collect();
                let message = format!(
                    "No topic {topic} in toolset {id}. Topics: {}. Call toolset_load again with one of them, or without topic.",
                    if topics.is_empty() { "none".to_owned() } else { topics.join(", ") }
                );
                return failure_result(
                    "invalid_arguments",
                    &failure("invalid_arguments", &message, None),
                    "execution",
                    Some(json!({"topics": topics})),
                    limits,
                );
            }
        },
        (_, set) => set.and_then(|set| set.guide.as_ref()),
    };
    let text = guide.map(|guide| render_guide(&guide.text, &shown_names(catalog)));
    let promotable = members
        .iter()
        .any(|entry| entry.deferred && entry.available && !promotions.is_promoted_name(&entry.definition.name));
    let promoting = mode == LoadMode::Promote && promotable && promotions.request(id);
    let deferred_left = members.iter().any(|entry| entry.deferred);
    let summary = match (deferred_left, promoting) {
        (false, _) => format!("The tools of {id} are in your tool list: call them directly. Guide below."),
        (true, true) => format!(
            "Loaded {id}: {} tools. They join your tool list on your next turn and are then called by name; until then run them with tool_call.",
            members.len()
        ),
        (true, false) => format!(
            "Loaded {id}: {} tools. Run them with tool_call (name and arguments); your tool list does not change.",
            members.len()
        ),
    };
    let topics: Vec<&str> = set
        .map(|set| set.topics.iter().map(|(name, _)| name.as_str()).collect())
        .unwrap_or_default();
    let tools: Vec<Value> = members.iter().map(compact).collect();
    let mut data = json!({
        "toolset": id,
        "title": set.map_or(id, |set| set.title.as_str()),
        "mode": if mode == LoadMode::Promote { "promote" } else { "indirect" },
        "guide": text,
        "tools": tools,
    });
    if let Some(guide) = guide {
        data["guide_version"] = guide.version.clone().into();
    }
    if !topics.is_empty() {
        data["topics"] = json!(topics);
    }
    if promoting {
        data["joins_tool_list"] = "next turn".into();
    }
    ok(data, summary, limits)
}
