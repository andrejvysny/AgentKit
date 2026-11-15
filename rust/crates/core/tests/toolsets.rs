//! Toolsets (native tool use T4.1, TU-27): registry, cards, guides,
//! `toolset_load` and its two load modes.
//!
//! Acceptance (TU-27): loading returns guide and definitions; in indirect
//! mode the tools array is unchanged; in promote mode it changes once, at a
//! turn boundary, and the event is recorded.
use agentkit_core::*;
use async_trait::async_trait;
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::collections::BTreeMap;
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
    toolset: Option<&'static str>,
}

#[async_trait]
impl Tool for Echo {
    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }
    fn toolset(&self) -> Option<&str> {
        self.toolset
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
        toolset: None,
    })
}

const CALENDAR_GUIDE: &str = "---\nversion: 3\n---\n# calendar\nList first with {{tool:calendar_list_events}}, then add with {{tool:calendar_create_event}}. Unknown: {{tool:nothing_here}}.\n";

fn calendar_toolset() -> Toolset {
    Toolset::new("calendar", "Calendar", "List and create calendar events.")
        .with_guide(CALENDAR_GUIDE)
        .with_topic("events", "---\nversion: 1\n---\nEvents have a day and a title.")
        .with_tools(&["calendar_list_events", "calendar_create_event"])
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
        tool("mcp__notes__list", "List notes in the notebook.", &["notebook"], ran),
    ]
}

fn classification(direct: &[&str], mode: LoadMode) -> Classification {
    Classification {
        direct: DirectSet::names(direct.iter().copied()),
        max_tokens: 0,
        max_definitions: 0,
        load_mode: mode,
        ..Classification::default()
    }
}

/// A frozen registry the way the chat executor builds one.
fn session(
    tools: Vec<Arc<Echo>>,
    toolsets: Vec<Toolset>,
    classification: &Classification,
) -> (ToolRegistry, ToolSurface) {
    let mut registry = ToolRegistry::new();
    for set in toolsets {
        registry.register_toolset(set);
    }
    for tool in tools {
        registry.register(tool).unwrap();
    }
    registry
        .install_meta_tools_with(classification.load_mode)
        .unwrap();
    let mut classification = classification.clone();
    classification.deferred_hints = registry.deferred_hints();
    let surface = ToolSurface::reconcile_with(None, &registry.definitions(), Some(&classification));
    registry.freeze_surface(
        surface.manifest.definitions().to_vec(),
        surface.deferred_names(),
    );
    registry.freeze_catalog(&surface.deferred);
    (registry, surface)
}

fn names(definitions: &[ToolDefinition]) -> Vec<String> {
    definitions.iter().map(|d| d.name.clone()).collect()
}

/// Request n answers with `rounds[n - 1]`, then a final text. Records the
/// tool names every request offered.
struct Script {
    rounds: Vec<Vec<ToolCall>>,
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
        let event = match self.rounds.get(offered.len() - 1) {
            Some(calls) => ProviderEvent::Completed {
                content: String::new(),
                tool_calls: calls.clone(),
                finish_reason: "tool_calls".into(),
            },
            None => ProviderEvent::Completed {
                content: "done".into(),
                tool_calls: vec![],
                finish_reason: "stop".into(),
            },
        };
        Box::pin(futures_util::stream::iter([Ok(event)]))
    }
}

struct Outcome {
    offered: Vec<Vec<String>>,
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
    fn warnings(&self, code: &str) -> Vec<Value> {
        self.of("run.warning")
            .into_iter()
            .filter(|data| data["code"] == code)
            .collect()
    }
}

async fn run(registry: ToolRegistry, rounds: Vec<Vec<ToolCall>>) -> Outcome {
    let provider = Arc::new(Script {
        rounds,
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
    let offered = provider.offered.lock().unwrap().clone();
    Outcome { offered, events }
}

fn call(id: &str, name: &str, arguments: Value) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        arguments_json: arguments.to_string(),
    }
}

fn load(id: &str, toolset: &str) -> ToolCall {
    call(id, "toolset_load", json!({"toolset": toolset}))
}

fn via(id: &str, name: &str, arguments: Value) -> ToolCall {
    call(id, "tool_call", json!({"name": name, "arguments": arguments}))
}

fn model(outcome: &Outcome, index: usize) -> Value {
    let ok = &outcome.of("run.tool.succeeded")[index];
    serde_json::from_str(ok["modelResultJson"].as_str().unwrap()).unwrap()
}

