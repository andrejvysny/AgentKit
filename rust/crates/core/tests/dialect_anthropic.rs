//! The Anthropic dialect (T3.2, TU-25): built-in text editor and bash tools,
//! the recorded request and response (hand-built from the vendor docs'
//! examples), and calls the canonical tools cannot express.
use agentkit_core::*;
use serde_json::{Value, json};

fn fixture() -> Value {
    serde_json::from_str(include_str!("fixtures/dialect-anthropic-v1.json")).expect("fixture")
}

fn canonical() -> Vec<ToolDefinition> {
    fixture()["canonical"]
        .as_array()
        .expect("array")
        .iter()
        .map(|value| serde_json::from_value(value.clone()).expect("definition"))
        .collect()
}

fn offered_names(definitions: &[ToolDefinition]) -> Vec<String> {
    definitions.iter().map(|d| d.name.clone()).collect()
}

/// What a request builder sends for a built-in tool.
fn vendor_entry(definition: &ToolDefinition) -> Value {
    let kind = builtin_tool_type(definition).expect("built-in");
    json!({ "type": kind, "name": definition.name })
}

#[test]
fn anthropic_is_looked_up_with_its_own_forms() {
    let dialect = lookup_dialect("anthropic", "1").expect("anthropic v1");
    assert_eq!(dialect.id, "anthropic");
    assert_ne!(dialect.wire_names(), common_dialect().wire_names());
    assert!(dialect.knows_wire(ANTHROPIC_TEXT_EDITOR_NAME));
    assert!(dialect.knows_wire(ANTHROPIC_BASH_NAME));
}

#[test]
fn recorded_request_offers_one_text_editor_and_bash_without_descriptions() {
    let rendered = anthropic_dialect().render(&canonical());
    // read, write, edit and list collapse into one editor; run_command is bash;
    // the search tool keeps the common shape.
    assert_eq!(
        offered_names(&rendered),
        ["str_replace_based_edit_tool", "glob", "bash"]
    );
    let builtins: Vec<Value> = rendered
        .iter()
        .filter(|d| builtin_tool_type(d).is_some())
        .map(vendor_entry)
        .collect();
    assert_eq!(
        builtins,
        fixture()["request"]["tools"].as_array().unwrap().clone()
    );
    for definition in rendered.iter().filter(|d| builtin_tool_type(d).is_some()) {
        assert!(definition.description.is_empty(), "{}", definition.name);
        assert!(definition.examples.is_empty(), "{}", definition.name);
    }
    // The editor can write, so it carries the write effect.
    assert_eq!(rendered[0].effect, "write");
    // The search tool is an ordinary tool with a description and schema.
    assert!(builtin_tool_type(&rendered[1]).is_none());
    assert!(!rendered[1].description.is_empty());
}

#[test]
fn render_is_pure() {
    let dialect = anthropic_dialect();
    let canonical = canonical();
    assert_eq!(
        serde_json::to_value(dialect.render(&canonical)).unwrap(),
        serde_json::to_value(dialect.render(&canonical)).unwrap()
    );
}

#[test]
fn recorded_response_calls_translate_to_the_canonical_calls() {
    let dialect = anthropic_dialect();
    for pair in fixture()["response"]["calls"].as_array().expect("calls") {
        let wire = &pair["wire"];
        let translated = dialect
            .translate(wire["name"].as_str().unwrap(), wire["arguments"].clone())
            .unwrap_or_else(|error| panic!("{wire}: {error}"));
        assert_eq!(translated.name, pair["canonical"]["name"], "{wire}");
        assert_eq!(translated.args, pair["canonical"]["arguments"], "{wire}");
        assert!(translated.args_changed, "{wire}");
    }
}

#[test]
fn canonical_calls_are_shown_as_the_built_in_call_the_model_would_make() {
    let dialect = anthropic_dialect();
    for pair in fixture()["response"]["calls"].as_array().expect("calls") {
        let canonical = &pair["canonical"];
        let (name, args) = dialect
            .wire_call(canonical["name"].as_str().unwrap(), &canonical["arguments"])
            .expect("wire call");
        // A directory view is shown with its trailing slash restored.
        assert_eq!(name, pair["wire"]["name"]);
        assert_eq!(args, pair["wire"]["arguments"], "{canonical}");
    }
}

