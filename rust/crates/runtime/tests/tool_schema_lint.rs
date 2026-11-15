//! Schema lint over every static AgentKit tool definition: the coding tool
//! pack, the subagent tools, the advisor tool and the language-server tools
//! (native tool use spec, "Schema rules" L1 to L12; requirements TU-04 and
//! TU-05).
//!
//! This crate is where the layering lets one test reach all of them. The
//! exception list is empty by construction: a rule a tool breaks is fixed in
//! the tool, never listed here.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use agentkit_agents::{
    BUILTIN_ROLES, REPORT_FINDINGS_TOOL, SUBAGENT_ARG_ALIASES, SUBAGENT_TOOL_NAMES,
    advisor_tool_definition, report_findings_definition, subagent_tool_definitions,
};
use agentkit_core::{
    AuthKind, CancellationToken, ChatProvider, ChatRequest, ConnectionIdentity, CoreError,
    ExampleLane, Tool, ToolConstraint, ToolContext, ToolDefinition, ToolLimits, ToolRegistry,
    ToolResult, meta_definitions, presented_description,
};
use agentkit_governance::{ContributedTool, ToolContributionContext, ToolContributor};
use agentkit_lsp::{LSP_ARG_ALIASES, lsp_tool_definitions};
use agentkit_providers::{
    ChatCompletionsProvider, SchemaLane, TrustedTransport, build_responses_request_with_warnings,
    schema_lane_violations,
};
use agentkit_toolpacks_coding::{
    CODING_TOOL_NAMES, CodingLimitsOverride, CodingToolPackOptions, CodingWorkspace, SandboxMode,
    create_coding_tool_pack,
};
use async_trait::async_trait;
use futures::StreamExt;
use serde_json::{Value, json};

/// `(tool, rule)` pairs allowed to break a rule. Empty, and it stays empty.
const EXCEPTIONS: [(&str, &str); 0] = [];

const ADVISOR_TOOL: &str = "advisor_review";
const LSP_TOOL_NAMES: [&str; 6] = [
    "lsp_definition",
    "lsp_references",
    "lsp_hover",
    "lsp_symbols",
    "lsp_implementation",
    "lsp_diagnostics",
];

/// A tool known only by its definition (and parameter aliases): enough for
/// schema validation, aliases and lint; it never runs.
struct DefinitionOnly {
    definition: ToolDefinition,
    aliases: &'static [(&'static str, &'static str)],
}

#[async_trait]
impl Tool for DefinitionOnly {
    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }

    fn arg_aliases(&self) -> Vec<(String, String)> {
        self.aliases
            .iter()
            .map(|(alias, canonical)| ((*alias).to_owned(), (*canonical).to_owned()))
            .collect()
    }

    async fn execute(&self, _: ToolContext, _: Value) -> Result<ToolResult, CoreError> {
        Err(CoreError::new("lint_stub", "definition only"))
    }
}

fn definition_only(
    definition: ToolDefinition,
    aliases: &'static [(&'static str, &'static str)],
) -> Arc<dyn Tool> {
    Arc::new(DefinitionOnly {
        definition,
        aliases,
    })
}

fn contribution() -> ToolContributionContext {
    ToolContributionContext {
        durable: None,
        chat_id: Some("chat-1".into()),
        run_id: "run-1".into(),
        scope_id: None,
        actor: Value::Null,
        actor_id: None,
        bindings: vec![],
        limits: ToolLimits {
            profile: "small".into(),
            max_bytes: 16384,
            max_items: None,
        },
        cancellation: CancellationToken::new(),
        has_primary_binding: true,
        timeout_ms: 30_000,
    }
}

/// Every static tool: the real coding tools (configured with one test
/// command, as a project would), and the definitions of the rest.
async fn all_tools() -> Vec<Arc<dyn Tool>> {
    let pack = create_coding_tool_pack(CodingToolPackOptions {
        workspace: CodingWorkspace {
            root: "/tmp".into(),
        },
        limits: CodingLimitsOverride::default(),
        allow_sensitive: false,
        clock: None,
        sandbox: SandboxMode::Off,
        network: false,
        allow_git_writes: false,
        test_commands: vec!["cargo test".into()],
        platform: Some("linux".into()),
        sandbox_exec_path: None,
        coordinator: None,
        coordination_wait_ms: None,
        environment: None,
        permission: None,
    })
    .await
    .expect("coding pack");
    let mut tools: Vec<Arc<dyn Tool>> = pack
        .contribute(&contribution())
        .await
        .expect("contribute")
        .into_iter()
        .map(|tool| match tool {
            ContributedTool::Read(tool) | ContributedTool::DirectWrite(tool) => tool,
            ContributedTool::Write(_) => panic!("coding tools are not proposal-governed"),
        })
        .collect();
    for definition in subagent_tool_definitions() {
        tools.push(definition_only(definition, SUBAGENT_ARG_ALIASES));
    }
    tools.push(definition_only(report_findings_definition(), &[]));
    tools.push(definition_only(advisor_tool_definition(), &[]));
    for definition in lsp_tool_definitions() {
        tools.push(definition_only(definition, LSP_ARG_ALIASES));
    }
    tools
}