#[test]
fn a_guide_file_has_front_matter_and_placeholders_render() {
    let guide = Guide::parse(CALENDAR_GUIDE);
    assert_eq!(guide.version, "3");
    assert!(guide.text.starts_with("# calendar"));
    assert_eq!(
        guide_placeholders(&guide.text),
        ["calendar_list_events", "calendar_create_event", "nothing_here"]
    );
    let shown = BTreeMap::from([
        ("calendar_list_events".to_owned(), "cal_list".to_owned()),
        ("calendar_create_event".to_owned(), "cal_a, cal_b or cal_c".to_owned()),
    ]);
    let text = render_guide(&guide.text, &shown);
    assert!(text.contains("List first with cal_list, then add with cal_a, cal_b or cal_c."));
    assert!(text.contains("Unknown: nothing_here."), "{text}");
    assert!(!text.contains("{{"));
    // No front matter: no version, the whole text is the guide.
    assert_eq!(Guide::parse("plain").version, "");
}

#[test]
fn tools_belong_to_the_toolset_that_lists_them_or_that_they_declare() {
    let ran = Ran::default();
    let mut tools = catalog_tools(&ran);
    tools.push(Arc::new(Echo {
        definition: definition("page_search", "Search pages by text.", &["query"]),
        ran: ran.clone(),
        toolset: Some("calendar"),
    }));
    let (registry, _) = session(
        tools,
        vec![calendar_toolset()],
        &classification(&["read_file"], LoadMode::Indirect),
    );
    let catalog = registry.catalog().expect("catalog");
    let of = |name: &str| catalog.get(name).unwrap().namespace.clone();
    // Listed by the set, declared by the tool, dynamic for a server, and the
    // fallback for the rest.
    assert_eq!(of("calendar_list_events"), "calendar");
    assert_eq!(of("page_search"), "calendar");
    assert_eq!(of("mcp__notes__list"), "mcp__notes");
    assert_eq!(of("read_file"), "read");
    assert!(registry.toolsets().iter().any(|set| set.id == "meta"));
}

#[test]
fn the_index_carries_one_card_per_toolset_and_is_cache_stable() {
    let ran = Ran::default();
    let direct = classification(&["read_file"], LoadMode::Indirect);
    let (registry, surface) = session(catalog_tools(&ran), vec![calendar_toolset()], &direct);
    let all: Vec<ToolDefinition> = surface
        .manifest
        .definitions()
        .iter()
        .chain(&surface.deferred)
        .cloned()
        .collect();
    let view_all = registry.toolset_view(&all, None);
    let index = surface.index_with(&view_all).expect("an index");
    let lines: Vec<&str> = index.lines().collect();
    assert!(lines[0].contains("toolset_load"));
    assert_eq!(
        &lines[1..],
        [
            "- calendar (2): List and create calendar events.",
            "- mcp__notes (1): List notes in the notebook.",
        ]
    );
    // Same toolsets, same bytes, whatever order the tools arrive in.
    let mut reversed = catalog_tools(&ran);
    reversed.reverse();
    let (registry2, surface2) = session(reversed, vec![calendar_toolset()], &direct);
    let all2: Vec<ToolDefinition> = surface2
        .manifest
        .definitions()
        .iter()
        .chain(&surface2.deferred)
        .cloned()
        .collect();
    assert_eq!(
        surface2.index_with(&registry2.toolset_view(&all2, None)),
        Some(index)
    );
}

#[test]
fn a_card_is_one_short_line_and_a_toolset_with_a_guide_is_listed_when_direct() {
    let ran = Ran::default();
    // Meta tools kept although every tool is direct (late contributors).
    let all_direct = Classification {
        keep_meta: true,
        ..Classification::default()
    };
    let (registry, surface) = session(catalog_tools(&ran), vec![calendar_toolset()], &all_direct);
    assert!(surface.deferred.is_empty());
    assert!(surface.manifest.contains("toolset_load"));
    let view = registry.toolset_view(surface.manifest.definitions(), None);
    let index = surface.index_with(&view).expect("guided toolsets are listed");
    assert!(index.contains("- calendar (2): List and create calendar events."));
    assert!(!index.contains("mcp__notes"), "no guide, nothing deferred");
    let card = &calendar_toolset().card;
    assert!(card.len() <= 120 && !card.contains('\n'));
}

