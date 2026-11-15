//! The presentation layer and the common dialect (T3.1, TU-23): render,
//! translate, the recorded request and response fixture, and the call
//! pipeline with a frozen dialect.
use agentkit_contracts::{AiSessionPrefix, JsonDeserialize};
use agentkit_core::*;
use async_trait::async_trait;
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

fn fixture() -> Value {
    serde_json::from_str(include_str!("fixtures/dialect-common-v1.json")).expect("fixture")
}

fn definitions(values: &Value) -> Vec<ToolDefinition> {
    values
        .as_array()
        .expect("array")
        .iter()
        .map(|value| serde_json::from_value(value.clone()).expect("definition"))
        .collect()
}

fn as_values(definitions: &[ToolDefinition]) -> Value {
    serde_json::to_value(definitions).expect("serialize")
}

fn names(definitions: &[ToolDefinition]) -> Vec<String> {
    definitions.iter().map(|d| d.name.clone()).collect()
}

// ---- the recorded request and response -------------------------------------

#[test]
fn recorded_request_is_what_common_v1_renders() {
    let fixture = fixture();
    let canonical = definitions(&fixture["canonical"]);
    let rendered = common_dialect().render(&canonical);
    assert_eq!(as_values(&rendered), fixture["request"]["tools"]);
}

#[test]
fn recorded_response_calls_translate_to_the_canonical_calls() {
    let dialect = common_dialect();
    for pair in fixture()["response"]["calls"].as_array().expect("calls") {
        let wire = &pair["wire"];
        let translated = dialect
            .translate(wire["name"].as_str().unwrap(), wire["arguments"].clone())
            .unwrap_or_else(|error| panic!("{wire}: {error}"));
        assert_eq!(translated.name, pair["canonical"]["name"], "{wire}");
        assert_eq!(translated.args, pair["canonical"]["arguments"], "{wire}");
        // And back: the canonical call is shown as the call the model made.
        let (name, args) = dialect
            .wire_call(
                pair["canonical"]["name"].as_str().unwrap(),
                &pair["canonical"]["arguments"],
            )
            .expect("wire call");
        assert_eq!(name, wire["name"], "{wire}");
        assert_eq!(args, wire["arguments"], "{wire}");
    }
}

// ---- the data -------------------------------------------------------------

#[test]
fn common_v1_table_matches_the_spec_and_has_no_collisions() {
    let dialect = common_dialect();
    assert_eq!((dialect.id.as_str(), dialect.version.as_str()), ("common", "1"));
    let wires = dialect.wire_names();
    let expected = [
        "read_file",
        "write_file",
        "edit",
        "list_directory",
        "glob",
        "grep_search",
        "run_shell_command",
        "git_status",
        "git_diff",
        "git_log",
        "git_show",
        "run_tests",
        "repo_map",
        "submit_plan",
    ];
    assert_eq!(wires, expected);
    let mut unique = wires.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(unique.len(), wires.len(), "two tools share a wire name");
    for wire in &wires {
        assert!(
            wire.bytes().next().is_some_and(|b| b.is_ascii_lowercase())
                && wire.len() <= 64
                && wire
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'),
            "{wire} is not a valid wire name"
        );
    }
    assert_eq!(dialect.canonical_of("edit"), Some("edit_file"));
    assert_eq!(dialect.canonical_of("git_log"), Some("git_inspect"));
    assert_eq!(dialect.canonical_of("edit_file"), None);
    assert_eq!(dialect.wire_names_of("git_inspect").len(), 4);
}

#[test]
fn a_tool_named_like_a_wire_name_is_reported_as_a_collision() {
    let canonical = definitions(&fixture()["canonical"]);
    assert!(common_dialect().collisions(&canonical).is_empty());
    let mut with_foreign = canonical;
    let mut foreign = with_foreign[2].clone();
    foreign.name = "git_diff".into();
    with_foreign.push(foreign);
    assert_eq!(common_dialect().collisions(&with_foreign), ["git_diff"]);
}