fn registry_of(tools: &[Arc<dyn Tool>]) -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    for tool in tools {
        registry.register(tool.clone()).expect("register");
    }
    registry
}

// ---- schema walking -------------------------------------------------------

fn type_names(schema: &Value) -> Vec<&str> {
    match &schema["type"] {
        Value::String(kind) => vec![kind.as_str()],
        Value::Array(kinds) => kinds.iter().filter_map(Value::as_str).collect(),
        _ => vec![],
    }
}

fn is_type(schema: &Value, kind: &str) -> bool {
    type_names(schema).contains(&kind)
}

fn properties_of(schema: &Value) -> Vec<(&String, &Value)> {
    schema["properties"]
        .as_object()
        .map(|properties| properties.iter().collect())
        .unwrap_or_default()
}

/// Every schema node that describes a value: the root, each property and
/// each array item, with a readable path.
fn visit(schema: &Value, path: &str, visitor: &mut dyn FnMut(&Value, &str)) {
    visitor(schema, path);
    for (name, child) in properties_of(schema) {
        visit(child, &format!("{path}.{name}"), visitor);
    }
    if schema.get("items").is_some_and(Value::is_object) {
        visit(&schema["items"], &format!("{path}[]"), visitor);
    }
}

/// `(property name, schema, path)` for every property at any depth.
fn all_properties(schema: &Value) -> Vec<(String, Value, String)> {
    let mut found = vec![];
    visit(schema, "$", &mut |node, path| {
        for (name, child) in properties_of(node) {
            found.push((name.clone(), child.clone(), format!("{path}.{name}")));
        }
    });
    found
}

fn is_scalar(schema: &Value) -> bool {
    let kinds = type_names(schema);
    !kinds.is_empty()
        && kinds
            .iter()
            .all(|kind| matches!(*kind, "string" | "integer" | "number" | "boolean" | "null"))
        && schema.get("properties").is_none()
        && schema.get("items").is_none()
}

fn is_scalar_array(schema: &Value) -> bool {
    is_type(schema, "array") && schema.get("items").is_some_and(is_scalar)
}

fn is_flat_object(schema: &Value) -> bool {
    is_type(schema, "object")
        && properties_of(schema)
            .iter()
            .all(|(_, child)| is_scalar(child) || is_scalar_array(child))
}

// ---- the rules ------------------------------------------------------------

type Problems = Vec<String>;

fn l1_root_shape(def: &ToolDefinition, out: &mut Problems) {
    let schema = &def.input_schema;
    if !is_type(schema, "object") {
        out.push("L1: root type is not object".into());
    }
    for keyword in [
        "anyOf", "oneOf", "allOf", "not", "enum", "if", "then", "else",
    ] {
        if schema.get(keyword).is_some() {
            out.push(format!("L1: root has {keyword}"));
        }
    }
}

fn is_value_or_null(alternatives: &Value) -> bool {
    alternatives.as_array().is_some_and(|items| {
        items.len() == 2 && items.iter().any(|item| item == &json!({"type": "null"}))
    })
}

fn l2_property_alternatives(def: &ToolDefinition, out: &mut Problems) {
    for (_, schema, path) in all_properties(&def.input_schema) {
        for keyword in ["anyOf", "oneOf"] {
            if let Some(alternatives) = schema.get(keyword)
                && !is_value_or_null(alternatives)
            {
                out.push(format!("L2: {path} has {keyword} other than value-or-null"));
            }
        }
        for keyword in ["allOf", "not"] {
            if schema.get(keyword).is_some() {
                out.push(format!("L2: {path} has {keyword}"));
            }
        }
        let kinds = type_names(&schema);
        if kinds.len() > 1 && !(kinds.len() == 2 && kinds.contains(&"null")) {
            out.push(format!(
                "L2: {path} has a type list other than value-or-null"
            ));
        }
    }
}

fn l3_depth(def: &ToolDefinition, out: &mut Problems) {
    for (name, schema) in properties_of(&def.input_schema) {
        let list_of_flat_objects = is_type(schema, "array")
            && schema
                .get("items")
                .is_some_and(|items| is_scalar(items) || is_flat_object(items));
        if !(is_scalar(schema) || is_flat_object(schema) || list_of_flat_objects) {
            out.push(format!(
                "L3: {name} is deeper than an object or a list of flat objects"
            ));
        }
    }
}