#[tokio::test]
async fn toolset_load_returns_the_guide_and_compact_definitions() {
    let ran = Ran::default();
    let (registry, _) = session(
        catalog_tools(&ran),
        vec![calendar_toolset()],
        &classification(&["read_file"], LoadMode::Indirect),
    );
    let outcome = run(
        registry,
        vec![vec![
            load("l1", "calendar"),
            call("l2", "toolset_load", json!({"toolset": "calendar", "topic": "events"})),
            call("l3", "toolset_load", json!({"toolset": "calendar", "topic": "nope"})),
            load("l4", "calendr"),
        ]],
    )
    .await;
    let first = model(&outcome, 0);
    let data = &first["data"];
    assert_eq!(data["toolset"], "calendar");
    assert_eq!(data["mode"], "indirect");
    assert_eq!(data["guide_version"], "3");
    let guide = data["guide"].as_str().unwrap();
    assert!(guide.contains("List first with calendar_list_events, then add with calendar_create_event."));
    assert!(!guide.contains("{{"));
    let tools = data["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 2);
    assert_eq!(tools[0]["name"], "calendar_create_event");
    assert_eq!(tools[0]["params"], json!(["title*", "day*"]));
    assert_eq!(data["topics"], json!(["events"]));
    assert!(first["summary"].as_str().unwrap().contains("tool_call"));
    // A topic returns its own guide.
    assert_eq!(model(&outcome, 1)["data"]["guide"], "Events have a day and a title.");
    // An unknown topic and an unknown toolset fail with what to do next.
    let failed = outcome.of("run.tool.failed");
    assert_eq!(failed[0]["errorCode"], "invalid_arguments");
    assert!(failed[0]["errorMessage"].as_str().unwrap().contains("events"));
    assert_eq!(failed[1]["errorCode"], "tool_not_found");
    assert!(failed[1]["errorMessage"].as_str().unwrap().contains("calendar"));
}

#[tokio::test]
async fn indirect_mode_keeps_the_tools_array_and_runs_loaded_tools_through_tool_call() {
    let ran = Ran::default();
    let (registry, _) = session(
        catalog_tools(&ran),
        vec![calendar_toolset()],
        &classification(&["read_file"], LoadMode::Indirect),
    );
    let outcome = run(
        registry,
        vec![
            vec![load("l1", "calendar")],
            vec![via("c1", "calendar_list_events", json!({"day": "2026-03-02"}))],
        ],
    )
    .await;
    assert_eq!(outcome.offered.len(), 3);
    assert_eq!(outcome.offered[0], outcome.offered[1]);
    assert_eq!(outcome.offered[1], outcome.offered[2]);
    assert!(!outcome.offered[2].contains(&"calendar_list_events".to_owned()));
    assert_eq!(
        *ran.lock().unwrap(),
        [r#"calendar_list_events:{"day":"2026-03-02"}"#]
    );
    assert!(outcome.warnings("toolset_promoted").is_empty());
}

struct Recorder(Mutex<Vec<(String, Vec<String>)>>);

#[async_trait]
impl PromotionObserver for Recorder {
    async fn promoted(
        &self,
        toolset: &str,
        definitions: &[ToolDefinition],
    ) -> Result<Option<u64>, CoreError> {
        let mut seen = self.0.lock().unwrap();
        seen.push((toolset.to_owned(), names(definitions)));
        Ok(Some(seen.len() as u64 + 1))
    }
}

#[tokio::test]
async fn promote_mode_changes_the_array_once_at_a_turn_boundary_and_records_it() {
    let ran = Ran::default();
    let (registry, _) = session(
        catalog_tools(&ran),
        vec![calendar_toolset()],
        &classification(&["read_file"], LoadMode::Promote),
    );
    let recorder = Arc::new(Recorder(Mutex::new(vec![])));
    assert!(registry.set_promotion_observer(recorder.clone()));
    let outcome = run(
        registry,
        vec![
            // Same batch as the load: the tool has not joined yet.
            vec![
                load("l1", "calendar"),
                call("early", "calendar_list_events", json!({"day": "d"})),
            ],
            vec![call("c1", "calendar_list_events", json!({"day": "2026-03-02"}))],
        ],
    )
    .await;
    let before = &outcome.offered[0];
    let after = &outcome.offered[1];
    assert!(!before.contains(&"calendar_list_events".to_owned()));
    assert_eq!(
        *after,
        [
            "calendar_create_event",
            "calendar_list_events",
            "read_file",
            "tool_call",
            "tool_describe",
            "tool_search",
            "toolset_load"
        ]
    );
    assert_eq!(outcome.offered[2], *after, "it changed once, and stays");
    // Never mid-turn: the call in the loading batch was refused.
    let failed = outcome.of("run.tool.failed");
    assert_eq!(failed.len(), 1);
    assert_eq!(failed[0]["errorCode"], "tool_deferred");
    // Called by name at the next turn.
    assert_eq!(
        *ran.lock().unwrap(),
        [r#"calendar_list_events:{"day":"2026-03-02"}"#]
    );
    // One recorded event, and the observer re-stamped the manifest once.
    let promoted = outcome.warnings("toolset_promoted");
    assert_eq!(promoted.len(), 1);
    assert_eq!(promoted[0]["toolset"], "calendar");
    assert_eq!(
        promoted[0]["tools"],
        json!(["calendar_create_event", "calendar_list_events"])
    );
    assert_eq!(promoted[0]["revision"], 2);
    assert_eq!(recorder.0.lock().unwrap().len(), 1);
    let summary = model(&outcome, 0)["summary"].as_str().unwrap().to_owned();
    assert!(summary.contains("next turn"), "{summary}");
}

#[tokio::test]
async fn promoted_tools_are_direct_for_describe_and_flagged_in_search() {
    let ran = Ran::default();
    let (registry, _) = session(
        catalog_tools(&ran),
        vec![calendar_toolset()],
        &classification(&["read_file"], LoadMode::Promote),
    );
    let outcome = run(
        registry,
        vec![
            vec![load("l1", "calendar")],
            vec![
                call("d1", "tool_describe", json!({"name": "calendar_list_events"})),
                call("s1", "tool_search", json!({"query": "calendar events"})),
            ],
        ],
    )
    .await;
    assert_eq!(outcome.warnings("toolset_promoted").len(), 1);
    let described = model(&outcome, 1);
    assert_eq!(described["data"]["loading"], "direct", "{described}");
    assert!(
        described["summary"].as_str().unwrap().contains("in your tool list"),
        "{described}"
    );
    let searched = model(&outcome, 2);
    let matches = searched["data"]["matches"].as_array().expect("matches");
    assert!(
        matches
            .iter()
            .any(|m| m["name"] == "calendar_list_events" && m["in_tool_list"] == true),
        "{searched}"
    );
}

#[tokio::test]
async fn loading_a_toolset_twice_promotes_it_once() {
    let ran = Ran::default();
    let (registry, _) = session(
        catalog_tools(&ran),
        vec![calendar_toolset()],
        &classification(&["read_file"], LoadMode::Promote),
    );
    let outcome = run(
        registry,
        vec![vec![load("l1", "calendar"), load("l2", "calendar")]],
    )
    .await;
    assert_eq!(outcome.warnings("toolset_promoted").len(), 1);
}

#[test]
fn the_profile_sets_direct_tools_the_bound_and_the_load_mode() {
    let mut profile = builtin_profile(QWEN_PROFILE_ID).expect("qwen profile");
    profile.direct_tools = vec!["read_file".into(), "git_inspect".into()];
    profile.max_visible_tools = 2.0;
    let plain = Classification::from_profile(&profile, None);
    assert_eq!(plain.load_mode, LoadMode::Promote);
    assert_eq!(plain.max_definitions, 2);
    assert_eq!(
        plain.direct,
        DirectSet::names(["read_file", "git_inspect"])
    );
    // Under the common dialect `git_inspect` is offered as four tools.
    let dialect = common_dialect();
    let wired = Classification::from_profile(&profile, Some(&dialect));
    let DirectSet::Names(names) = &wired.direct else {
        panic!("names");
    };
    assert!(names.contains("git_status") && names.contains("git_log") && names.contains("read_file"));
    // The default keeps the meta tools only when something is deferred.
    assert!(!wired.keep_meta);
}

#[test]
fn keep_meta_advertises_the_meta_tools_when_every_tool_is_direct() {
    let ran = Ran::default();
    let (_, plain) = session(catalog_tools(&ran), vec![], &Classification::default());
    assert!(!plain.manifest.contains("tool_call"));
    let kept = Classification {
        keep_meta: true,
        ..Classification::default()
    };
    let (registry, surface) = session(catalog_tools(&ran), vec![], &kept);
    assert!(surface.deferred.is_empty());
    for meta in META_TOOLS {
        assert!(surface.manifest.contains(meta), "{meta}");
    }
    assert!(registry.meta_active());
}
