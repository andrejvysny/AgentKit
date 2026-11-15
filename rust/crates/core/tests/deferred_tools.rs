//! Deferred tools (roadmap 2.3): classification, the catalog index, the meta
//! tools, and `tool_call` through the call pipeline. CE-08 (governance
//! parity) runs against real guards in `chat/tests/deferred_tools.rs`; CE-09
//! (argument validation) is here.
use agentkit_contracts::{AiSessionPrefix, JsonDeserialize};
use agentkit_core::*;
use async_trait::async_trait;
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

fn definition(name: &str, description: &str, params: &[&str]) -> ToolDefinition {
    let properties: serde_json::Map<String, Value> = params
        .iter()
        .map(|param| ((*param).to_owned(), json!({"type": "string"})))
        .collect();
    serde_json::from_value(json!({
        "name": name, "version": "1", "effect": "read",
        "capability": format!("{}.read", name.split('_').next().unwrap()),
        "description": description,
        "inputSchema": {"type": "object", "properties": properties,
            "required": params, "additionalProperties": false}
    }))
    .unwrap()
}

struct Echo {
    definition: ToolDefinition,
    ran: Arc<Mutex<Vec<String>>>,
    loading: ToolLoading,
}

#[async_trait]
impl Tool for Echo {
    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }
    fn loading(&self) -> ToolLoading {
        self.loading
    }
    async fn execute(&self, context: ToolContext, input: Value) -> Result<ToolResult, CoreError> {
        self.ran
            .lock()
            .unwrap()
            .push(format!("{}:{input}", self.definition.name));
        Ok(ToolResult {
            ok: true,
            data: Some(json!({"ran": self.definition.name})),
            sources: vec![],
            warnings: vec![],
            truncated: false,
            limits: context.limits,
            model_data: None,
            summary: None,
            status: None,
            model_content: None,
        })
    }
}

type Ran = Arc<Mutex<Vec<String>>>;

fn tool(name: &str, description: &str, params: &[&str], ran: &Ran) -> Arc<Echo> {
    Arc::new(Echo {
        definition: definition(name, description, params),
        ran: ran.clone(),
        loading: ToolLoading::Direct,
    })
}

/// Everything not in `direct` is deferred, whatever the budget.
fn classification(direct: &[&str]) -> Classification {
    Classification {
        direct: DirectSet::names(direct.iter().copied()),
        max_tokens: 0,
        max_definitions: 0,
        ..Classification::default()
    }
}

/// A frozen registry the way the chat executor builds one.
fn session(
    tools: Vec<Arc<Echo>>,
    classification: &Classification,
    prior: Option<&AiSessionPrefix>,
) -> (ToolRegistry, ToolSurface) {
    let mut registry = ToolRegistry::new();
    for tool in tools {
        registry.register(tool).unwrap();
    }
    registry.install_meta_tools().unwrap();
    let mut classification = classification.clone();
    classification.deferred_hints = registry.deferred_hints();
    let surface =
        ToolSurface::reconcile_with(prior, &registry.definitions(), Some(&classification));
    registry.freeze_surface(
        surface.manifest.definitions().to_vec(),
        surface.deferred_names(),
    );
    registry.freeze_catalog(&surface.deferred);
    (registry, surface)
}

fn prefix() -> AiSessionPrefix {
    let digest = |seed: char| seed.to_string().repeat(64);
    AiSessionPrefix::from_json(json!({
        "revision": 1, "toolsDigest": digest('a'), "systemDigest": digest('b'),
        "rulesDigest": digest('c'), "memoryCoreDigest": digest('d'),
        "paramsDigest": digest('e'), "frozenAtTask": "t", "cacheKey": "k",
    }))
    .unwrap()
}

fn recorded(surface: &ToolSurface) -> AiSessionPrefix {
    let mut stored = prefix();
    surface.stamp(&mut stored);
    AiSessionPrefix::from_json(serde_json::to_value(&stored).unwrap()).unwrap()
}

fn names(definitions: &[ToolDefinition]) -> Vec<String> {
    definitions.iter().map(|d| d.name.clone()).collect()
}