fn l4_required_and_closed(def: &ToolDefinition, out: &mut Problems) {
    visit(&def.input_schema, "$", &mut |node, path| {
        if !is_type(node, "object") {
            return;
        }
        // An object with no `properties` is opaque: it carries another
        // tool's own arguments (`tool_call.arguments`), whose schema is
        // checked where that tool is called. Same rule as OneMind T1.4; no
        // exception list.
        if node.get("properties").is_none() {
            return;
        }
        match node.get("required").and_then(Value::as_array) {
            None => out.push(format!("L4: {path} has no required list")),
            Some(required) => {
                for name in required.iter().filter_map(Value::as_str) {
                    if node["properties"].get(name).is_none() {
                        out.push(format!("L4: {path} requires unknown property {name}"));
                    }
                }
            }
        }
        if node.get("additionalProperties") != Some(&json!(false)) {
            out.push(format!(
                "L4: {path} does not set additionalProperties false"
            ));
        }
    });
}

/// Names the vocabulary replaced; none may appear.
const OLD_NAMES: [&str; 22] = [
    "file",
    "filePath",
    "filepath",
    "relativePath",
    "relativeDir",
    "cwd",
    "workdir",
    "timeoutSec",
    "timeoutMs",
    "timeout_sec",
    "timeout_ms",
    "overallTimeoutMs",
    "maxCount",
    "max_count",
    "maxChars",
    "max_chars",
    "maxChunks",
    "sourceOffset",
    "maxResults",
    "paths",
    "args",
    "edits",
];