#[test]
fn render_is_pure_and_leaves_other_tools_alone() {
    let canonical = definitions(&fixture()["canonical"]);
    let dialect = common_dialect();
    assert_eq!(
        as_values(&dialect.render(&canonical)),
        as_values(&dialect.render(&canonical))
    );
    let rendered = dialect.render(&canonical);
    assert_eq!(
        names(&rendered),
        ["edit", "git_status", "git_diff", "git_log", "git_show", "calendar_list"]
    );
    // A tool the dialect does not know keeps its definition byte for byte.
    assert_eq!(as_values(&rendered[5..]), as_values(&canonical[2..]));
}

#[test]
fn split_git_tools_are_flat_and_hold_no_command() {
    let canonical = definitions(&fixture()["canonical"]);
    let rendered = common_dialect().render(&canonical);
    for definition in &rendered[1..5] {
        let schema = &definition.input_schema;
        assert!(schema["properties"].get("command").is_none(), "{}", definition.name);
        assert_eq!(schema["required"], json!([]), "{}", definition.name);
        assert!(!definition.description.contains("git_inspect"));
    }
    let diff = &rendered[2];
    let keys: Vec<&String> = diff.input_schema["properties"].as_object().unwrap().keys().collect();
    assert_eq!(keys, ["path", "rev", "staged"]);
}

// ---- translate ------------------------------------------------------------

#[test]
fn unknown_names_and_canonical_ids_pass_through() {
    let dialect = common_dialect();
    for name in ["tool_search", "mcp__srv__lookup", "edit_file", "git_inspect"] {
        let call = dialect.translate(name, json!({"a": 1})).unwrap();
        assert_eq!((call.name.as_str(), &call.args), (name, &json!({"a": 1})));
        assert!(!call.args_changed);
    }
}

#[test]
fn a_fixed_argument_is_added_and_a_conflicting_one_is_refused() {
    let dialect = common_dialect();
    let added = dialect.translate("git_diff", json!({"staged": true})).unwrap();
    assert_eq!(added.args, json!({"staged": true, "command": "diff"}));
    assert!(added.args_changed);
    // The model repeating the fixed value is not a conflict.
    let same = dialect.translate("git_diff", json!({"command": "diff"})).unwrap();
    assert!(!same.args_changed);
    let conflict = dialect.translate("git_diff", json!({"command": "log"})).unwrap_err();
    assert!(conflict.message.contains("git_diff") && conflict.message.contains("command"));
    let not_object = dialect.translate("git_status", json!([1])).unwrap_err();
    assert!(not_object.message.contains("JSON object"));
}

#[test]
fn a_call_no_wire_tool_can_express_is_not_rendered_as_one() {
    let dialect = common_dialect();
    assert!(dialect.wire_call("git_inspect", &json!({"command": "stash"})).is_none());
    assert!(dialect.wire_call("git_inspect", &json!("nope")).is_none());
    assert_eq!(
        dialect.wire_call("calendar_list", &json!({"day": "x"})),
        Some(("calendar_list".to_owned(), json!({"day": "x"})))
    );
}

// ---- which dialect a session uses -----------------------------------------

fn prefix(manifest: bool, dialect_version: Option<&str>) -> AiSessionPrefix {
    let digest = |seed: char| seed.to_string().repeat(64);
    let mut value = json!({
        "revision": 1, "toolsDigest": digest('a'), "systemDigest": digest('b'),
        "rulesDigest": digest('c'), "memoryCoreDigest": digest('d'),
        "paramsDigest": digest('e'), "frozenAtTask": "t", "cacheKey": "k",
    });
    if manifest {
        let tool = fixture()["canonical"][2].clone();
        value["toolManifest"] = json!([tool]);
        value["toolManifestHash"] = json!(digest('f'));
    }
    if let Some(version) = dialect_version {
        value["dialectVersion"] = json!(version);
    }
    AiSessionPrefix::from_json(value).unwrap()
}

#[test]
fn a_new_session_uses_common_and_the_profile_selects() {
    assert_eq!(session_dialect(None, None, ToolLane::Responses).unwrap().id, "common");
    assert_eq!(session_dialect(Some(&prefix(false, None)), None, ToolLane::Responses).unwrap().id, "common");
    for (id, expected) in [("generic", "common"), ("anthropic", "anthropic"), ("openai", "openai")] {
        let profile = builtin_profile(id).expect(id);
        let chosen = session_dialect(None, Some(&profile), ToolLane::Responses).unwrap();
        assert_eq!(chosen.id, expected, "{id}");
        if expected == "anthropic" || expected == "openai" {
            continue; // T3.2, T3.3: own forms, see tests/dialect_anthropic.rs and dialect_openai.rs
        }
        // The generic profile presents the common forms.
        assert_eq!(chosen.wire_names(), common_dialect().wire_names());
    }
    let mut unknown = builtin_profile("generic").unwrap();
    unknown.dialect.version = "99".into();
    assert_eq!(session_dialect(None, Some(&unknown), ToolLane::Responses).unwrap().id, "common");
}