struct Script {
    calls: Vec<ToolCall>,
    offered: Mutex<Vec<Vec<String>>>,
}

#[async_trait]
impl ChatProvider for Script {
    fn id(&self) -> &str {
        "test"
    }
    fn stream_chat(&self, request: ChatRequest, _: CancellationToken) -> ProviderStream {
        let mut offered = self.offered.lock().unwrap();
        offered.push(names(&request.tools));
        let event = if offered.len() == 1 {
            ProviderEvent::Completed {
                content: String::new(),
                tool_calls: self.calls.clone(),
                finish_reason: "tool_calls".into(),
            }
        } else {
            ProviderEvent::Completed {
                content: "done".into(),
                tool_calls: vec![],
                finish_reason: "stop".into(),
            }
        };
        Box::pin(futures_util::stream::iter([Ok(event)]))
    }
}

struct Outcome {
    offered: Vec<String>,
    events: Vec<Value>,
}

impl Outcome {
    fn of(&self, kind: &str) -> Vec<Value> {
        self.events
            .iter()
            .filter(|event| event["type"] == kind)
            .map(|event| event["data"].clone())
            .collect()
    }
}

async fn run(registry: ToolRegistry, calls: Vec<ToolCall>) -> Outcome {
    let provider = Arc::new(Script {
        calls,
        offered: Mutex::new(vec![]),
    });
    let handle = run_chat(
        RunOptions {
            request: ChatRequest {
                model: "m".into(),
                ..Default::default()
            },
            tool_call_records: true,
            ..Default::default()
        },
        provider.clone(),
        Arc::new(registry),
    );
    let events = handle
        .events
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .map(|event| serde_json::to_value(event).unwrap())
        .collect();
    let offered = provider.offered.lock().unwrap()[0].clone();
    Outcome { offered, events }
}

fn call(id: &str, name: &str, arguments: Value) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        arguments_json: arguments.to_string(),
    }
}

fn via(id: &str, name: &str, arguments: Value) -> ToolCall {
    call(
        id,
        "tool_call",
        json!({"name": name, "arguments": arguments}),
    )
}

fn catalog_tools(ran: &Ran) -> Vec<Arc<Echo>> {
    vec![
        tool("read_file", "Read a file from disk.", &["file_path"], ran),
        tool(
            "calendar_list_events",
            "List calendar events for a day.",
            &["day"],
            ran,
        ),
        tool(
            "calendar_create_event",
            "Create a calendar event.",
            &["title", "day"],
            ran,
        ),
        tool(
            "page_search",
            "Search pages by text. Returns titles.",
            &["query"],
            ran,
        ),
        tool(
            "mcp__notes__list",
            "List notes in the notebook.",
            &["notebook"],
            ran,
        ),
    ]
}

#[test]
fn a_manifest_within_the_budget_is_all_direct_and_omits_the_meta_tools() {
    let ran = Ran::default();
    let (_, surface) = session(catalog_tools(&ran), &Classification::default(), None);
    assert_eq!(surface.manifest.definitions().len(), 5);
    assert!(surface.deferred.is_empty());
    assert!(!surface.manifest.contains("tool_call"));
    assert!(surface.index().is_none());
}

#[test]
fn a_direct_set_splits_the_manifest_and_the_meta_tools_join_the_array() {
    let ran = Ran::default();
    let (registry, surface) = session(
        catalog_tools(&ran),
        &classification(&["read_file", "page_search"]),
        None,
    );
    assert_eq!(
        names(surface.manifest.definitions()),
        [
            "page_search",
            "read_file",
            "tool_call",
            "tool_describe",
            "tool_search",
            "toolset_load"
        ]
    );
    assert_eq!(
        names(&surface.deferred),
        [
            "calendar_create_event",
            "calendar_list_events",
            "mcp__notes__list"
        ]
    );
    assert_eq!(
        names(&registry.definitions()),
        names(surface.manifest.definitions())
    );
    assert!(registry.meta_active());
}