fn is_snake_case(name: &str) -> bool {
    !name.is_empty()
        && name.chars().next().is_some_and(|c| c.is_ascii_lowercase())
        && !name.ends_with('_')
        && !name.contains("__")
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

fn l5_vocabulary(def: &ToolDefinition, out: &mut Problems) {
    for (name, schema, path) in all_properties(&def.input_schema) {
        if !is_snake_case(&name) {
            out.push(format!("L5: {path} is not snake_case"));
        }
        if OLD_NAMES.contains(&name.as_str()) {
            out.push(format!("L5: {path} uses a name the vocabulary replaced"));
        }
        for suffix in ["_ms", "_sec", "_secs", "_seconds"] {
            if name.ends_with(suffix) {
                out.push(format!(
                    "L5: {path} puts a unit in the name; use timeout in ms"
                ));
            }
        }
        let kind_is = |expected: &str| is_type(&schema, expected);
        let description = schema["description"].as_str().unwrap_or_default();
        match name.as_str() {
            "file_path" | "path" | "directory" | "query" | "pattern" | "cursor"
                if !kind_is("string") =>
            {
                out.push(format!("L5: {path} must be a string"))
            }
            "limit" | "offset" | "timeout" if !kind_is("integer") => {
                out.push(format!("L5: {path} must be an integer"))
            }
            _ => {}
        }
        if name == "timeout" && !description.contains("millisecond") {
            out.push(format!("L5: {path} must say it is in milliseconds"));
        }
    }
}

/// Parameter names that choose from a fixed set and so must be an enum.
const CHOICE_NAMES: [&str; 10] = [
    "mode",
    "kind",
    "type",
    "status",
    "role",
    "format",
    "order",
    "direction",
    "scope",
    "language",
];

fn l6_enums_and_bounds(def: &ToolDefinition, out: &mut Problems) {
    for (name, schema, path) in all_properties(&def.input_schema) {
        if is_type(&schema, "number") {
            out.push(format!("L6: {path} is a number; counts are integers"));
        }
        if is_type(&schema, "integer")
            && (schema.get("minimum").is_none() || schema.get("maximum").is_none())
        {
            out.push(format!("L6: {path} is an integer without both bounds"));
        }
        if is_type(&schema, "array")
            && (schema.get("maxItems").is_none() && !is_type(&schema["items"], "object"))
        {
            out.push(format!("L6: {path} is a list without maxItems"));
        }
        let description = schema["description"]
            .as_str()
            .unwrap_or_default()
            .to_lowercase();
        if is_type(&schema, "string") && schema.get("enum").is_none() {
            if CHOICE_NAMES.contains(&name.as_str()) {
                out.push(format!(
                    "L6: {path} chooses from a fixed set but is a free string"
                ));
            }
            if description.contains("one of ") || description.contains("either ") {
                out.push(format!(
                    "L6: {path} lists choices in prose; make it an enum"
                ));
            }
        }
    }
}

fn l7_runtime_owned(def: &ToolDefinition, out: &mut Problems) {
    for (name, _, path) in all_properties(&def.input_schema) {
        let lower = name.to_lowercase();
        let banned = [
            "sha256",
            "revision",
            "hash",
            "idempotency",
            "mutation_id",
            "mutationid",
            "nonce",
            "etag",
        ]
        .iter()
        .any(|part| lower.contains(part))
            || (lower.ends_with("key") && lower != "key");
        if banned {
            out.push(format!(
                "L7: {path} is an idempotency key, revision or hash"
            ));
        }
    }
}

fn contains_word(text: &str, word: &str) -> bool {
    text.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .any(|token| token == word)
}

fn l8_constraints(
    def: &ToolDefinition,
    tool: &dyn Tool,
    registry: &ToolRegistry,
    out: &mut Problems,
) {
    let constraints = tool.constraints();
    let names: BTreeSet<String> = all_properties(&def.input_schema)
        .into_iter()
        .map(|(name, _, _)| name)
        .collect();
    for constraint in &constraints {
        let id = &constraint.code;
        if constraint.code.is_empty() || constraint.rule.is_empty() {
            out.push("L8: a constraint has no code or rule".into());
        }
        if !constraint.teaching_error.contains(id.as_str())
            || !constraint.teaching_error.contains("Next:")
        {
            out.push(format!(
                "L8: {id} teaching error must carry its code and a Next: step"
            ));
        }
        if constraint.params.len() < 2 || constraint.params.iter().any(|p| !names.contains(p)) {
            out.push(format!(
                "L8: {id} must relate two or more existing parameters"
            ));
        }
        if tool
            .check_constraints(&constraint.violating_example)
            .as_deref()
            != Some(id.as_str())
        {
            out.push(format!(
                "L8: {id} checker does not catch its violating example"
            ));
        }
        if !registry
            .validate_input(&def.name, &constraint.violating_example)
            .is_empty()
        {
            out.push(format!(
                "L8: {id} violating example must be schema-valid (else the schema already says it)"
            ));
        }
    }
    for example in &def.examples {
        if let Some(code) = tool.check_constraints(example) {
            out.push(format!("L8: an example breaks constraint {code}"));
        }
    }
    // A property description that names another parameter states a rule
    // between them; the rule must be listed, with its checker.
    let properties = all_properties(&def.input_schema);
    for (name, schema, path) in &properties {
        let description = schema["description"].as_str().unwrap_or_default();
        for (other, _, _) in &properties {
            if other != name
                && contains_word(description, other)
                && !constraints_relate(&constraints, name, other)
            {
                out.push(format!(
                    "L8: {path} mentions parameter {other} in prose but no constraint lists the rule"
                ));
            }
        }
    }
}

fn constraints_relate(constraints: &[ToolConstraint], a: &str, b: &str) -> bool {
    constraints
        .iter()
        .any(|c| c.params.iter().any(|p| p == a) && c.params.iter().any(|p| p == b))
}

const BANNED_KEYS: [&str; 6] = [
    "minProperties",
    "maxProperties",
    "patternProperties",
    "dependencies",
    "dependentRequired",
    "$ref",
];

fn l9_portable_keys(def: &ToolDefinition, out: &mut Problems) {
    for violation in schema_lane_violations(SchemaLane::Responses, &def.input_schema) {
        out.push(format!("L9: {violation}"));
    }
    fn walk(value: &Value, path: &str, out: &mut Problems) {
        if let Some(map) = value.as_object() {
            for (key, child) in map {
                if BANNED_KEYS.contains(&key.as_str()) {
                    out.push(format!("L9: {path} uses {key}"));
                }
                if key != "properties" {
                    walk(child, &format!("{path}.{key}"), out);
                } else if let Some(properties) = child.as_object() {
                    for (name, property) in properties {
                        walk(property, &format!("{path}.{name}"), out);
                    }
                }
            }
        }
    }
    walk(&def.input_schema, "$", out);
}

fn l11_examples(def: &ToolDefinition, registry: &ToolRegistry, out: &mut Problems) {
    if def.examples.is_empty() {
        out.push("L11: no example call".into());
    }
    for (index, example) in def.examples.iter().enumerate() {
        let errors = registry.validate_input(&def.name, example);
        if !errors.is_empty() {
            out.push(format!(
                "L11: example {index} does not validate: {errors:?}"
            ));
        }
    }
}

/// First segments of tool names; a token that starts with one and is not a
/// known tool, parameter or enum value is a reference to a tool that does
/// not exist.
const TOOL_PREFIXES: [&str; 14] = [
    "read", "write", "edit", "list", "find", "code", "repo", "git", "run", "submit", "subagent",
    "lsp", "advisor", "tool",
];

fn l12_description(def: &ToolDefinition, tool_names: &BTreeSet<String>, out: &mut Problems) {
    let length = def.description.chars().count();
    if !(150..=900).contains(&length) {
        out.push(format!(
            "L12: description is {length} characters; allowed 150 to 900"
        ));
    }
    let mut known: BTreeSet<String> = tool_names.clone();
    for (name, schema, _) in all_properties(&def.input_schema) {
        known.insert(name);
        for value in schema["enum"].as_array().into_iter().flatten() {
            if let Some(text) = value.as_str() {
                known.insert(text.to_owned());
            }
        }
    }
    for token in def
        .description
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .filter(|token| token.contains('_'))
    {
        let prefix = token.split('_').next().unwrap_or_default();
        if TOOL_PREFIXES.contains(&prefix) && !known.contains(token) {
            out.push(format!(
                "L12: description names {token}, which is not a tool"
            ));
        }
    }
}

/// Every rule that looks at one tool.
fn check_tool(tool: &dyn Tool, registry: &ToolRegistry, tool_names: &BTreeSet<String>) -> Problems {
    let def = tool.definition();
    let mut found = Problems::new();
    l1_root_shape(def, &mut found);
    l2_property_alternatives(def, &mut found);
    l3_depth(def, &mut found);
    l4_required_and_closed(def, &mut found);
    l5_vocabulary(def, &mut found);
    l6_enums_and_bounds(def, &mut found);
    l7_runtime_owned(def, &mut found);
    l8_constraints(def, tool, registry, &mut found);
    l9_portable_keys(def, &mut found);
    l11_examples(def, registry, &mut found);
    l12_description(def, tool_names, &mut found);
    found
        .into_iter()
        .filter(|problem| {
            let rule = problem.split(':').next().unwrap_or_default();
            !EXCEPTIONS
                .iter()
                .any(|(name, excepted)| *name == def.name && *excepted == rule)
        })
        .map(|problem| format!("{}: {problem}", def.name))
        .collect()
}

// ---- the tests ------------------------------------------------------------

#[test]
fn the_exception_list_is_empty() {
    assert!(EXCEPTIONS.is_empty());
}

#[tokio::test]
async fn every_static_tool_is_linted() {
    let tools = all_tools().await;
    let names: BTreeSet<String> = tools
        .iter()
        .map(|tool| tool.definition().name.clone())
        .collect();
    let expected: BTreeSet<String> = CODING_TOOL_NAMES
        .iter()
        .chain(SUBAGENT_TOOL_NAMES.iter())
        .chain([REPORT_FINDINGS_TOOL].iter())
        .chain(LSP_TOOL_NAMES.iter())
        .chain([ADVISOR_TOOL].iter())
        .map(|name| (*name).to_owned())
        .collect();
    assert_eq!(
        names, expected,
        "a tool was added or removed without the lint"
    );
    assert_eq!(tools.len(), 26);
}

#[tokio::test]
async fn schema_rules_l1_to_l12_hold_for_every_static_tool() {
    let tools = all_tools().await;
    let registry = registry_of(&tools);
    let names: BTreeSet<String> = tools
        .iter()
        .map(|tool| tool.definition().name.clone())
        .collect();
    let mut problems = Problems::new();
    for tool in &tools {
        problems.extend(check_tool(tool.as_ref(), &registry, &names));
    }
    assert!(problems.is_empty(), "{problems:#?}");
}

/// The meta tools (T4.1) follow the same rules; `tool_call.arguments` is the
/// one opaque object.
#[tokio::test]
async fn schema_rules_l1_to_l12_hold_for_the_meta_tools() {
    let mut tools = all_tools().await;
    tools.extend(
        meta_definitions()
            .into_iter()
            .map(|definition| definition_only(definition, &[])),
    );
    let registry = registry_of(&tools);
    let names: BTreeSet<String> = tools
        .iter()
        .map(|tool| tool.definition().name.clone())
        .collect();
    let mut problems = Problems::new();
    for tool in tools.iter().filter(|tool| {
        meta_definitions()
            .iter()
            .any(|meta| meta.name == tool.definition().name)
    }) {
        problems.extend(check_tool(tool.as_ref(), &registry, &names));
    }
    assert!(problems.is_empty(), "{problems:#?}");
}

#[test]
fn an_object_with_properties_is_still_closed_and_required() {
    let mut problems = Problems::new();
    let open = ToolDefinition {
        name: "open_tool".into(),
        version: "1".into(),
        effect: "read".into(),
        capability: "test".into(),
        description: String::new(),
        input_schema: json!({"type": "object", "properties": {"a": {"type": "string"}}}),
        output_schema: None,
        timeout_ms: None,
        examples: vec![],
    };
    l4_required_and_closed(&open, &mut problems);
    assert_eq!(problems.len(), 2, "{problems:?}");
}

/// The lint must not pass vacuously: a definition that breaks every rule is
/// flagged by every rule.
#[tokio::test]
async fn the_lint_catches_each_rule() {
    let bad = ToolDefinition {
        name: "bad_tool".into(),
        version: "1".into(),
        effect: "read".into(),
        capability: "test".into(),
        description: "Short, and it names read_everything.".into(),
        input_schema: json!({
            "type": "object",
            "anyOf": [{"required": ["a"]}, {"required": ["b"]}],
            "properties": {
                "relativePath": {"type": "string"},
                "mode": {"type": "string", "description": "one of fast or slow"},
                "count": {"type": "number"},
                "expectedSha256": {"type": "string"},
                "pick": {"anyOf": [{"type": "string"}, {"type": "integer"}]},
                "bag": {"type": "object", "properties": {
                    "inner": {"type": "object", "properties": {"deep": {"type": "string"}}}
                }},
                "limit": {"type": "integer"},
                "other": {"type": "string", "description": "only valid with relativePath"},
                "wide": {"type": "string", "minProperties": 1},
            },
            "minProperties": 1,
        }),
        output_schema: None,
        timeout_ms: None,
        examples: vec![],
    };
    let tool = definition_only(bad.clone(), &[]);
    let registry = registry_of(std::slice::from_ref(&tool));
    let names: BTreeSet<String> = ["bad_tool".to_owned()].into();
    let problems = check_tool(tool.as_ref(), &registry, &names).join("\n");
    for rule in [
        "L1", "L2", "L3", "L4", "L5", "L6", "L7", "L8", "L9", "L11", "L12",
    ] {
        assert!(
            problems.contains(&format!("bad_tool: {rule}:")),
            "{rule} did not fire:\n{problems}"
        );
    }
}

// ---- L10: the schema sent is the schema defined ----------------------------

struct Capture {
    bodies: Mutex<Vec<Value>>,
}

#[async_trait]
impl TrustedTransport for Capture {
    fn identity(&self) -> ConnectionIdentity {
        ConnectionIdentity {
            provider_id: "provider".into(),
            connection_id: "account".into(),
            protocol: "chat-completions".into(),
            generation: 1,
        }
    }

    fn auth_kind(&self) -> AuthKind {
        AuthKind::ApiKey
    }

    async fn request(
        &self,
        _: &ConnectionIdentity,
        _: &str,
        body: Option<Value>,
        _: CancellationToken,
    ) -> Result<reqwest::Response, CoreError> {
        if let Some(body) = body {
            self.bodies.lock().expect("bodies").push(body);
        }
        Ok(http::Response::builder()
            .status(200)
            .body(String::new())
            .expect("response")
            .into())
    }
}

#[tokio::test]
async fn schema_sent_equals_schema_defined_on_every_provider_path() {
    let tools = all_tools().await;
    let definitions: Vec<ToolDefinition> =
        tools.iter().map(|tool| tool.definition().clone()).collect();
    let request = ChatRequest {
        model: "model".into(),
        tools: definitions.clone(),
        ..Default::default()
    };

    // Chat Completions: the parameters of each function are the schema.
    let capture = Arc::new(Capture {
        bodies: Mutex::new(vec![]),
    });
    let provider = ChatCompletionsProvider::new(capture.clone()).expect("provider");
    let _events: Vec<_> = provider
        .stream_chat(request.clone(), CancellationToken::new())
        .collect()
        .await;
    let bodies = capture.bodies.lock().expect("bodies").clone();
    let sent = bodies
        .iter()
        .find_map(|body| body["tools"].as_array())
        .expect("a request body with tools");
    assert_eq!(sent.len(), definitions.len());
    for (sent, defined) in sent.iter().zip(&definitions) {
        assert_eq!(sent["function"]["name"], json!(defined.name));
        assert_eq!(
            sent["function"]["description"],
            json!(presented_description(defined, ExampleLane::ChatCompletions)),
            "{}: description sent differs",
            defined.name
        );
        assert_eq!(
            sent["function"]["parameters"], defined.input_schema,
            "{}: chat completions schema sent differs from the definition",
            defined.name
        );
    }

    // Responses: no tool is dropped and no schema is rewritten.
    let identity = ConnectionIdentity {
        provider_id: "provider".into(),
        connection_id: "account".into(),
        protocol: "responses".into(),
        generation: 1,
    };
    let (body, warnings) =
        build_responses_request_with_warnings(&request, &identity, false).expect("body");
    assert!(warnings.is_empty(), "tools were dropped: {warnings:?}");
    let sent = body["tools"][0]["tools"].as_array().expect("tools");
    assert_eq!(sent.len(), definitions.len());
    for (sent, defined) in sent.iter().zip(&definitions) {
        assert_eq!(sent["name"], json!(defined.name));
        assert_eq!(
            sent["parameters"], defined.input_schema,
            "{}: responses schema sent differs from the definition",
            defined.name
        );
    }
}

// ---- TU-07: old names keep working for one release -------------------------

#[tokio::test]
async fn every_renamed_parameter_still_maps_and_validates() {
    let tools = all_tools().await;
    let registry = registry_of(&tools);
    // (tool, call with the old names, the same call with the new names)
    let cases = [
        (
            "read_file",
            json!({"path": "a.rs", "offset": 2}),
            json!({"file_path": "a.rs", "offset": 2}),
        ),
        (
            "write_file",
            json!({"path": "a.rs", "content": "x"}),
            json!({"file_path": "a.rs", "content": "x"}),
        ),
        (
            "edit_file",
            json!({"path": "a.rs", "oldText": "a", "newText": "b"}),
            json!({"file_path": "a.rs", "old_string": "a", "new_string": "b"}),
        ),
        (
            "edit_file",
            json!({"filePath": "a.rs", "old_str": "a", "new_str": "b"}),
            json!({"file_path": "a.rs", "old_string": "a", "new_string": "b"}),
        ),
        (
            "list_dir",
            json!({"includeIgnored": true}),
            json!({"include_ignored": true}),
        ),
        (
            "run_command",
            json!({"cmd": "ls", "cwd": "src"}),
            json!({"command": "ls", "directory": "src"}),
        ),
        (
            "run_command",
            json!({"command": "ls", "timeoutSec": 30}),
            json!({"command": "ls", "timeout": 30000}),
        ),
        (
            "run_command",
            json!({"command": "ls", "timeout_sec": 30}),
            json!({"command": "ls", "timeout": 30000}),
        ),
        (
            "run_command",
            json!({"command": "ls", "timeoutMs": 5000}),
            json!({"command": "ls", "timeout": 5000}),
        ),
        (
            "run_command",
            json!({"command": "ls", "timeout_ms": 5000}),
            json!({"command": "ls", "timeout": 5000}),
        ),
        (
            "git_inspect",
            json!({"command": "log", "maxCount": 5}),
            json!({"command": "log", "limit": 5}),
        ),
        (
            "subagent_status",
            json!({"childId": "c1"}),
            json!({"child_id": "c1"}),
        ),
        (
            "subagent_await",
            json!({"childIds": ["c1"], "timeoutMs": 1000}),
            json!({"child_ids": ["c1"], "timeout": 1000}),
        ),
        (
            "subagent_spawn",
            json!({"role": BUILTIN_ROLES[0], "title": "t", "objective": "o", "output_format": "summary", "scope": {"stop_when": "done"}, "exclusiveWorkspace": true}),
            json!({"role": BUILTIN_ROLES[0], "title": "t", "objective": "o", "output_format": "summary", "scope": {"stop_when": "done"}, "exclusive_workspace": true}),
        ),
        (
            "lsp_definition",
            json!({"file": "a.rs", "line": 1, "character": 1}),
            json!({"file_path": "a.rs", "line": 1, "character": 1}),
        ),
        (
            "lsp_references",
            json!({"file": "a.rs", "line": 1, "character": 1, "includeDeclaration": true}),
            json!({"file_path": "a.rs", "line": 1, "character": 1, "include_declaration": true}),
        ),
        (
            "lsp_diagnostics",
            json!({"file": "a.rs"}),
            json!({"file_path": "a.rs"}),
        ),
    ];
    for (tool, old, new) in cases {
        let normalised = registry.normalise(tool, old.clone());
        assert_eq!(normalised.args, new, "{tool}: {old}");
        assert!(
            !normalised.aliases_used.is_empty(),
            "{tool}: no alias recorded for {old}"
        );
        let errors = registry.validate_input(tool, &normalised.args);
        assert!(errors.is_empty(), "{tool}: {errors:?}");
    }
}

/// L7: the parameters that left the schemas are refused, not silently
/// accepted, when a model still sends them.
#[tokio::test]
async fn runtime_owned_parameters_are_not_in_any_schema() {
    let tools = all_tools().await;
    let registry = registry_of(&tools);
    for (tool, call) in [
        (
            "write_file",
            json!({"file_path": "a", "content": "x", "expectedSha256": "0".repeat(64)}),
        ),
        (
            "edit_file",
            json!({"file_path": "a", "old_string": "a", "new_string": "b", "expectedSha256": "0".repeat(64)}),
        ),
        (
            "subagent_spawn",
            json!({"role": "general", "title": "t", "objective": "o", "output_format": "summary", "scope": {"stop_when": "done"}, "spawnKey": "k"}),
        ),
        (
            "subagent_message",
            json!({"child_id": "c", "content": "m", "messageKey": "k"}),
        ),
        (
            "subagent_resume",
            json!({"child_id": "c", "resumeKey": "k"}),
        ),
    ] {
        assert!(
            !registry.validate_input(tool, &call).is_empty(),
            "{tool} accepted {call}"
        );
    }
}

// ---- description standard (T1.8, TU-13) -----------------------------------

/// The standard asks for four parts: what it does, when to use it, when not
/// (and which tool instead), what it returns. Wording rewrites wait for the
/// comparison (X4, T4.3), so the check is loose and report-only: it names
/// the parts a description lacks. It enforces nothing except that the tools
/// listed in `STRUCTURE_COMPLIANT` keep their parts.
fn description_gaps(def: &ToolDefinition, tool_names: &BTreeSet<String>) -> Vec<&'static str> {
    let text = def.description.to_lowercase();
    let mut gaps = Vec::new();
    if !(text.contains("use ") || text.contains("when ")) {
        gaps.push("when to use");
    }
    let names_another_tool = text
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .any(|token| token != def.name && tool_names.contains(token));
    if !(names_another_tool
        || ["instead", "do not", "don't", "not for"]
            .iter()
            .any(|marker| text.contains(marker)))
    {
        gaps.push("when not / which tool instead");
    }
    if !(text.contains("returns") || text.contains("result is")) {
        gaps.push("what it returns");
    }
    gaps
}