#[test]
fn view_range_becomes_offset_and_limit() {
    let dialect = anthropic_dialect();
    for case in fixture()["response"]["ranges"].as_array().expect("ranges") {
        let call =
            json!({"command": "view", "path": "primes.py", "view_range": case["view_range"]});
        let translated = dialect
            .translate(ANTHROPIC_TEXT_EDITOR_NAME, call)
            .expect("translates");
        assert_eq!(translated.name, "read_file");
        assert_eq!(translated.args, case["canonical"]);
    }
    for range in [
        json!([0, 5]),
        json!([5, 2]),
        json!([1]),
        json!("1-5"),
        json!([1.5, 2]),
    ] {
        let call = json!({"command": "view", "path": "a", "view_range": range});
        let error = dialect
            .translate(ANTHROPIC_TEXT_EDITOR_NAME, call)
            .expect_err("refused");
        assert!(
            error.message.contains("view_range"),
            "{range}: {}",
            error.message
        );
    }
}

#[test]
fn calls_the_canonical_tools_cannot_express_are_refused_with_teaching_text() {
    let dialect = anthropic_dialect();
    for case in fixture()["response"]["refused"]
        .as_array()
        .expect("refused")
    {
        let error = dialect
            .translate(case["name"].as_str().unwrap(), case["arguments"].clone())
            .expect_err("refused");
        assert!(
            error.message.contains(case["says"].as_str().unwrap()),
            "{case}: {}",
            error.message
        );
    }
}

#[test]
fn malformed_calls_are_refused_not_repaired() {
    let dialect = anthropic_dialect();
    let editor = ANTHROPIC_TEXT_EDITOR_NAME;
    for (name, args) in [
        (editor, json!("view")),
        (editor, json!({})),
        (editor, json!({"command": "rename", "path": "a"})),
        (editor, json!({"command": "view"})),
        (editor, json!({"command": "view", "path": ""})),
        (editor, json!({"command": "view", "path": "a", "extra": 1})),
        (
            editor,
            json!({"command": "str_replace", "path": "a", "old_str": "x"}),
        ),
        (editor, json!({"command": "create", "path": "a"})),
        ("bash", json!({})),
        ("bash", json!({"command": ""})),
        ("bash", json!({"command": "ls", "restart": true})),
        ("bash", json!({"command": "ls", "cwd": "/"})),
    ] {
        assert!(
            dialect.translate(name, args.clone()).is_err(),
            "{name} {args}"
        );
    }
}

#[test]
fn the_editor_is_offered_only_when_read_write_and_edit_are_all_present() {
    let without_write: Vec<ToolDefinition> = canonical()
        .into_iter()
        .filter(|definition| definition.name != "write_file")
        .collect();
    let rendered = anthropic_dialect().render(&without_write);
    let names = offered_names(&rendered);
    assert!(
        !names.contains(&ANTHROPIC_TEXT_EDITOR_NAME.to_owned()),
        "{names:?}"
    );
    assert!(names.contains(&"read_file".to_owned()) && names.contains(&"edit".to_owned()));
    assert!(names.contains(&"bash".to_owned()));
    // Without run_command there is no bash tool either.
    let without_run: Vec<ToolDefinition> = canonical()
        .into_iter()
        .filter(|definition| definition.name != "run_command")
        .collect();
    assert!(!offered_names(&anthropic_dialect().render(&without_run)).contains(&"bash".to_owned()));
}

#[test]
fn history_is_shown_as_built_in_calls_and_inexpressible_calls_stay_canonical() {
    let dialect = anthropic_dialect();
    // replace_all and a command with options have no built-in form.
    assert!(dialect
        .wire_call(
            "edit_file",
            &json!({"file_path": "a", "old_string": "x", "new_string": "y", "replace_all": true})
        )
        .is_none());
    assert!(
        dialect
            .wire_call("run_command", &json!({"command": "ls", "timeout": 5000}))
            .is_none()
    );
    assert!(
        dialect
            .wire_call("list_dir", &json!({"include_ignored": true}))
            .is_none()
    );
    // Ranges and the workspace root.
    let (_, view) = dialect
        .wire_call(
            "read_file",
            &json!({"file_path": "a", "offset": 3, "limit": 4}),
        )
        .expect("view");
    assert_eq!(view["view_range"], json!([3, 6]));
    let (_, root) = dialect.wire_call("list_dir", &json!({})).expect("root");
    assert_eq!(root, json!({"command": "view", "path": "."}));
    // Other tools pass through unchanged.
    let (name, args) = dialect
        .wire_call("find_files", &json!({"pattern": "*.rs"}))
        .expect("glob");
    assert_eq!((name.as_str(), args), ("glob", json!({"pattern": "*.rs"})));
}