#[test]
fn a_tool_flag_defers_when_no_direct_set_is_given() {
    let ran = Ran::default();
    let mut tools = catalog_tools(&ran);
    tools.push(Arc::new(Echo {
        definition: definition("heavy_tool", "A rarely used tool.", &["x"]),
        ran: ran.clone(),
        loading: ToolLoading::Deferred,
    }));
    let classification = Classification {
        max_tokens: 0,
        max_definitions: 0,
        ..Classification::default()
    };
    let (_, surface) = session(tools, &classification, None);
    assert_eq!(names(&surface.deferred), ["heavy_tool"]);
    assert!(surface.manifest.contains("read_file"));
}

#[test]
fn the_catalog_index_is_sorted_compact_and_unchanged_by_a_late_tool() {
    let ran = Ran::default();
    let direct = classification(&["read_file"]);
    let (_, first) = session(catalog_tools(&ran), &direct, None);
    let index = first.index().expect("an index");
    let lines: Vec<&str> = index.lines().collect();
    assert!(lines[0].contains("tool_search") && lines[0].contains("tool_call"));
    assert_eq!(
        &lines[1..],
        [
            "- calendar (2): Create a calendar event.",
            "- mcp__notes (1): List notes in the notebook.",
            "- page (1): Search pages by text.",
        ]
    );
    // Same catalog, same bytes, whatever order the candidates arrive in.
    let mut reversed = catalog_tools(&ran);
    reversed.reverse();
    let (_, again) = session(reversed, &direct, None);
    assert_eq!(again.index(), Some(index.clone()));

    // A late contributor joins the catalog; the index the session froze
    // with is what later turns keep sending (the host stores it in S2).
    let stored = recorded(&first);
    let mut late = catalog_tools(&ran);
    late.push(tool("late_tool", "Arrives late.", &["x"], &ran));
    let (_, next) = session(late, &direct, Some(&stored));
    assert_eq!(next.added, ["late_tool"]);
    assert_eq!(
        names(next.manifest.definitions()),
        names(first.manifest.definitions())
    );
    assert!(next.deferred_names().contains(&"late_tool".to_owned()));
}

#[tokio::test]
async fn tool_search_ranks_name_over_parameters_over_description() {
    let ran = Ran::default();
    let tools = vec![
        tool("alpha_tool", "Does something with a note.", &["x"], &ran),
        tool("beta_tool", "Unrelated.", &["note"], &ran),
        tool("note_tool", "Unrelated.", &["x"], &ran),
        tool("gamma_tool", "Unrelated.", &["x"], &ran),
        tool("keep", "Direct.", &["x"], &ran),
    ];
    let (registry, _) = session(tools, &classification(&["keep"]), None);
    let found = run(
        registry,
        vec![call("s1", "tool_search", json!({"query": "note"}))],
    )
    .await;
    assert!(found.offered.contains(&"tool_search".to_owned()));
    let result = &found.of("run.tool.succeeded")[0];
    let model: Value = serde_json::from_str(result["modelResultJson"].as_str().unwrap()).unwrap();
    let data = &model["data"];
    let order: Vec<&str> = data["matches"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["name"].as_str().unwrap())
        .collect();
    assert_eq!(order, ["note_tool", "beta_tool", "alpha_tool"]);
    assert_eq!(data["total_matched"], 3);
    let first = &data["matches"][0];
    assert_eq!(first["effect"], "read");
    assert_eq!(first["params"], json!(["x*"]));
    assert!(first.get("input_schema").is_none(), "entries are compact");
}

#[test]
fn search_limit_namespace_and_ties() {
    let ran = Ran::default();
    let (registry, _) = session(catalog_tools(&ran), &classification(&[]), None);
    // The catalog is what the meta tools read.
    let (all, total) = registry
        .catalog()
        .unwrap()
        .search("calendar event", None, 8);
    assert_eq!(total, 2);
    assert_eq!(
        names_of(&all),
        ["calendar_create_event", "calendar_list_events"]
    );
    let (one, total) = registry
        .catalog()
        .unwrap()
        .search("calendar event", None, 1);
    assert_eq!((one.len(), total), (1, 2));
    let (scoped, _) = registry
        .catalog()
        .unwrap()
        .search("list", Some("mcp__notes"), 5);
    assert_eq!(names_of(&scoped), ["mcp__notes__list"]);
    let (none, total) = registry.catalog().unwrap().search("zzzz", None, 5);
    assert!(none.is_empty() && total == 0);
}