/// Tools whose descriptions already carry all three marked parts. They may
/// not regress; the rest are the input list for T4.3.
const STRUCTURE_COMPLIANT: [&str; 13] = [
    "read_file",
    "write_file",
    "find_files",
    "run_tests",
    "subagent_message",
    "subagent_await",
    "advisor_review",
    "lsp_definition",
    "lsp_references",
    "lsp_hover",
    "lsp_symbols",
    "lsp_implementation",
    "lsp_diagnostics",
];

#[tokio::test]
async fn description_structure_is_reported_and_compliant_tools_keep_it() {
    let tools = all_tools().await;
    let names: BTreeSet<String> = tools
        .iter()
        .map(|tool| tool.definition().name.clone())
        .collect();
    let mut regressions = Vec::new();
    for tool in &tools {
        let def = tool.definition();
        let gaps = description_gaps(def, &names);
        if !gaps.is_empty() {
            // Report only: the list for T4.3 (run with --nocapture).
            eprintln!("T1.8 structure gap: {} lacks {gaps:?}", def.name);
        }
        if STRUCTURE_COMPLIANT.contains(&def.name.as_str()) && !gaps.is_empty() {
            regressions.push(format!("{} lacks {gaps:?}", def.name));
        }
    }
    assert!(regressions.is_empty(), "{regressions:#?}");
}