#[test]
fn the_openai_profile_follows_the_provider_lane() {
    let profile = builtin_profile("openai").expect("openai");
    let on = |lane| session_dialect(None, Some(&profile), lane).unwrap().id;
    assert_eq!(on(ToolLane::Responses), "openai");
    assert_eq!(on(ToolLane::ChatCompletions), "openai-chat");
    // Other profiles do not change with the lane.
    let anthropic = builtin_profile("anthropic").expect("anthropic");
    for lane in [ToolLane::Responses, ToolLane::ChatCompletions] {
        assert_eq!(session_dialect(None, Some(&anthropic), lane).unwrap().id, "anthropic");
    }
    // The Chat Completions lane offers the common coding names, no built-ins.
    let chat = session_dialect(None, Some(&profile), ToolLane::ChatCompletions).unwrap();
    assert_eq!(chat.wire_names(), common_dialect().wire_names());
    assert_eq!(ToolLane::default(), ToolLane::ChatCompletions);
}

#[test]
fn a_session_frozen_before_dialects_keeps_canonical_names() {
    assert!(session_dialect(Some(&prefix(true, None)), None, ToolLane::Responses).is_none());
    assert!(session_dialect(Some(&prefix(true, Some("1"))), None, ToolLane::Responses).is_some());
}

// ---- history shown to the model -------------------------------------------