fn names_of(entries: &[&CatalogEntry]) -> Vec<String> {
    entries.iter().map(|e| e.definition.name.clone()).collect()
}

#[tokio::test]
async fn tool_describe_returns_the_full_definition_or_the_closest_names() {
    let ran = Ran::default();
    let (registry, _) = session(catalog_tools(&ran), &classification(&["read_file"]), None);
    let outcome = run(
        registry,
        vec![
            call(
                "d1",
                "tool_describe",
                json!({"name": "calendar_list_events"}),
            ),
            call(
                "d2",
                "tool_describe",
                json!({"name": "calendar_list_event"}),
            ),
        ],
    )
    .await;
    let ok = &outcome.of("run.tool.succeeded")[0];
    let model: Value = serde_json::from_str(ok["modelResultJson"].as_str().unwrap()).unwrap();
    assert_eq!(model["data"]["name"], "calendar_list_events");
    assert_eq!(model["data"]["loading"], "deferred");
    assert_eq!(model["data"]["input_schema"]["required"], json!(["day"]));
    let failed = &outcome.of("run.tool.failed")[0];
    assert_eq!(failed["errorCode"], "tool_not_found");
    assert!(
        failed["errorMessage"]
            .as_str()
            .unwrap()
            .contains("calendar_list_events")
    );
}