#[test]
fn the_structure_check_flags_a_missing_part() {
    let names: BTreeSet<String> = ["alpha".to_owned(), "beta".to_owned()].into();
    let mut def = ToolDefinition {
        name: "alpha".into(),
        version: "1".into(),
        effect: "read".into(),
        capability: "test".into(),
        description: "Does a thing.".into(),
        input_schema: json!({"type": "object"}),
        output_schema: None,
        timeout_ms: None,
        examples: vec![],
    };
    assert_eq!(description_gaps(&def, &names).len(), 3);
    def.description = "Does a thing. Use it first; for more use beta. Returns text.".into();
    assert!(description_gaps(&def, &names).is_empty());
}

/// Examples reach the model through the description on lanes with no field
/// for them; the schema is never touched (L10). The rendered length is
/// printed per tool; the 900-character limit applies to the authored text.
#[tokio::test]
async fn presented_examples_leave_the_schema_and_authored_text_intact() {
    let tools = all_tools().await;
    for tool in &tools {
        let def = tool.definition();
        for lane in [
            ExampleLane::Anthropic,
            ExampleLane::ChatCompletions,
            ExampleLane::Responses,
        ] {
            let shown = presented_description(def, lane);
            assert!(shown.starts_with(&def.description), "{}", def.name);
            for example in &def.examples {
                assert!(
                    shown.contains(&format!("Example: {}({example})", def.name)),
                    "{} {lane:?} lost an example",
                    def.name
                );
            }
        }
        eprintln!(
            "T1.8 rendered length: {} authored {} rendered {}",
            def.name,
            def.description.chars().count(),
            presented_description(def, ExampleLane::ChatCompletions)
                .chars()
                .count()
        );
    }
}