#[test]
fn stored_history_is_shown_in_the_names_the_model_was_offered() {
    let mut assistant = ChatMessage::text("assistant", "");
    assistant.tool_calls = vec![
        ToolCall {
            id: "1".into(),
            name: "git_inspect".into(),
            arguments_json: r#"{"command":"diff","staged":true}"#.into(),
        },
        ToolCall {
            id: "2".into(),
            name: "edit_file".into(),
            arguments_json: "not json".into(),
        },
        ToolCall {
            id: "3".into(),
            name: "calendar_list".into(),
            arguments_json: "{}".into(),
        },
    ];
    let mut first = ChatMessage::text("tool", "ok");
    first.tool_call_id = Some("1".into());
    first.name = Some("git_inspect".into());
    let mut second = ChatMessage::text("tool", "no");
    second.tool_call_id = Some("2".into());
    second.name = Some("edit_file".into());
    let shown = present_history(
        &common_dialect(),
        vec![ChatMessage::text("user", "hi"), assistant, first, second],
    );
    let calls = &shown[1].tool_calls;
    assert_eq!(calls[0].name, "git_diff");
    assert_eq!(calls[0].arguments_json, r#"{"staged":true}"#);
    // Arguments that are not JSON cannot be shown through a wire tool.
    assert_eq!(calls[1].name, "edit_file");
    assert_eq!(calls[1].arguments_json, "not json");
    assert_eq!(calls[2].name, "calendar_list");
    assert_eq!(shown[2].name.as_deref(), Some("git_diff"));
    assert_eq!(shown[3].name.as_deref(), Some("edit"));
}

#[test]
fn history_shows_the_calls_as_the_model_emitted_them_when_they_were_recorded() {
    // The stored call is canonical; the emitted one rides in the metadata.
    let emitted = ToolCall {
        id: "1".into(),
        name: "git_diff".into(),
        arguments_json: r#"{ "staged": true }"#.into(),
    };
    let mut assistant = ChatMessage::text("assistant", "");
    assistant.tool_calls = vec![ToolCall {
        id: "1".into(),
        name: "git_inspect".into(),
        arguments_json: r#"{"command":"diff","staged":true}"#.into(),
    }];
    let mut metadata = serde_json::Map::new();
    metadata.insert(
        WIRE_CALLS_METADATA_KEY.into(),
        serde_json::to_value(vec![emitted.clone()]).unwrap(),
    );
    assistant.metadata = Some(metadata);
    let mut result = ChatMessage::text("tool", "ok");
    result.tool_call_id = Some("1".into());
    result.name = Some("git_inspect".into());
    for shown in [
        present_history(&common_dialect(), vec![assistant.clone(), result.clone()]),
        // A session without a dialect shows the emitted call too.
        restore_wire_calls(vec![assistant.clone(), result.clone()]),
    ] {
        // Same bytes, whitespace included: a provider continuation hashes them.
        assert_eq!(shown[0].tool_calls, vec![emitted.clone()]);
        assert_eq!(shown[1].name.as_deref(), Some("git_diff"));
    }
    // A record that does not pair with the stored calls is ignored.
    let mut unpaired = assistant;
    unpaired.tool_calls[0].id = "other".into();
    let shown = present_history(&common_dialect(), vec![unpaired]);
    assert_eq!(shown[0].tool_calls[0].name, "git_diff");
    assert_eq!(shown[0].tool_calls[0].arguments_json, r#"{"staged":true}"#);
}

// ---- the call pipeline with a frozen dialect --------------------------------

struct Script {
    calls: Vec<ToolCall>,
    requests: Mutex<Vec<ChatRequest>>,
}
#[async_trait]
impl ChatProvider for Script {
    fn id(&self) -> &str {
        "test"
    }
    fn stream_chat(&self, request: ChatRequest, _: CancellationToken) -> ProviderStream {
        let mut seen = self.requests.lock().unwrap();
        seen.push(request);
        let event = if seen.len() == 1 {
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

struct Recorder {
    definition: ToolDefinition,
    received: Arc<Mutex<Vec<(String, Value)>>>,
}
#[async_trait]
impl Tool for Recorder {
    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }
    async fn execute(&self, context: ToolContext, input: Value) -> Result<ToolResult, CoreError> {
        self.received
            .lock()
            .unwrap()
            .push((self.definition.name.clone(), input));
        Ok(ToolResult {
            ok: true,
            data: Some(json!({"ok": true})),
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

fn frozen_registry(received: &Arc<Mutex<Vec<(String, Value)>>>) -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    for definition in definitions(&fixture()["canonical"]) {
        registry
            .register(Arc::new(Recorder {
                definition,
                received: received.clone(),
            }))
            .unwrap();
    }
    let rendered = common_dialect().render(&registry.definitions());
    registry.freeze_presented(rendered, vec![], common_dialect());
    registry
}

fn call(id: &str, name: &str, args: &str) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        arguments_json: args.into(),
    }
}

struct Outcome {
    events: Vec<Value>,
    requests: Vec<ChatRequest>,
}

async fn run(registry: ToolRegistry, calls: Vec<ToolCall>) -> Outcome {
    let provider = Arc::new(Script {
        calls,
        requests: Mutex::new(vec![]),
    });
    let handle = run_chat(
        RunOptions {
            request: ChatRequest {
                model: "m".into(),
                ..Default::default()
            },
            tool_call_records: true,
            max_tool_calls_per_iteration: 8,
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
    let requests = provider.requests.lock().unwrap().clone();
    Outcome { events, requests }
}

fn of_type<'a>(outcome: &'a Outcome, kind: &str) -> Vec<&'a Value> {
    outcome
        .events
        .iter()
        .filter(|event| event["type"] == kind)
        .map(|event| &event["data"])
        .collect()
}

#[tokio::test]
async fn tools_run_under_canonical_ids_and_the_record_shows_both_names() {
    let received = Arc::new(Mutex::new(vec![]));
    let outcome = run(
        frozen_registry(&received),
        vec![
            call("a", "edit", r#"{"file_path":"a.rs","old_string":"x","new_string":"y"}"#),
            call("b", "git_diff", r#"{"staged":true}"#),
            call("c", "calendar_list", r#"{"day":"d"}"#),
        ],
    )
    .await;
    assert_eq!(
        *received.lock().unwrap(),
        vec![
            (
                "edit_file".to_owned(),
                json!({"file_path":"a.rs","old_string":"x","new_string":"y"})
            ),
            ("git_inspect".to_owned(), json!({"staged":true,"command":"diff"})),
            ("calendar_list".to_owned(), json!({"day":"d"})),
        ]
    );
    // Events and results carry the canonical id.
    let running = of_type(&outcome, "run.tool.running");
    assert_eq!(running[0]["toolName"], "edit_file");
    assert_eq!(running[1]["toolName"], "git_inspect");
    let records = of_type(&outcome, "run.tool.record");
    assert_eq!(records.len(), 3);
    assert_eq!(records[0]["toolName"], "edit");
    assert_eq!(records[0]["canonicalTool"], "edit_file");
    assert_eq!(records[1]["toolName"], "git_diff");
    assert_eq!(records[1]["canonicalTool"], "git_inspect");
    assert_eq!(records[2]["toolName"], "calendar_list");
    assert!(records[2].get("canonicalTool").is_none());
    for record in &records {
        assert_eq!(record["dialect"], "common");
        assert_eq!(record["dialectVersion"], "1");
    }
}

#[tokio::test]
async fn the_request_offers_wire_names_and_history_comes_back_in_them() {
    let received = Arc::new(Mutex::new(vec![]));
    let outcome = run(
        frozen_registry(&received),
        vec![
            call("a", "edit", r#"{"file_path":"a.rs","old_string":"x","new_string":"y"}"#),
            call("b", "git_diff", r#"{"staged":true}"#),
        ],
    )
    .await;
    assert_eq!(
        names(&outcome.requests[0].tools),
        ["edit", "git_status", "git_diff", "git_log", "git_show", "calendar_list"]
    );
    let history = &outcome.requests[1].messages;
    let assistant = history.iter().find(|m| m.role == "assistant").unwrap();
    let shown: Vec<(&str, &str)> = assistant
        .tool_calls
        .iter()
        .map(|call| (call.name.as_str(), call.arguments_json.as_str()))
        .collect();
    assert_eq!(shown[0].0, "edit");
    assert_eq!(shown[1], ("git_diff", r#"{"staged":true}"#));
    let results: Vec<&str> = history
        .iter()
        .filter(|m| m.role == "tool")
        .filter_map(|m| m.name.as_deref())
        .collect();
    assert_eq!(results, ["edit", "git_diff"]);
}

#[tokio::test]
async fn a_conflicting_fixed_argument_is_a_failure_the_model_reads() {
    let received = Arc::new(Mutex::new(vec![]));
    let outcome = run(
        frozen_registry(&received),
        vec![call("a", "git_diff", r#"{"command":"log"}"#)],
    )
    .await;
    assert!(received.lock().unwrap().is_empty());
    let records = of_type(&outcome, "run.tool.record");
    assert_eq!(records[0]["failureCode"], "bad_args");
    let result = outcome.requests[1]
        .messages
        .iter()
        .find(|m| m.role == "tool")
        .unwrap();
    assert!(result.content.to_string().contains("git_diff"));
}

#[tokio::test]
async fn failures_name_tools_as_the_model_sees_them() {
    let received = Arc::new(Mutex::new(vec![]));
    let outcome = run(frozen_registry(&received), vec![call("a", "edti", "{}")]).await;
    let text = outcome.requests[1]
        .messages
        .iter()
        .find(|m| m.role == "tool")
        .unwrap()
        .content
        .to_string();
    assert!(text.contains("edit"), "{text}");
    assert!(!text.contains("edit_file") && !text.contains("git_inspect"), "{text}");
}

#[tokio::test]
async fn a_registry_without_a_dialect_is_unchanged() {
    let received = Arc::new(Mutex::new(vec![]));
    let mut registry = ToolRegistry::new();
    for definition in definitions(&fixture()["canonical"]) {
        registry
            .register(Arc::new(Recorder {
                definition,
                received: received.clone(),
            }))
            .unwrap();
    }
    let surface = registry.definitions();
    registry.freeze_surface(surface, vec![]);
    let outcome = run(
        registry,
        vec![call("a", "edit_file", r#"{"file_path":"a","old_string":"x","new_string":"y"}"#)],
    )
    .await;
    assert_eq!(received.lock().unwrap()[0].0, "edit_file");
    let records = of_type(&outcome, "run.tool.record");
    assert!(records[0].get("dialect").is_none());
    assert_eq!(
        names(&outcome.requests[0].tools),
        ["edit_file", "git_inspect", "calendar_list"]
    );
}