#[tokio::test]
async fn tool_call_runs_a_deferred_tool_through_the_pipeline_as_the_inner_tool() {
    let ran = Ran::default();
    let (registry, _) = session(catalog_tools(&ran), &classification(&["read_file"]), None);
    let outcome = run(
        registry,
        vec![via(
            "c1",
            "calendar_list_events",
            json!({"day": "2026-03-02"}),
        )],
    )
    .await;
    assert_eq!(
        *ran.lock().unwrap(),
        [r#"calendar_list_events:{"day":"2026-03-02"}"#]
    );
    // Every event after the announcement names the inner tool.
    let running = outcome.of("run.tool.running");
    assert_eq!(running[0]["toolName"], "calendar_list_events");
    assert_eq!(running[0]["toolCallId"], "c1");
    let records = outcome.of("run.tool.record");
    assert_eq!(records[0]["toolName"], "calendar_list_events");
    assert_eq!(
        outcome.of("run.tool.succeeded")[0]["toolName"],
        "calendar_list_events"
    );
}

#[tokio::test]
async fn tool_call_normalises_the_inner_arguments_like_a_direct_call() {
    let ran = Ran::default();
    let (registry, _) = session(catalog_tools(&ran), &classification(&["page_search"]), None);
    // `path` is the standard alias of `file_path`.
    let outcome = run(
        registry,
        vec![via("c1", "read_file", json!({"path": "a.rs"}))],
    )
    .await;
    assert_eq!(*ran.lock().unwrap(), [r#"read_file:{"file_path":"a.rs"}"#]);
    assert_eq!(
        outcome.of("run.tool.record")[0]["aliasesUsed"],
        json!(["path->file_path"])
    );
}

#[tokio::test]
async fn ce09_invalid_arguments_return_the_schema_and_run_nothing() {
    let ran = Ran::default();
    let (registry, _) = session(catalog_tools(&ran), &classification(&["read_file"]), None);
    let outcome = run(
        registry,
        vec![via("c1", "calendar_create_event", json!({"title": "x"}))],
    )
    .await;
    assert!(ran.lock().unwrap().is_empty(), "no tool ran");
    assert!(
        outcome.of("run.tool.running").len() == 1,
        "the call is announced, then refused at the pipeline"
    );
    let failed = &outcome.of("run.tool.failed")[0];
    assert_eq!(failed["errorCode"], "invalid_arguments");
    let message = failed["errorMessage"].as_str().unwrap();
    assert!(message.contains("day"), "{message}");
    assert!(
        message.contains("\"required\""),
        "carries the schema: {message}"
    );
    assert!(message.contains("calendar_create_event"), "{message}");
}

#[tokio::test]
async fn tool_call_needs_a_name_and_an_arguments_object() {
    let ran = Ran::default();
    let (registry, _) = session(catalog_tools(&ran), &classification(&["read_file"]), None);
    let outcome = run(
        registry,
        vec![
            call("c1", "tool_call", json!({"arguments": {}})),
            call("c2", "tool_call", json!({"name": "calendar_list_events"})),
            call(
                "c3",
                "tool_call",
                json!({"name": "tool_search", "arguments": {}}),
            ),
        ],
    )
    .await;
    let failures = outcome.of("run.tool.failed");
    assert_eq!(failures.len(), 3);
    assert!(
        failures
            .iter()
            .all(|f| f["errorCode"] == "invalid_arguments")
    );
    assert!(ran.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_direct_call_of_a_deferred_tool_is_refused_with_the_way_to_call_it() {
    let ran = Ran::default();
    let (registry, _) = session(catalog_tools(&ran), &classification(&["read_file"]), None);
    let outcome = run(
        registry,
        vec![call("c1", "calendar_list_events", json!({"day": "d"}))],
    )
    .await;
    assert!(ran.lock().unwrap().is_empty());
    let failed = &outcome.of("run.tool.failed")[0];
    assert_eq!(failed["errorCode"], "tool_deferred");
    assert!(
        failed["errorMessage"]
            .as_str()
            .unwrap()
            .contains("tool_call")
    );
}

#[tokio::test]
async fn an_unknown_name_is_tool_not_found_with_the_closest_names() {
    let ran = Ran::default();
    let (registry, _) = session(catalog_tools(&ran), &classification(&["read_file"]), None);
    let outcome = run(registry, vec![via("c1", "calendar_list_event", json!({}))]).await;
    let failed = &outcome.of("run.tool.failed")[0];
    assert_eq!(failed["errorCode"], "tool_not_found");
    assert!(
        failed["errorMessage"]
            .as_str()
            .unwrap()
            .contains("calendar_list_events")
    );
}

#[tokio::test]
async fn a_late_contributor_tool_is_callable_through_tool_call() {
    let ran = Ran::default();
    let direct = classification(&["read_file"]);
    let (_, first) = session(catalog_tools(&ran), &direct, None);
    let stored = recorded(&first);
    let mut later = catalog_tools(&ran);
    later.push(tool("late_tool", "Arrives late.", &["x"], &ran));
    let (registry, surface) = session(later, &direct, Some(&stored));
    assert_eq!(surface.added, ["late_tool"]);
    let outcome = run(
        registry,
        vec![
            call("c1", "late_tool", json!({"x": "1"})),
            via("c2", "late_tool", json!({"x": "1"})),
        ],
    )
    .await;
    assert_eq!(outcome.offered, names(first.manifest.definitions()));
    assert_eq!(*ran.lock().unwrap(), [r#"late_tool:{"x":"1"}"#]);
    assert_eq!(
        outcome.of("run.tool.failed")[0]["errorCode"],
        "tool_deferred"
    );
}

#[tokio::test]
async fn a_cataloged_tool_nothing_offers_is_tool_unavailable() {
    let ran = Ran::default();
    let direct = classification(&["read_file"]);
    let (_, first) = session(catalog_tools(&ran), &direct, None);
    let stored = recorded(&first);
    let mut fewer = catalog_tools(&ran);
    fewer.retain(|tool| tool.definition.name != "page_search");
    let (registry, _) = session(fewer, &direct, Some(&stored));
    let outcome = run(
        registry,
        vec![via("c1", "page_search", json!({"query": "q"}))],
    )
    .await;
    assert_eq!(
        outcome.of("run.tool.failed")[0]["errorCode"],
        "tool_unavailable"
    );
}

#[tokio::test]
async fn a_withheld_tool_is_refused_through_tool_call_exactly_as_directly() {
    let ran = Ran::default();
    let mut registry = ToolRegistry::new();
    for tool in catalog_tools(&ran) {
        registry.register(tool).unwrap();
    }
    registry.withhold_tool(
        definition("calendar_delete_event", "Delete an event.", &["day"]),
        "plan mode: writes are refused",
    );
    registry.install_meta_tools().unwrap();
    let surface = ToolSurface::reconcile_with(
        None,
        &registry.definitions(),
        Some(&classification(&["read_file"])),
    );
    registry.freeze_surface(
        surface.manifest.definitions().to_vec(),
        surface.deferred_names(),
    );
    registry.freeze_catalog(&surface.deferred);
    let outcome = run(
        registry,
        vec![
            call("direct", "calendar_delete_event", json!({"day": "d"})),
            via("meta", "calendar_delete_event", json!({"day": "d"})),
        ],
    )
    .await;
    let failures = outcome.of("run.tool.failed");
    assert_eq!(failures.len(), 2);
    assert_eq!(failures[0]["errorCode"], failures[1]["errorCode"]);
    assert!(
        failures[1]["errorMessage"]
            .as_str()
            .unwrap()
            .contains("plan mode")
    );
}

#[test]
fn the_meta_tool_definitions_follow_the_schema_rules() {
    let known: BTreeSet<String> = META_TOOLS.iter().map(|n| (*n).to_owned()).collect();
    let mut registry = ToolRegistry::new();
    registry.install_meta_tools().unwrap();
    for definition in meta_definitions() {
        let length = definition.description.chars().count();
        assert!(
            (150..=900).contains(&length),
            "{}: {length}",
            definition.name
        );
        assert!(!definition.examples.is_empty(), "{}", definition.name);
        for example in &definition.examples {
            assert!(
                registry
                    .validate_input(&definition.name, example)
                    .is_empty(),
                "{}: example does not validate",
                definition.name
            );
        }
        let schema = &definition.input_schema;
        assert_eq!(schema["type"], "object");
        assert_eq!(schema["additionalProperties"], false);
        assert!(schema["required"].is_array());
        for key in ["anyOf", "oneOf", "allOf", "not", "enum"] {
            assert!(
                schema.get(key).is_none(),
                "{} root has {key}",
                definition.name
            );
        }
        for (name, property) in schema["properties"].as_object().unwrap() {
            assert!(
                name.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
                "{name} is snake_case"
            );
            if property["type"] == "integer" {
                assert!(property.get("minimum").is_some() && property.get("maximum").is_some());
            }
        }
        for token in definition
            .description
            .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .filter(|t| t.starts_with("tool_"))
        {
            assert!(known.contains(token), "{} names {token}", definition.name);
        }
    }
    // The one open object: it carries another tool's own arguments.
    let call = tool_call_definition();
    assert_eq!(
        call.input_schema["properties"]["arguments"]["additionalProperties"],
        true
    );
}

#[test]
fn unwrap_tool_call_builds_the_inner_call_under_the_same_id() {
    let inner = unwrap_tool_call(&via("c9", "page_search", json!({"query": "q"}))).unwrap();
    assert_eq!(inner.id, "c9");
    assert_eq!(inner.name, "page_search");
    assert_eq!(inner.arguments_json, r#"{"query":"q"}"#);
    assert!(unwrap_tool_call(&call("c1", "page_search", json!({}))).is_none());
    assert!(unwrap_tool_call(&call("c1", "tool_call", json!({"name": "x"}))).is_none());
    // Text that holds the arguments object is read as the object.
    let text = call(
        "c2",
        "tool_call",
        json!({"name": "page_search", "arguments": "{\"query\":\"q\"}"}),
    );
    assert_eq!(
        unwrap_tool_call(&text).unwrap().arguments_json,
        r#"{"query":"q"}"#
    );
}
